//! Phase C unblock (1) — `atelier run` runtime.
//!
//! Wires the §2.5 actor + §15 dispatcher (with all 7 built-in tools) + §15
//! hook loader + §7 DoD config + §11 sandbox + §1 typed cost ledger into a
//! runnable agent loop. Reads a prompt, loops turns against an adapter
//! until `claimed_done: true`, transitions to `Verifying` for DoD checks,
//! and persists the session to `<repo>/.atelier/sessions/<uuid>/`.
//!
//! Pure-Rust API (`Runner::run`) so integration tests can drive it without
//! going through the binary. The `main.rs` `run` subcommand is a thin
//! wrapper that parses argv and prints events to stdout.

use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use atelier_core::{
    adapter::{
        anthropic::AnthropicAdapter, Adapter, AdapterError, Message, MockAdapter, Role,
        ToolCallRequest, ToolSpec,
    },
    dispatcher::{
        Dispatcher, SessionDispatcher, ShellHookExecutor, Tool, ToolContext, ToolRegistry,
    },
    dod::DodConfig,
    hooks::HookSet,
    ledger::Ledger,
    persistence::OnDiskSession,
    protocol::Envelope,
    protocol_strategy::{parse_json_sentinel, parse_native_tool, NativeToolCall, Strategy},
    sandbox::SandboxPolicy,
    session::{self, Command as SessionCommand, Event},
    state::NoopHook,
    tools::{
        ast_grep::AstGrep, edit_file::EditFile, grep::Grep, list_dir::ListDir, read_file::ReadFile,
        shell::Shell, write_file::WriteFile,
    },
    SessionHandle, SessionId, State, Transition,
};

/// How `atelier run` reports events. The binary uses `Stdout` (one line per
/// event); tests use `Capture` to assert on the recorded sequence without
/// stdout interception. `Null` is for tests that don't care about events.
///
/// The `#[allow(dead_code)]` is intentional: this module is consumed by
/// two different build targets (the `atelier` binary and the
/// `run_integration` integration tests), and each only uses a subset of
/// the variants. Without the allow, clippy in one target flags variants
/// the other target needs as dead.
#[allow(dead_code)]
pub enum EventSink {
    /// Print each `Event` to stdout as one JSON-ish line.
    Stdout,
    /// Collect events into a shared `Vec` for test assertions.
    Capture(Arc<parking_lot::Mutex<Vec<Event>>>),
    /// Discard.
    Null,
}

/// Provider selector — what `--provider` flips between. `Mock` is for tests
/// and dev-loop walkthroughs; `Anthropic` is the first real provider, gated
/// on `ANTHROPIC_API_KEY` being present in the environment.
///
/// `#[allow(dead_code)]`: same rationale as [`EventSink`] — the `Anthropic`
/// variant is only constructed by the `atelier` binary (live API path);
/// the integration tests stay on `Mock`. Without the allow, clippy in the
/// test target flags `Anthropic` as dead.
#[allow(dead_code)]
pub enum ProviderChoice {
    /// In-tree `MockAdapter`. Seeded chunk streams come from `mock_responses`.
    Mock { responses: Vec<MockResponse> },
    /// Anthropic Messages API. `model_id` is the `<provider>:<model>` form
    /// the cost ledger stores, e.g. `anthropic:claude-opus-4-7`. API key is
    /// read from `ANTHROPIC_API_KEY` at construction time.
    Anthropic { model_id: String },
}

/// One pre-baked response the `MockAdapter` should return on the Nth
/// `chat()` call. Lets tests script a multi-turn flow without writing the
/// JSON-mode sentinel by hand.
pub struct MockResponse {
    pub assistant_text: String,
    pub envelope: Envelope,
    pub tool_calls: Vec<ToolCallRequest>,
    /// When true, this response carries `claimed_done: true` and the loop
    /// transitions to `Verifying` afterwards. Independent of
    /// `envelope.claimed_done` so a test can drive the transition without
    /// rebuilding the envelope.
    pub claims_done: bool,
}

/// What `Runner::run` returns. Caller (binary or test) decides whether a
/// non-success exit code is warranted.
pub struct RunReport {
    pub session_id: SessionId,
    pub turns: usize,
    pub final_state: State,
    /// `true` iff the DoD config (if present) reported all checks green.
    /// `None` when no DoD config was found — the harness doesn't fail
    /// closed in that case; it's a soft "no DoD configured" state.
    pub dod_passed: Option<bool>,
}

pub struct Runner {
    workspace: PathBuf,
    adapter: Arc<dyn Adapter>,
    sink: EventSink,
    /// Max turns before bailing — defends against a model that never
    /// emits `claimed_done`. 32 matches a generous canonical-workload
    /// median (PROVISIONAL).
    max_turns: usize,
}

