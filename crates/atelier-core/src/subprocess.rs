//! Shared subprocess + sandbox + timeout helper.
//!
//! Spec §11 "Implementation":
//!   * macOS — `sandbox-exec` with a generated `.sb` profile per tool call.
//!   * Linux — `bubblewrap` with read-only repo bind mounts, tmpfs `/tmp`,
//!     no network unless `--allow-net`.
//!
//! Spec §15 "Hooks":
//!   * Each hook declares a time budget; **over-budget = warn and continue,
//!     never block.**
//!
//! Both the `shell` built-in tool and the §15 `ShellHookExecutor` need the
//! same primitive: spawn a subprocess inside the §11 sandbox profile with
//! a hard wall-clock cap, capturing stdout / stderr / exit code. This
//! module is that primitive — pulled out as a shared helper so the two
//! consumers don't duplicate the `tokio::process` plumbing.
//!
//! ## Surface
//!
//! * [`run`] spawns whatever `(program, args)` you hand it under a
//!   [`SubprocessSpec`] (env + working dir + time budget) and returns a
//!   [`SubprocessOutcome`] (exit code, stdout, stderr, duration,
//!   `timed_out` flag). **The caller is responsible for sandboxing.**
//!   Bare `run` is the test surface; production code wraps first.
//!
//! * [`sandboxed_argv`] takes the user-intended `(argv, &SandboxPolicy)` and
//!   returns the `(program, wrapped_args)` pair to hand to `run`. This is
//!   where macOS `sandbox-exec -p <profile> --` and Linux
//!   `bwrap <args> -- <argv>` are generated.
//!
//! ## Why the split
//!
//! CI runs Ubuntu + macOS but doesn't install `bubblewrap` by default. Tests
//! that exercise the timeout / pipe-capture / exit-code machinery use bare
//! `run("echo", …)` — no sandbox dependency. A separate `cfg(target_os =
//! "macos")` integration test will exercise the wrapped path against
//! `sandbox-exec`, which is always present on macOS.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

/// Max time we'll wait for a killed child to reap after we signal it.
/// A D-state child (pending uninterruptible I/O) can ignore SIGKILL until
/// the kernel releases it; bounding the wait keeps the runtime live at the
/// cost of leaking the occasional zombie.
const POST_KILL_REAP_TIMEOUT: Duration = Duration::from_secs(5);

/// Default per-pipe byte cap. Spec §11 / §15: a runaway `find /` or `yes`
/// inside the shell tool / a hook must not be able to OOM the parent.
/// Output beyond this cap is discarded (we keep reading so the child's
/// pipe stays unblocked, but we drop the bytes) and the corresponding
/// `…_truncated` flag is set on the outcome.
pub const DEFAULT_OUTPUT_CAP_BYTES: usize = 1 << 20; // 1 MiB

/// Environment variables that pass through to subprocesses by default.
/// Everything else — notably `ANTHROPIC_API_KEY`, `OPENAI_API_KEY`,
/// `AWS_*`, `GITHUB_TOKEN`, `SSH_AUTH_SOCK`, `SSH_AGENT_PID`, keyring
/// helpers — is dropped. Spec §11 + §12: model-controlled tool invocations
/// must not see the harness's credentials. Callers add specific overrides
/// (including their own re-introduction of any secret, if a tool legitimately
/// needs one) via [`SubprocessSpec::env`].
///
/// This is the practical floor that lets compiled tools find their loader
/// (`PATH`), the locale machinery resolve (`LANG`/`LC_*`/`TZ`), and language
/// tools find a home directory (`HOME`). Anything not listed here is
/// considered sensitive-by-default.
pub const ENV_PASSTHROUGH: &[&str] = &[
    "PATH",
    "HOME",
    "USER",
    "LOGNAME",
    "LANG",
    "LC_ALL",
    "LC_CTYPE",
    "LC_MESSAGES",
    "TERM",
    "TZ",
    "TMPDIR",
    "SHELL",
];

use tokio::io::AsyncReadExt;
use tokio::process::Command;
use tokio::time::Instant;

