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

/// Max time we'll wait for a killed child to reap after [`Child::start_kill`].
/// A D-state child (pending uninterruptible I/O) can ignore SIGKILL until
/// the kernel releases it; bounding the wait keeps the runtime live at the
/// cost of leaking the occasional zombie.
const POST_KILL_REAP_TIMEOUT: Duration = Duration::from_secs(5);

use tokio::io::AsyncReadExt;
use tokio::process::Command;
use tokio::time::Instant;

#[cfg(target_os = "linux")]
use crate::sandbox::linux_bwrap_argv;
#[cfg(target_os = "macos")]
use crate::sandbox::macos_profile;
use crate::sandbox::SandboxPolicy;

/// Spawn-time options. `time_budget_ms` is the wall-clock cap; on timeout
/// the child is killed and `timed_out: true` is set on the outcome. `env`
/// is **additive** — the spawned process inherits the parent's env plus
/// any overrides. `working_dir` defaults to the parent's cwd when `None`.
#[derive(Debug, Clone)]
pub struct SubprocessSpec {
    pub time_budget_ms: u64,
    pub env: BTreeMap<String, String>,
    pub working_dir: Option<PathBuf>,
}

impl SubprocessSpec {
    pub fn with_budget_ms(time_budget_ms: u64) -> Self {
        Self {
            time_budget_ms,
            env: BTreeMap::new(),
            working_dir: None,
        }
    }
}

/// Outcome of one subprocess invocation. `timed_out: true` means the
/// process exceeded `time_budget_ms` and was killed by the helper; the
/// captured stdout / stderr are whatever the child produced before the
/// kill. `exit_code` is `None` only when the process was killed (timeout
/// or signal) — a clean exit always carries a code.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubprocessOutcome {
    pub exit_code: Option<i32>,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
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
    if let Some(dir) = &spec.working_dir {
        cmd.current_dir(dir);
    }
    for (k, v) in &spec.env {
        cmd.env(k, v);
    }
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = cmd.spawn().map_err(|e| SubprocessError::Spawn {
        program: program.to_string(),
        source: e,
    })?;
    let child_pid = child.id();

    // Drain stdout/stderr concurrently. The pipes are sized small (~64 KB
    // on most platforms) so a slow consumer can block the child; spawning
    // a reader for each side avoids that deadlock.
    let stdout_pipe = child
        .stdout
        .take()
        .expect("piped stdout was requested above");
    let stderr_pipe = child
        .stderr
        .take()
        .expect("piped stderr was requested above");
    let stdout_task = tokio::spawn(read_to_end(stdout_pipe));
    let stderr_task = tokio::spawn(read_to_end(stderr_pipe));

    let timeout = Duration::from_millis(spec.time_budget_ms);
    let wait_result = tokio::time::timeout(timeout, child.wait()).await;

    let (exit_code, timed_out) = match wait_result {
        Ok(Ok(status)) => (status.code(), false),
        Ok(Err(e)) => return Err(SubprocessError::Io(e.to_string())),
        Err(_elapsed) => {
            let _ = child.start_kill();
            // Bound the post-kill reap: a child stuck in D-state (e.g.
            // pending NFS I/O) won't respond to SIGKILL until the kernel
            // releases it, and `wait()` would hang the worker thread
            // forever. 5s is generous for any responsive child;
            // unresponsive ones leak a zombie but free the runtime.
            // The post-kill outcome is logged so operators can
            // distinguish "killed and reaped clean" from "killed but
            // reap timed out → zombie possible" — both surface to the
            // caller as `timed_out: true, exit_code: None`.
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

    let stdout = stdout_task.await.unwrap_or_default();
    let stderr = stderr_task.await.unwrap_or_default();
    let duration_ms = started.elapsed().as_millis() as u64;

    Ok(SubprocessOutcome {
        exit_code,
        stdout,
        stderr,
        duration_ms,
        timed_out,
    })
}

async fn read_to_end<R: AsyncReadExt + Unpin>(mut reader: R) -> Vec<u8> {
    let mut buf = Vec::new();
    let _ = reader.read_to_end(&mut buf).await;
    buf
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