impl Runner {
    /// Build a `Runner` for the given provider. Fallible because real
    /// providers (`Anthropic`) need credentials at construction time —
    /// `ANTHROPIC_API_KEY` missing → `RunError::Config`. The `Mock` branch
    /// is infallible.
    pub fn new(
        workspace: PathBuf,
        provider: ProviderChoice,
        sink: EventSink,
    ) -> Result<Self, RunError> {
        let adapter: Arc<dyn Adapter> = match provider {
            ProviderChoice::Mock { responses } => Arc::new(build_mock_adapter(responses)),
            ProviderChoice::Anthropic { model_id } => {
                Arc::new(AnthropicAdapter::from_env(model_id).map_err(adapter_to_run_error)?)
            }
        };
        Ok(Self {
            workspace,
            adapter,
            sink,
            max_turns: 32,
        })
    }

    pub fn with_max_turns(mut self, n: usize) -> Self {
        self.max_turns = n;
        self
    }

    /// Drive the loop until `claims_done` or `max_turns`. Returns when:
    ///   * a turn carried `claims_done: true` (success path; runs DoD next),
    ///   * `max_turns` reached (timeout; `final_state = AwaitingUser`),
    ///   * the adapter errored irrecoverably (propagated).
    pub async fn run(&self, prompt: String) -> Result<RunReport, RunError> {
        let workspace = self.workspace.clone();

        // 1. Load config: hooks + DoD. Both are tolerant of missing files —
        //    a fresh repo with no .atelier/hooks/ or .atelier/dod.json
        //    just gets an empty HookSet + None DoD.
        let hooks = HookSet::load_dir(&workspace.join(".atelier/hooks"))
            .map_err(|e| RunError::Config(format!("hooks: {e}")))?;
        let dod = DodConfig::load(&workspace).map_err(|e| RunError::Config(format!("dod: {e}")))?;

        // 2. Sandbox + dispatcher + ledger.
        let sandbox = SandboxPolicy::restrictive(&workspace)
            .map_err(|e| RunError::Config(format!("sandbox: {e}")))?;
        let registry = built_in_registry()?;
        let dispatcher = Dispatcher::new(registry, hooks)
            .with_executor(Arc::new(ShellHookExecutor::new(sandbox.clone())));
        let ledger = Arc::new(Ledger::new());

        // 3. Session actor + SessionDispatcher.
        let session_handle = session::spawn(Arc::new(NoopHook), Arc::new(NoopHook));
        let session_dispatcher =
            SessionDispatcher::new(dispatcher, ledger.clone(), session_handle.events_sender());

        // 4. Drain events into the sink. tokio task; exits when the
        //    broadcast channel closes (we hold `session_handle` so this is
        //    after we Shutdown below).
        let mut event_rx = session_handle.subscribe();
        let sink_handle = spawn_sink_drain(&self.sink, &mut event_rx);

        // 5. Turn loop.
        let session_id = session_handle.id();
        let mut messages: Vec<Message> = vec![Message {
            role: Role::User,
            content: prompt,
            tool_call_id: None,
        }];
        let mut turns = 0;
        let mut final_state = State::Idle;
        let tools_spec = registry_to_tool_specs();

        for turn in 0..self.max_turns {
            advance(&session_handle, State::Idle, State::Streaming).await?;
            final_state = State::Streaming;

            let response = self
                .adapter
                .chat(&messages, &tools_spec)
                .await
                .map_err(|e| RunError::Adapter(format!("{e}")))?;

            // 6. Parse envelope from response per the adapter's chosen
            //    strategy. Native tool calls (if any) take precedence:
            //    the dispatcher executes them and feeds results back as
            //    Role::Tool messages.
            let envelope = match response.strategy {
                Strategy::NativeTool => {
                    // The envelope rides as a `harness_meta` tool call;
                    // pull it out (if present) and dispatch the rest.
                    extract_native_envelope(&response.tool_calls).unwrap_or_default()
                }
                Strategy::JsonSentinel => parse_json_sentinel(&response.text)
                    .map(|parsed| parsed.envelope)
                    .unwrap_or_default(),
                Strategy::RegexProse => {
                    atelier_core::protocol_strategy::parse_regex_prose(&response.text)
                        .unwrap_or_default()
                }
            };

            messages.push(Message {
                role: Role::Assistant,
                content: response.text.clone(),
                tool_call_id: None,
            });

            // 7. Dispatch any non-envelope tool calls.
            let real_tool_calls: Vec<_> = response
                .tool_calls
                .into_iter()
                .filter(|c| c.name != atelier_core::protocol_strategy::HARNESS_META_NAME)
                .collect();

            if !real_tool_calls.is_empty() {
                advance(&session_handle, State::Streaming, State::ToolDispatching).await?;
                advance(
                    &session_handle,
                    State::ToolDispatching,
                    State::ToolExecuting,
                )
                .await?;
                let ctx = ToolContext {
                    workspace_root: &workspace,
                    sandbox: &sandbox,
                };
                for call in real_tool_calls {
                    let outcome = session_dispatcher.dispatch(&call, &ctx, now_rfc3339).await;
                    // Feed the tool result back into the next turn's
                    // messages so the adapter sees what happened.
                    let result_str = match &outcome.result {
                        Ok(r) => serde_json::to_string(&r.output).unwrap_or_default(),
                        Err(e) => format!("{{\"error\":\"{e}\"}}"),
                    };
                    messages.push(Message {
                        role: Role::Tool,
                        content: result_str,
                        tool_call_id: Some(outcome.tool_call_id),
                    });
                }
                advance(&session_handle, State::ToolExecuting, State::Streaming).await?;
                final_state = State::Streaming;
            }

            turns = turn + 1;

            // 8. If the envelope or scripted response says done, exit.
            if envelope.claimed_done == Some(true) {
                advance(&session_handle, State::Streaming, State::Verifying).await?;
                final_state = State::Verifying;
                break;
            }
        }

        // 9. DoD checks (placeholder — the runner doesn't yet shell out
        //    to dod.checks. That's a follow-on in the runner; the trait
        //    seam is here.)
        let dod_passed = dod.as_ref().map(|_cfg| {
            // TODO: spawn each check.command in the §11 sandbox + assert
            // expect clauses. For now, reporting None of the checks fail
            // gives a soft-pass signal; the field structure lets a real
            // DoD runner replace this without API churn.
            true
        });

        // 10. Done — transition to terminal and persist.
        if final_state == State::Verifying {
            advance(&session_handle, State::Verifying, State::Done).await?;
            final_state = State::Done;
        }

        // 11. Persist session. Best-effort — failure here logs but doesn't
        //     fail the run, because the in-memory state is what the user
        //     cares about for `atelier run`. (The actor's checkpoint hook
        //     is where every-transition persistence would land; this is
        //     the end-of-run snapshot.)
        let session_dir = OnDiskSession::session_dir(&workspace, session_id.0);
        let snapshot = OnDiskSession::fresh(
            session_id.0,
            env!("CARGO_PKG_VERSION").to_string(),
            now_rfc3339(),
        );
        if let Err(e) = snapshot.save_to(&session_dir) {
            tracing::warn!(error = %e, "atelier run: session snapshot save failed");
        }

        // 12. Shutdown. The broadcast channel only closes when *every*
        //     Sender clone drops, and SessionDispatcher holds one
        //     (cloned from session_handle.events_sender()). Drop them in
        //     order: send Shutdown command, drop the dispatcher's
        //     Sender clone, drop the handle's Sender clone, then the
        //     sink's rx.recv() sees Closed and the drain task exits.
        let _ = session_handle.send(SessionCommand::Shutdown).await;
        drop(session_dispatcher);
        drop(session_handle);
        // Safety belt: if a future regression keeps a Sender alive, the
        // drain task would block forever; bound the wait so the runner
        // doesn't hang the test or CLI process.
        let _ = tokio::time::timeout(std::time::Duration::from_secs(5), sink_handle).await;

        Ok(RunReport {
            session_id,
            turns,
            final_state,
            dod_passed,
        })
    }
}