#[cfg(target_os = "linux")]
use crate::sandbox::linux_bwrap_argv;
#[cfg(target_os = "macos")]
use crate::sandbox::macos_profile;
use crate::sandbox::SandboxPolicy;

/// Spawn-time options.
///
/// * `time_budget_ms` — wall-clock cap. On timeout the child's whole
///   process group is killed (Unix) and `timed_out: true` is set on the
///   outcome.
/// * `env` — overrides on top of the [`ENV_PASSTHROUGH`] allowlist; the
///   child does NOT inherit the harness's full env (spec §11 / §12).
/// * `working_dir` — defaults to the parent's cwd when `None`.
/// * `output_cap_bytes` — per-pipe byte cap, defaulted to
///   [`DEFAULT_OUTPUT_CAP_BYTES`]. Set to `usize::MAX` to opt out (the
///   helper still drains the pipe, but won't truncate).
#[derive(Debug, Clone)]
pub struct SubprocessSpec {
    pub time_budget_ms: u64,
    pub env: BTreeMap<String, String>,
    pub working_dir: Option<PathBuf>,
    pub output_cap_bytes: usize,
}

impl SubprocessSpec {
    pub fn with_budget_ms(time_budget_ms: u64) -> Self {
        Self {
            time_budget_ms,
            env: BTreeMap::new(),
            working_dir: None,
            output_cap_bytes: DEFAULT_OUTPUT_CAP_BYTES,
        }
    }
}

/// Outcome of one subprocess invocation. `timed_out: true` means the
/// process exceeded `time_budget_ms` and was killed by the helper; the
/// captured stdout / stderr are whatever the child produced before the
/// kill. `exit_code` is `None` only when the process was killed (timeout
/// or signal) — a clean exit always carries a code.
///
/// `stdout_truncated` / `stderr_truncated` are set when the child wrote
/// more than the spec's `output_cap_bytes` to that pipe; the captured
/// bytes contain the first `cap` bytes of output, the rest were drained
/// and dropped so the child's write side never blocked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubprocessOutcome {
    pub exit_code: Option<i32>,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub stdout_truncated: bool,
    pub stderr_truncated: bool,
    pub duration_ms: u64,
    pub timed_out: bool,
}

impl SubprocessOutcome {
    /// `true` when the process exited 0. Both signal kills and non-zero
    /// exits fail this check; the caller can disambiguate via
    /// `timed_out` + `exit_code`.
    pub fn is_success(&self) -> bool {
        self.exit_code == Some(0)
    }

    /// Stdout as UTF-8, lossy. Cheap convenience for log lines + tests.
    pub fn stdout_str_lossy(&self) -> std::borrow::Cow<'_, str> {
        String::from_utf8_lossy(&self.stdout)
    }

    pub fn stderr_str_lossy(&self) -> std::borrow::Cow<'_, str> {
        String::from_utf8_lossy(&self.stderr)
    }
}

/// Errors before / during spawn. Timeouts are **not** errors — they're a
/// flag on a successful outcome (spec §15 "warn and continue").
#[derive(Debug, thiserror::Error)]
pub enum SubprocessError {
    #[error("failed to spawn {program:?}: {source}")]
    Spawn {
        program: String,
        #[source]
        source: std::io::Error,
    },

    #[error("I/O error during subprocess wait: {0}")]
    Io(String),

    #[error(
        "subprocess sandboxing is not supported on this platform; only macOS (sandbox-exec) and Linux (bwrap) are implemented"
    )]
    UnsupportedPlatform,
}