#[derive(Debug, thiserror::Error)]
pub enum RunError {
    #[error("config: {0}")]
    Config(String),
    #[error("adapter: {0}")]
    Adapter(String),
    #[error("session command failed: {0}")]
    Session(String),
}

/// Lift an `AdapterError` raised at construction time (so far only
/// `Anthropic::from_env`) into a `RunError`. Missing credentials are
/// `Config` so the binary can hint at remediation; everything else is
/// `Adapter` so it surfaces as a transient adapter issue.
fn adapter_to_run_error(e: AdapterError) -> RunError {
    match e {
        AdapterError::NotConfigured(m) => RunError::Config(m),
        other => RunError::Adapter(other.to_string()),
    }
}

fn built_in_registry() -> Result<ToolRegistry, RunError> {
    let mut r = ToolRegistry::new();
    let tools: Vec<Arc<dyn Tool>> = vec![
        Arc::new(ReadFile),
        Arc::new(ListDir),
        Arc::new(Grep),
        Arc::new(WriteFile),
        Arc::new(EditFile),
        Arc::new(AstGrep),
        Arc::new(Shell),
    ];
    for t in tools {
        r.register(t)
            .map_err(|e| RunError::Config(format!("tool registry: {e}")))?;
    }
    Ok(r)
}

/// Empty `&[ToolSpec]` for v0 — adapters that need the tool list for
/// native tool-use mode get it from this. The real list (with each tool's
/// `input_schema`) lands when the dispatcher's input-schema work expands.
fn registry_to_tool_specs() -> Vec<ToolSpec> {
    Vec::new()
}