/// Spawn `(program, args)` under `spec`. Captures stdout / stderr in
/// parallel via background tasks so a child writing a lot to stderr can't
/// deadlock the pipe before stdout drains. Times out at
/// `spec.time_budget_ms` and kills the child without escalating to an
/// error — spec §15 hooks warn-but-never-block, and the `shell` tool's
/// caller may genuinely care about partial output.
pub async fn run(
    program: &str,
    args: &[String],
    spec: &SubprocessSpec,
) -> Result<SubprocessOutcome, SubprocessError> {
    let started = Instant::now();

    let mut cmd = Command::new(program);
    cmd.args(args);

    // ENV SCRUBBING (§11/§12): start from a clean env, then re-introduce
    // only the documented passthrough set, then apply caller overrides.
    // The harness env may hold ANTHROPIC_API_KEY, AWS_*, GITHUB_TOKEN,
    // SSH_AUTH_SOCK — none of which a model-controlled `sh -c` payload
    // should see.
    cmd.env_clear();
    for key in ENV_PASSTHROUGH {
        if let Ok(val) = std::env::var(key) {
            cmd.env(key, val);
        }
    }
    for (k, v) in &spec.env {
        cmd.env(k, v);
    }

    if let Some(dir) = &spec.working_dir {
        cmd.current_dir(dir);
    }
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    // PROCESS GROUP (§11): put the child in its own group (pgid = child
    // pid). On timeout we SIGKILL `-pgid`, reaping any grandchildren
    // `sh -c "long | pipe"` spawned. Without this, only the immediate
    // child dies and the pipe processes orphan to init.
    #[cfg(unix)]
    cmd.process_group(0);

    let mut child = cmd.spawn().map_err(|e| SubprocessError::Spawn {
        program: program.to_string(),
        source: e,
    })?;
    let child_pid = child.id();

    // Drain stdout/stderr concurrently with a per-pipe byte cap. Spawning
    // a reader for each side avoids the small-pipe-buffer deadlock; the
    // cap prevents a runaway `find /` from OOM'ing the parent.
    let stdout_pipe = child
        .stdout
        .take()
        .expect("piped stdout was requested above");
    let stderr_pipe = child
        .stderr
        .take()
        .expect("piped stderr was requested above");
    let cap = spec.output_cap_bytes;
    let stdout_task = tokio::spawn(read_capped(stdout_pipe, cap));
    let stderr_task = tokio::spawn(read_capped(stderr_pipe, cap));

    let timeout = Duration::from_millis(spec.time_budget_ms);
    let wait_result = tokio::time::timeout(timeout, child.wait()).await;

    let (exit_code, timed_out) = match wait_result {
        Ok(Ok(status)) => (status.code(), false),
        Ok(Err(e)) => return Err(SubprocessError::Io(e.to_string())),
        Err(_elapsed) => {
            kill_process_group(&mut child, child_pid);
            // Bound the post-kill reap: a child stuck in D-state (e.g.
            // pending NFS I/O) won't respond to SIGKILL until the kernel
            // releases it. 5s is generous for any responsive child;
            // unresponsive ones leak a zombie but free the runtime.
            match tokio::time::timeout(POST_KILL_REAP_TIMEOUT, child.wait()).await {
                Ok(Ok(_)) => {}
                Ok(Err(e)) => {
                    tracing::warn!(
                        program = %program,
                        pid = ?child_pid,
                        error = %e,
                        "subprocess wait failed after kill"
                    );
                }
                Err(_reap_elapsed) => {
                    tracing::warn!(
                        program = %program,
                        pid = ?child_pid,
                        reap_timeout_ms = POST_KILL_REAP_TIMEOUT.as_millis() as u64,
                        "subprocess did not reap within post-kill timeout; possible zombie"
                    );
                }
            }
            (None, true)
        }
    };

    // Bound the reader-task awaits. After we SIGKILL'd the process
    // group, the pipes SHOULD close immediately — but if a leaked
    // descendant escaped the pgid (theoretical: process group reset
    // before we signaled) and is still holding the write end, the
    // readers would block on EOF forever and wedge the runtime. Discard
    // partial output on elapse — same contract as `timed_out`: the
    // caller knows the run was abnormal.
    let stdout = match tokio::time::timeout(POST_KILL_REAP_TIMEOUT, stdout_task).await {
        Ok(Ok(v)) => v,
        Ok(Err(_join_err)) => (Vec::new(), false),
        Err(_elapsed) => {
            tracing::warn!(
                program = %program,
                pid = ?child_pid,
                "stdout reader did not finish within POST_KILL_REAP_TIMEOUT; \
                 possible leaked descendant holding the pipe"
            );
            (Vec::new(), false)
        }
    };
    let stderr = match tokio::time::timeout(POST_KILL_REAP_TIMEOUT, stderr_task).await {
        Ok(Ok(v)) => v,
        Ok(Err(_join_err)) => (Vec::new(), false),
        Err(_elapsed) => {
            tracing::warn!(
                program = %program,
                pid = ?child_pid,
                "stderr reader did not finish within POST_KILL_REAP_TIMEOUT; \
                 possible leaked descendant holding the pipe"
            );
            (Vec::new(), false)
        }
    };
    let (stdout, stdout_truncated) = stdout;
    let (stderr, stderr_truncated) = stderr;
    let duration_ms = started.elapsed().as_millis() as u64;

    Ok(SubprocessOutcome {
        exit_code,
        stdout,
        stderr,
        stdout_truncated,
        stderr_truncated,
        duration_ms,
        timed_out,
    })
}

/// Read up to `cap` bytes into a buffer; if the child writes more, keep
/// reading (so the pipe never blocks) but drop the overflow and report
/// `truncated: true`. A `cap` of `usize::MAX` effectively disables the
/// cap while still draining cleanly.
async fn read_capped<R>(mut reader: R, cap: usize) -> (Vec<u8>, bool)
where
    R: AsyncReadExt + Unpin,
{
    let mut buf: Vec<u8> = Vec::new();
    let mut sink = [0u8; 8192];
    let mut total: usize = 0;
    let mut truncated = false;
    loop {
        if total >= cap {
            match reader.read(&mut sink).await {
                Ok(0) => break,
                Ok(_) => {
                    truncated = true;
                }
                Err(_) => break,
            }
        } else {
            let want = (cap - total).min(sink.len());
            match reader.read(&mut sink[..want]).await {
                Ok(0) => break,
                Ok(n) => {
                    buf.extend_from_slice(&sink[..n]);
                    total += n;
                }
                Err(_) => break,
            }
        }
    }
    (buf, truncated)
}

/// SIGKILL the child's whole process group on Unix; fall back to the
/// child-only kill on platforms where we didn't put it in a fresh group.
#[cfg(unix)]
fn kill_process_group(child: &mut tokio::process::Child, pid: Option<u32>) {
    if let Some(pid) = pid {
        // The child was spawned with `process_group(0)`, so its pgid
        // equals its pid. Signaling `-pgid` reaches every process in
        // that group, including any grandchildren `sh -c` spawned.
        //
        // SAFETY: `libc::kill` with a process-group target is a
        // documented syscall; the worst a stray signal can do here is
        // be ignored (group already reaped). We never construct an
        // invalid pgid because we only kill groups we created.
        //
        // v57 (L cleanup) — `pid as i32` was an unchecked cast: on
        // hosts with a raised `/proc/sys/kernel/pid_max` (Linux
        // allows up to 2^22; theoretically more on 64-bit), a PID
        // above `i32::MAX` would wrap to negative and `-pgid` would
        // become positive — signalling a *different*,
        // attacker-influenceable process. The wrap is not reachable
        // on any current OS default, but the unchecked cast is wrong
        // on principle.
        match i32::try_from(pid) {
            Ok(pgid) => unsafe {
                libc::kill(-pgid, libc::SIGKILL);
            },
            Err(_) => {
                tracing::warn!(
                    pid,
                    "subprocess: pid does not fit in i32; falling back to per-child kill (process group survives)"
                );
                let _ = child.start_kill();
            }
        }
    } else {
        let _ = child.start_kill();
    }
}

#[cfg(not(unix))]
fn kill_process_group(child: &mut tokio::process::Child, _pid: Option<u32>) {
    let _ = child.start_kill();
}