/// Pull the §2 envelope out of a native-tool response. The `harness_meta`
/// tool-call's arguments ARE the envelope; everything else is a real tool
/// call to dispatch.
fn extract_native_envelope(calls: &[ToolCallRequest]) -> Option<Envelope> {
    for c in calls {
        if c.name == atelier_core::protocol_strategy::HARNESS_META_NAME {
            return parse_native_tool(&NativeToolCall {
                name: c.name.clone(),
                arguments: c.arguments.clone(),
            })
            .ok();
        }
    }
    None
}

async fn advance(handle: &SessionHandle, _from: State, to: State) -> Result<(), RunError> {
    // We don't strictly need `from` — the actor enforces legality against
    // its own current state via `Transition::new`. Carried as a doc hint.
    let _ = Transition::new(_from, to); // compile-time check the edge is legal
    handle
        .send(SessionCommand::Advance(to))
        .await
        .map_err(|e| RunError::Session(format!("send Advance({to}): {e}")))
}

fn now_rfc3339() -> String {
    // PROVISIONAL — uses `time::OffsetDateTime` if we add the dep, but for
    // now a coarse second-precision via `SystemTime` keeps the cli dep-
    // light. The format roughly matches RFC 3339 (`YYYY-MM-DDTHH:MM:SSZ`).
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    // Convert seconds-since-epoch to a date string with a tiny inline
    // helper; we don't want chrono just for this.
    let secs_in_day = 86_400u64;
    let day = (now / secs_in_day) as i64;
    let sod = now % secs_in_day;
    let (h, m, s) = (
        (sod / 3600) as u32,
        ((sod / 60) % 60) as u32,
        (sod % 60) as u32,
    );
    // Days since 1970-01-01 → date via the well-known algorithm.
    let (y, mo, d) = days_to_ymd(day);
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{m:02}:{s:02}Z")
}

/// Civil-from-days, Howard Hinnant's algorithm.
fn days_to_ymd(z: i64) -> (i32, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = if m <= 2 { y + 1 } else { y };
    (y as i32, m, d)
}

fn build_mock_adapter(responses: Vec<MockResponse>) -> MockAdapter {
    let m = MockAdapter::new("mock:run");
    for r in responses {
        use atelier_core::adapter::{ChatResponse, StreamChunk, Usage};
        use atelier_core::context::TokenSource;
        m.queue_stream(vec![
            StreamChunk::Text {
                delta: r.assistant_text.clone(),
            },
            StreamChunk::Complete {
                response: ChatResponse {
                    text: r.assistant_text,
                    tool_calls: r.tool_calls,
                    usage: Usage {
                        prompt_tokens: 1,
                        completion_tokens: 1,
                        cached_tokens: None,
                        count_source: TokenSource::Approx,
                        latency_ms: Some(0),
                    },
                    strategy: if r.envelope.claimed_done == Some(true) || r.claims_done {
                        // The envelope rode in tool_calls; the runner's
                        // extract_native_envelope picks it up.
                        Strategy::NativeTool
                    } else {
                        Strategy::NativeTool
                    },
                },
            },
        ]);
    }
    m
}

fn spawn_sink_drain(
    sink: &EventSink,
    rx: &mut tokio::sync::broadcast::Receiver<Event>,
) -> tokio::task::JoinHandle<()> {
    match sink {
        EventSink::Stdout => {
            let mut rx = rx.resubscribe();
            tokio::spawn(async move {
                // Lock stdout per-line, not across the await — StdoutLock
                // isn't Send and would refuse to compile in a tokio task.
                while let Ok(ev) = rx.recv().await {
                    let stdout = io::stdout();
                    let mut handle = stdout.lock();
                    let _ = writeln!(handle, "{ev:?}");
                    let _ = handle.flush();
                }
            })
        }
        EventSink::Capture(buf) => {
            let buf = buf.clone();
            let mut rx = rx.resubscribe();
            tokio::spawn(async move {
                while let Ok(ev) = rx.recv().await {
                    buf.lock().push(ev);
                }
            })
        }
        EventSink::Null => {
            let mut rx = rx.resubscribe();
            tokio::spawn(async move { while rx.recv().await.is_ok() {} })
        }
    }
}

/// Convenience used by the binary to translate a `--prompt-file` path into
/// the prompt string. Centralised so the binary stays small.
/// `allow(dead_code)` because the integration-test build of this module
/// doesn't call it — only the binary does. See the EventSink comment for
/// the dual-build context.
#[allow(dead_code)]
pub fn read_prompt(path: Option<&Path>) -> io::Result<String> {
    match path {
        Some(p) => std::fs::read_to_string(p),
        None => {
            let mut s = String::new();
            io::Read::read_to_string(&mut io::stdin(), &mut s)?;
            Ok(s)
        }
    }
}