/// Wrap `(argv, policy)` into the `(program, args)` form `run` accepts.
///
/// * **macOS** — emits `("sandbox-exec", ["-p", <profile>, "--", argv...])`.
/// * **Linux** — emits `("bwrap", linux_bwrap_argv(policy, argv))`.
/// * **Other** — returns [`SubprocessError::UnsupportedPlatform`]. Spec §11
///   explicitly does not target Windows in v1.
pub fn sandboxed_argv(
    argv: &[String],
    policy: &SandboxPolicy,
) -> Result<(String, Vec<String>), SubprocessError> {
    #[cfg(target_os = "macos")]
    {
        let profile = macos_profile(policy);
        let mut wrapped = Vec::with_capacity(argv.len() + 3);
        wrapped.push("-p".to_string());
        wrapped.push(profile);
        wrapped.push("--".to_string());
        wrapped.extend(argv.iter().cloned());
        Ok(("sandbox-exec".to_string(), wrapped))
    }
    #[cfg(target_os = "linux")]
    {
        let cmd_refs: Vec<&str> = argv.iter().map(String::as_str).collect();
        let wrapped = linux_bwrap_argv(policy, &cmd_refs);
        Ok(("bwrap".to_string(), wrapped))
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = (argv, policy);
        Err(SubprocessError::UnsupportedPlatform)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(ms: u64) -> SubprocessSpec {
        SubprocessSpec::with_budget_ms(ms)
    }

    // ---------- bare run: timeout + pipes + exit code ----------

    #[tokio::test]
    async fn run_captures_stdout_and_returns_exit_zero_for_echo() {
        let out = run("echo", &["hello world".to_string()], &spec(5_000))
            .await
            .unwrap();
        assert!(out.is_success(), "echo should succeed");
        assert_eq!(out.exit_code, Some(0));
        assert!(!out.timed_out);
        assert_eq!(out.stdout_str_lossy().trim(), "hello world");
        assert!(out.stderr.is_empty());
    }

    #[tokio::test]
    async fn run_captures_stderr_separately_from_stdout() {
        // `sh -c "echo OUT; echo ERR 1>&2"` is portable on every POSIX
        // shell CI runs on.
        let out = run(
            "sh",
            &["-c".into(), "echo OUT; echo ERR 1>&2".into()],
            &spec(5_000),
        )
        .await
        .unwrap();
        assert!(out.is_success());
        assert_eq!(out.stdout_str_lossy().trim(), "OUT");
        assert_eq!(out.stderr_str_lossy().trim(), "ERR");
    }

    #[tokio::test]
    async fn run_records_nonzero_exit_code() {
        let out = run("sh", &["-c".into(), "exit 7".into()], &spec(5_000))
            .await
            .unwrap();
        assert!(!out.is_success());
        assert_eq!(out.exit_code, Some(7));
        assert!(!out.timed_out);
    }

    #[tokio::test]
    async fn run_kills_child_past_time_budget_and_sets_timed_out() {
        let out = run("sh", &["-c".into(), "sleep 5".into()], &spec(100))
            .await
            .unwrap();
        assert!(out.timed_out, "should time out");
        assert_eq!(out.exit_code, None, "killed children have no exit code");
        // Duration should be near the budget, not the sleep amount —
        // generous upper bound so flaky CI doesn't fail us.
        assert!(
            out.duration_ms < 2_000,
            "duration {} should be near 100ms budget",
            out.duration_ms
        );
    }

    #[tokio::test]
    async fn run_records_duration_within_budget_when_command_returns_fast() {
        let out = run("true", &[], &spec(5_000)).await.unwrap();
        assert!(!out.timed_out);
        assert!(out.duration_ms < 5_000);
    }

    #[tokio::test]
    async fn run_propagates_env_overrides() {
        let mut s = spec(5_000);
        s.env.insert("ATELIER_TEST".into(), "ok-from-helper".into());
        let out = run(
            "sh",
            &["-c".into(), "printf %s \"$ATELIER_TEST\"".into()],
            &s,
        )
        .await
        .unwrap();
        assert_eq!(out.stdout_str_lossy().as_ref(), "ok-from-helper");
    }

    // Env scrubbing: secrets in the parent env do NOT leak into the
    // spawned child unless the caller explicitly re-introduces them.
    // Spec §11/§12: model-controlled tool invocations must not see the
    // harness's credentials.
    #[tokio::test]
    async fn run_does_not_leak_unlisted_env_vars_into_child() {
        // SAFETY: writing to the process env is `unsafe` since the 2024
        // edition because other threads may read concurrently. The test
        // runtime is single-test-scoped via #[tokio::test] but other
        // tests in the binary share the env; we restore the var below.
        let var = "ATELIER_TEST_SECRET_DO_NOT_LEAK";
        // SAFETY: see comment above; the var is unset at end of test.
        unsafe {
            std::env::set_var(var, "shhh-this-is-a-secret");
        }
        let out = run(
            "sh",
            &[
                "-c".into(),
                // Print the var if set, otherwise the literal NUL so the
                // assertion is unambiguous.
                format!("printf %s \"${{{var}:-MISSING}}\""),
            ],
            &spec(5_000),
        )
        .await
        .unwrap();
        // SAFETY: see above.
        unsafe {
            std::env::remove_var(var);
        }
        assert_eq!(
            out.stdout_str_lossy().as_ref(),
            "MISSING",
            "non-passthrough env var leaked into child: {:?}",
            out.stdout_str_lossy()
        );
    }

    #[tokio::test]
    async fn run_passes_path_var_so_child_can_find_binaries() {
        // PATH is on the passthrough list; without it `sh` couldn't
        // resolve `printf` and the test would fail in a confusing way.
        // This is a positive control for env scrubbing — it strips
        // secrets but keeps the floor needed to function.
        let out = run("sh", &["-c".into(), "printf 'ok'".into()], &spec(5_000))
            .await
            .unwrap();
        assert_eq!(out.stdout_str_lossy().as_ref(), "ok");
    }

    // Byte cap: a child that writes more than the cap has its output
    // truncated and the matching flag set. The captured length is bounded
    // by the cap, not the child's actual output.
    #[tokio::test]
    async fn run_truncates_stdout_past_output_cap() {
        let mut s = spec(10_000);
        s.output_cap_bytes = 1024;
        let out = run(
            "sh",
            // Write ~16 KiB to stdout — well over the 1 KiB cap.
            &["-c".into(), "yes hello | head -c 16384".into()],
            &s,
        )
        .await
        .unwrap();
        assert!(out.is_success(), "child should exit 0");
        assert!(
            out.stdout.len() <= 1024,
            "captured {} bytes, cap was 1024",
            out.stdout.len()
        );
        assert!(out.stdout_truncated, "truncated flag should be set");
        assert!(!out.stderr_truncated, "stderr was empty, not truncated");
    }

    #[tokio::test]
    async fn run_does_not_set_truncated_when_under_cap() {
        let mut s = spec(5_000);
        s.output_cap_bytes = 1024;
        let out = run("printf", &["small".into()], &s).await.unwrap();
        assert!(!out.stdout_truncated);
        assert_eq!(out.stdout_str_lossy().as_ref(), "small");
    }

    // Process group: `sh -c "sleep 60 & wait"` spawns a sleep in the
    // background. On timeout we SIGKILL the whole process group, so the
    // sleep dies too. Without `process_group(0)` + `killpg`, only the
    // shell would die and the sleep would orphan to init.
    //
    // This test is Unix-only (process_group is only set on Unix) and
    // measures wall-clock to catch a leaked sleep.
    #[cfg(unix)]
    #[tokio::test]
    async fn run_kills_grandchildren_on_timeout() {
        let start = std::time::Instant::now();
        let out = run(
            "sh",
            // Background a long sleep; the shell waits for it. Without
            // killpg, the shell dies but the sleep keeps running and
            // holds the stdout pipe open, blocking our read_capped task.
            &["-c".into(), "sleep 30 & wait".into()],
            &spec(200),
        )
        .await
        .unwrap();
        // The whole call (including the post-kill reap) should complete
        // well under the 30s the grandchild would have slept. Generous
        // upper bound (POST_KILL_REAP_TIMEOUT is 5s) so flaky CI doesn't
        // fail us. If the grandchild were orphaned, the read_capped
        // tasks would hold the pipes open for the full 30s.
        let elapsed = start.elapsed();
        assert!(out.timed_out, "should time out");
        assert!(
            elapsed < Duration::from_secs(15),
            "elapsed {elapsed:?} suggests grandchild leaked"
        );
    }

    #[tokio::test]
    async fn run_honors_working_dir() {
        let dir = tempfile::TempDir::new().unwrap();
        let mut s = spec(5_000);
        s.working_dir = Some(dir.path().to_path_buf());
        let out = run("pwd", &[], &s).await.unwrap();
        let pwd = out.stdout_str_lossy().trim().to_string();
        // pwd may return a /private-prefixed canonical form on macOS;
        // accept either by canonicalising both sides via std::fs.
        let actual = std::fs::canonicalize(&pwd).unwrap();
        let expected = std::fs::canonicalize(dir.path()).unwrap();
        assert_eq!(actual, expected);
    }

    #[tokio::test]
    async fn run_surfaces_spawn_failure_for_missing_program() {
        let err = run("/definitely/not/a/program-xyzzy", &[], &spec(5_000))
            .await
            .unwrap_err();
        match err {
            SubprocessError::Spawn { program, .. } => {
                assert!(program.contains("xyzzy"));
            }
            other => panic!("expected Spawn, got {other:?}"),
        }
    }

    // ---------- sandboxed_argv shape ----------

    fn policy() -> SandboxPolicy {
        // /tmp is a valid absolute path on every supported platform.
        SandboxPolicy::restrictive("/tmp/repo").unwrap()
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn sandboxed_argv_on_macos_uses_sandbox_exec_dash_p_double_dash() {
        let (program, args) = sandboxed_argv(&["echo".into(), "hi".into()], &policy()).unwrap();
        assert_eq!(program, "sandbox-exec");
        assert_eq!(args[0], "-p");
        assert!(args[1].contains("(version 1)"));
        assert!(args[1].contains("(deny default)"));
        assert_eq!(args[2], "--");
        assert_eq!(args[3], "echo");
        assert_eq!(args[4], "hi");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn sandboxed_argv_on_linux_uses_bwrap_with_command_after_double_dash() {
        let (program, args) = sandboxed_argv(&["echo".into(), "hi".into()], &policy()).unwrap();
        assert_eq!(program, "bwrap");
        let dd = args
            .iter()
            .position(|s| s == "--")
            .expect("-- present in bwrap argv");
        assert_eq!(&args[dd + 1..], &["echo".to_string(), "hi".to_string()]);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn sandboxed_argv_macos_with_net_flips_to_allow_network() {
        let p = policy().with_net();
        let (_, args) = sandboxed_argv(&["echo".into()], &p).unwrap();
        let profile = &args[1];
        assert!(profile.contains("(allow network*)"));
        assert!(!profile.contains("(deny network*)"));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn sandboxed_argv_linux_default_unshares_net() {
        let (_, args) = sandboxed_argv(&["echo".into()], &policy()).unwrap();
        assert!(args.iter().any(|s| s == "--unshare-net"));
    }

    // ---------- outcome helpers ----------

    #[test]
    fn outcome_is_success_only_for_clean_zero_exit() {
        let mut out = SubprocessOutcome {
            exit_code: Some(0),
            stdout: Vec::new(),
            stderr: Vec::new(),
            stdout_truncated: false,
            stderr_truncated: false,
            duration_ms: 1,
            timed_out: false,
        };
        assert!(out.is_success());
        out.exit_code = Some(1);
        assert!(!out.is_success());
        out.exit_code = None;
        assert!(!out.is_success());
        out.exit_code = Some(0);
        out.timed_out = true;
        // is_success looks at exit_code; a child that exited cleanly
        // before the timeout fired is still success. The caller can
        // additionally check `timed_out` if it wants the stricter
        // condition.
        assert!(out.is_success());
    }
}
