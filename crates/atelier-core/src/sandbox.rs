//! §11 sandbox profile generators.
//!
//! Spec §11 "Implementation":
//!   * **macOS:** `sandbox-exec` with generated `.sb` profile per tool call.
//!   * **Linux:** `bubblewrap` with read-only repo bind mounts, tmpfs `/tmp`,
//!     no network unless `--allow-net` is set on the tool manifest.
//!   * **Windows:** not supported in v1. WSL recommended.
//!
//! Spec §11 "Policy":
//!   * Default: repo-scoped FS, no network egress, no writes to `/etc` or
//!     `/usr/local`.
//!   * Out-of-repo reads require approval (per-path policy applies).
//!
//! ## Scope of this module
//!
//! This module is the *profile generator* — given a [`SandboxPolicy`], it
//! returns the bytes / argv that the actual subprocess launcher (`tokio::process`,
//! wired by the tool dispatcher in §15) passes to `sandbox-exec` or `bwrap`.
//! It deliberately does **not** spawn anything; the spawn path will be tested
//! end-to-end as part of the §11 mechanical gate
//! (`curl evil.example` blocked + logged).

use std::path::{Path, PathBuf};

/// Sandbox profile shared between platforms.
///
/// Construct with [`SandboxPolicy::restrictive`] — the deny-all default
/// matches spec §11's posture. Opt in to additional capabilities (network,
/// extra read paths, extra write paths) explicitly. Each opt-in corresponds
/// to a manifest setting or a per-path approval the user has granted via §8.
#[derive(Debug, Clone)]
pub struct SandboxPolicy {
    /// Absolute path to the workspace root. The repo is read-write inside
    /// the sandbox; everything else is restricted.
    repo_root: PathBuf,
    /// When true, the sandbox permits network egress. Spec §11: "no network
    /// unless `--allow-net` is set on the tool manifest."
    allow_net: bool,
    /// Additional absolute paths granted read access. Used for §11 per-path
    /// policy approvals (e.g., user approved reading `~/.cache/foo` for a
    /// specific tool).
    extra_read_paths: Vec<PathBuf>,
    /// Additional absolute paths granted write access. Rare; requires
    /// explicit user approval. Spec §11 forbids writes to `/etc` and
    /// `/usr/local` — those are blocked even if listed here, via
    /// [`SandboxError::ForbiddenWriteTarget`] at profile-build time.
    extra_write_paths: Vec<PathBuf>,
}

impl SandboxPolicy {
    /// Spec-default deny-all policy. Repo is read-write; `/tmp` is read-write
    /// via tmpfs (Linux) or `(allow file-write* (subpath "/tmp"))` (macOS);
    /// everything else is read-only or blocked.
    pub fn restrictive(repo_root: impl Into<PathBuf>) -> Result<Self, SandboxError> {
        let repo_root = repo_root.into();
        if !repo_root.is_absolute() {
            return Err(SandboxError::NonAbsolutePath(repo_root));
        }
        // v57 (L cleanup) — fail-fast on non-printable characters
        // in the repo root. The macOS `.sb` profile embeds the path
        // as a Lisp string; a literal newline or control char would
        // break the profile parser. Repo paths come from us, not the
        // model, so this is just a contract assertion.
        if has_unsafe_path_chars(&repo_root) {
            return Err(SandboxError::UnsafePathCharacters(repo_root));
        }
        Ok(Self {
            repo_root,
            allow_net: false,
            extra_read_paths: Vec::new(),
            extra_write_paths: Vec::new(),
        })
    }

    /// Per spec §11 "no network unless `--allow-net` is set on the tool
    /// manifest" — flip the bit when the tool / MCP server declares it.
    pub fn with_net(mut self) -> Self {
        self.allow_net = true;
        self
    }

    pub fn allow_read(mut self, path: impl Into<PathBuf>) -> Result<Self, SandboxError> {
        let p = path.into();
        if !p.is_absolute() {
            return Err(SandboxError::NonAbsolutePath(p));
        }
        // v59 (MED-sec-3 fix) — same control-char rejection as
        // `restrictive()` so a config-driven allow_read path
        // containing a newline can't break `.sb` profile parsing or
        // smuggle injected forms past the macOS sandbox.
        if has_unsafe_path_chars(&p) {
            return Err(SandboxError::UnsafePathCharacters(p));
        }
        self.extra_read_paths.push(p);
        Ok(self)
    }

    pub fn allow_write(mut self, path: impl Into<PathBuf>) -> Result<Self, SandboxError> {
        let p = path.into();
        if !p.is_absolute() {
            return Err(SandboxError::NonAbsolutePath(p));
        }
        if has_unsafe_path_chars(&p) {
            return Err(SandboxError::UnsafePathCharacters(p));
        }
        if is_forbidden_write_target(&p) {
            return Err(SandboxError::ForbiddenWriteTarget(p));
        }
        self.extra_write_paths.push(p);
        Ok(self)
    }

    pub fn repo_root(&self) -> &Path {
        &self.repo_root
    }

    pub fn allow_net_flag(&self) -> bool {
        self.allow_net
    }
}

/// Profile-generation errors.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum SandboxError {
    #[error("sandbox path must be absolute: {0}")]
    NonAbsolutePath(PathBuf),

    /// Spec §11 forbids writes to `/etc` and `/usr/local` even if a tool
    /// manifest asks for them. Surfaced at profile-build time so the
    /// violation never reaches the kernel.
    #[error("write target {0} is forbidden by §11 policy (/etc, /usr/local)")]
    ForbiddenWriteTarget(PathBuf),

    /// v57 (L cleanup) — `.sb` profile strings are Lisp; embedding a
    /// raw newline / control byte would either break the profile
    /// parser or, worse, comment out the rest of the rules. We
    /// reject up-front so the profile is always well-formed.
    #[error("sandbox path {0} contains a non-printable / control character that cannot be safely embedded in a sandbox profile")]
    UnsafePathCharacters(PathBuf),
}

fn is_forbidden_write_target(p: &Path) -> bool {
    p.starts_with("/etc") || p.starts_with("/usr/local")
}

/// v57 (L cleanup) — reject path strings that embed control bytes /
/// non-printable characters. Used by `SandboxPolicy::restrictive`
/// (and any future `allow_read` / `allow_write` callers that want to
/// reuse it) to keep generated `.sb` profiles well-formed.
fn has_unsafe_path_chars(p: &Path) -> bool {
    p.to_string_lossy()
        .chars()
        .any(|c| (c as u32) < 0x20 || c == '\x7f')
}

/// Per spec §11 paths that any sandboxed subprocess needs read access to in
/// order to function (resolving libraries, locating system files). The repo
/// itself and any per-path approvals are added on top.
const MACOS_SYSTEM_READ_SUBPATHS: &[&str] = &[
    "/usr/lib",
    "/usr/share",
    "/usr/libexec",
    "/usr/bin",
    "/bin",
    "/System/Library",
    "/Library/Frameworks",
    "/private/var/db/dyld",
];

const MACOS_SYSTEM_LITERAL_READS: &[&str] = &["/etc/localtime"];

/// Generate a `sandbox-exec` `.sb` profile string for the given policy.
///
/// The profile starts with `(version 1) (deny default)` — the strictest
/// possible base — and grants exactly the capabilities the policy describes.
/// `process-fork` and `process-exec*` are always granted because every
/// non-trivial tool needs to spawn children; without them `sandbox-exec`
/// can't even invoke the target binary.
pub fn macos_profile(policy: &SandboxPolicy) -> String {
    let mut out = String::new();
    out.push_str("(version 1)\n");
    // Apple's standard baseline. Grants the low-level system access every
    // subprocess needs (dyld loader, mach kernel calls, sysctl reads,
    // common library reads) without enumerating every individual path —
    // those vary by macOS version and have caught us out repeatedly when
    // tightened by hand. The explicit `(deny default)` below restricts
    // everything else.
    out.push_str("(import \"system.sb\")\n");
    out.push_str("(deny default)\n");
    // Always required for any subprocess to function.
    out.push_str("(allow process-fork)\n");
    out.push_str("(allow process-exec*)\n");
    out.push_str("(allow signal (target self))\n");
    out.push_str("(allow sysctl-read)\n");
    out.push_str("(allow mach-lookup)\n");
    out.push_str("(allow file-read-metadata)\n");
    out.push_str("(allow ipc-posix-shm)\n");

    // System read paths — needed for dynamic linker, frameworks, etc.
    // Most of these are already covered by `system.sb`, but listing them
    // explicitly here means a profile-readability viewer doesn't have to
    // resolve the import to know what's allowed.
    for p in MACOS_SYSTEM_READ_SUBPATHS {
        out.push_str(&format!("(allow file-read* (subpath \"{p}\"))\n"));
    }
    for p in MACOS_SYSTEM_LITERAL_READS {
        out.push_str(&format!("(allow file-read* (literal \"{p}\"))\n"));
    }

    // Repo: read + write.
    let repo = sb_escape(policy.repo_root.to_string_lossy().as_ref());
    out.push_str(&format!("(allow file-read* (subpath \"{repo}\"))\n"));
    out.push_str(&format!("(allow file-write* (subpath \"{repo}\"))\n"));

    // /tmp scratch — writable, scoped.
    out.push_str("(allow file-read* (subpath \"/tmp\"))\n");
    out.push_str("(allow file-read* (subpath \"/private/tmp\"))\n");
    out.push_str("(allow file-write* (subpath \"/tmp\"))\n");
    out.push_str("(allow file-write* (subpath \"/private/tmp\"))\n");

    // Per-path approved reads.
    for p in &policy.extra_read_paths {
        let p = sb_escape(p.to_string_lossy().as_ref());
        out.push_str(&format!("(allow file-read* (subpath \"{p}\"))\n"));
    }

    // Per-path approved writes. `is_forbidden_write_target` already filtered
    // /etc and /usr/local at policy-build time.
    for p in &policy.extra_write_paths {
        let p = sb_escape(p.to_string_lossy().as_ref());
        out.push_str(&format!("(allow file-write* (subpath \"{p}\"))\n"));
    }

    // Network — default deny. Defensive belt-and-braces: emit explicit denies
    // so a profile reviewer can see the policy without inferring from the
    // base deny rule.
    if policy.allow_net {
        out.push_str("(allow network*)\n");
    } else {
        out.push_str("(deny network*)\n");
    }

    out
}

/// Escape a path for inclusion in an `.sb` quoted string. sandbox-exec uses
/// Lisp-style string literals; backslashes and double-quotes need escaping.
fn sb_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            other => out.push(other),
        }
    }
    out
}

/// Generate the `bwrap` argv (excluding the leading `bwrap`) for the given
/// policy and the target command. Caller prepends `"bwrap"` and feeds the
/// whole list to `tokio::process::Command`.
///
/// Defaults derived from spec §11: read-only bind mounts for system dirs,
/// tmpfs `/tmp`, repo bound read-write, `--unshare-net` unless `allow_net`.
/// `--die-with-parent` and `--new-session` are always set so a killed parent
/// reaps the child and a TTY breakout via SIGTSTP isn't possible.
pub fn linux_bwrap_argv(policy: &SandboxPolicy, command: &[&str]) -> Vec<String> {
    let mut args: Vec<String> = Vec::new();

    // System directories — read-only. `--ro-bind-try` skips the bind silently
    // when the source path is missing (e.g., `/lib64` on aarch64); the strict
    // `--ro-bind` would fail.
    for src in ["/usr", "/lib", "/lib64", "/bin", "/sbin", "/etc"] {
        args.push("--ro-bind-try".into());
        args.push(src.into());
        args.push(src.into());
    }

    args.push("--proc".into());
    args.push("/proc".into());
    args.push("--dev".into());
    args.push("/dev".into());
    args.push("--tmpfs".into());
    args.push("/tmp".into());

    // Repo: read-write bind mount.
    let repo = policy.repo_root.to_string_lossy().into_owned();
    args.push("--bind".into());
    args.push(repo.clone());
    args.push(repo);

    // Per-path approved reads.
    for p in &policy.extra_read_paths {
        let p = p.to_string_lossy().into_owned();
        args.push("--ro-bind".into());
        args.push(p.clone());
        args.push(p);
    }
    // Per-path approved writes.
    for p in &policy.extra_write_paths {
        let p = p.to_string_lossy().into_owned();
        args.push("--bind".into());
        args.push(p.clone());
        args.push(p);
    }

    // Network — default deny via namespace unshare.
    if !policy.allow_net {
        args.push("--unshare-net".into());
    }

    // Drop other namespaces by default for defence in depth.
    args.push("--unshare-pid".into());
    args.push("--unshare-uts".into());
    args.push("--unshare-ipc".into());
    args.push("--unshare-user-try".into());

    // Always-on hygiene.
    args.push("--die-with-parent".into());
    args.push("--new-session".into());

    // Target command.
    args.push("--".into());
    for piece in command {
        args.push((*piece).to_string());
    }

    args
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy() -> SandboxPolicy {
        SandboxPolicy::restrictive("/repo").unwrap()
    }

    #[test]
    fn restrictive_requires_absolute_repo_root() {
        let err = SandboxPolicy::restrictive("relative/path").unwrap_err();
        assert!(matches!(err, SandboxError::NonAbsolutePath(_)));
    }

    #[test]
    fn restrictive_rejects_repo_root_with_control_characters() {
        // Regression for L cleanup — a repo path with a literal
        // newline would break the macOS `.sb` profile parser.
        let err = SandboxPolicy::restrictive("/tmp/with\nnewline").unwrap_err();
        assert!(matches!(err, SandboxError::UnsafePathCharacters(_)));
    }

    #[test]
    fn allow_read_rejects_relative_paths() {
        let err = policy().allow_read("rel").unwrap_err();
        assert!(matches!(err, SandboxError::NonAbsolutePath(_)));
    }

    #[test]
    fn allow_read_rejects_control_chars_in_path() {
        // Regression for v59 MED-sec-3 — `.sb` profile injection class.
        let err = policy().allow_read("/tmp/with\nnewline").unwrap_err();
        assert!(matches!(err, SandboxError::UnsafePathCharacters(_)));
    }

    #[test]
    fn allow_write_rejects_control_chars_in_path() {
        let err = policy().allow_write("/tmp/with\nnewline").unwrap_err();
        assert!(matches!(err, SandboxError::UnsafePathCharacters(_)));
    }

    #[test]
    fn allow_write_rejects_etc_and_usr_local() {
        for forbidden in ["/etc", "/etc/passwd", "/usr/local", "/usr/local/bin/x"] {
            let err = policy().allow_write(forbidden).unwrap_err();
            assert!(
                matches!(err, SandboxError::ForbiddenWriteTarget(_)),
                "expected ForbiddenWriteTarget for {forbidden}"
            );
        }
    }

    #[test]
    fn allow_write_accepts_other_absolute_paths() {
        policy().allow_write("/var/cache/tool").unwrap();
        policy().allow_write("/Users/me/scratch").unwrap();
    }

    // ---------- macOS profile ----------

    #[test]
    fn macos_profile_starts_with_version_import_and_deny_default() {
        let p = macos_profile(&policy());
        assert!(p.starts_with("(version 1)\n"));
        assert!(p.contains("(import \"system.sb\")"));
        assert!(p.contains("(deny default)"));
        // `(deny default)` must come after the import so the explicit
        // restrictions override the baseline's allows where they overlap.
        let import_pos = p.find("(import").unwrap();
        let deny_pos = p.find("(deny default)").unwrap();
        assert!(import_pos < deny_pos);
    }

    #[test]
    fn macos_profile_grants_repo_read_and_write() {
        let p = macos_profile(&policy());
        assert!(p.contains(r#"(allow file-read* (subpath "/repo"))"#));
        assert!(p.contains(r#"(allow file-write* (subpath "/repo"))"#));
    }

    #[test]
    fn macos_profile_denies_network_by_default() {
        let p = macos_profile(&policy());
        assert!(p.contains("(deny network*)"));
        assert!(!p.contains("(allow network*)"));
    }

    #[test]
    fn macos_profile_allows_network_when_opted_in() {
        let p = macos_profile(&policy().with_net());
        assert!(p.contains("(allow network*)"));
        assert!(!p.contains("(deny network*)"));
    }

    #[test]
    fn macos_profile_grants_tmp_writable() {
        let p = macos_profile(&policy());
        assert!(p.contains(r#"(allow file-write* (subpath "/tmp"))"#));
        assert!(p.contains(r#"(allow file-write* (subpath "/private/tmp"))"#));
    }

    #[test]
    fn macos_profile_grants_extra_read_paths() {
        let pol = policy().allow_read("/Users/me/cache").unwrap();
        let p = macos_profile(&pol);
        assert!(p.contains(r#"(allow file-read* (subpath "/Users/me/cache"))"#));
    }

    #[test]
    fn macos_profile_escapes_quotes_in_repo_path() {
        let pol = SandboxPolicy::restrictive("/tmp/weird\"path").unwrap();
        let p = macos_profile(&pol);
        // Embedded quote is backslash-escaped, so the surrounding "" pair is
        // not broken — a profile reviewer / sandbox-exec parser sees one
        // contiguous string literal.
        assert!(p.contains(r#""/tmp/weird\"path""#));
    }

    #[test]
    fn macos_profile_includes_system_read_paths_for_dyld() {
        let p = macos_profile(&policy());
        for required in MACOS_SYSTEM_READ_SUBPATHS {
            assert!(p.contains(required), "missing system read for {required}");
        }
    }

    // ---------- Linux bwrap argv ----------

    #[test]
    fn linux_argv_binds_repo_read_write() {
        let argv = linux_bwrap_argv(&policy(), &["ls"]);
        let mut iter = argv.iter();
        let mut found = false;
        while let Some(arg) = iter.next() {
            if arg == "--bind" {
                let src = iter.next().unwrap();
                let dst = iter.next().unwrap();
                if src == "/repo" && dst == "/repo" {
                    found = true;
                    break;
                }
            }
        }
        assert!(found, "no `--bind /repo /repo` in argv: {argv:?}");
    }

    #[test]
    fn linux_argv_unshares_network_by_default() {
        let argv = linux_bwrap_argv(&policy(), &["ls"]);
        assert!(argv.iter().any(|s| s == "--unshare-net"));
    }

    #[test]
    fn linux_argv_omits_unshare_net_when_opted_in() {
        let argv = linux_bwrap_argv(&policy().with_net(), &["ls"]);
        assert!(!argv.iter().any(|s| s == "--unshare-net"));
    }

    #[test]
    fn linux_argv_uses_ro_bind_try_for_system_dirs() {
        // ro-bind-try not ro-bind so missing /lib64 on aarch64 doesn't fail.
        let argv = linux_bwrap_argv(&policy(), &["ls"]);
        let n = argv
            .iter()
            .filter(|s| s.as_str() == "--ro-bind-try")
            .count();
        assert!(n >= 5, "expected several --ro-bind-try entries, got {n}");
    }

    #[test]
    fn linux_argv_tmpfs_at_tmp() {
        let argv = linux_bwrap_argv(&policy(), &["ls"]);
        let mut it = argv.iter();
        let mut found = false;
        while let Some(a) = it.next() {
            if a == "--tmpfs" {
                if let Some(dst) = it.next() {
                    if dst == "/tmp" {
                        found = true;
                        break;
                    }
                }
            }
        }
        assert!(found, "no `--tmpfs /tmp` in argv: {argv:?}");
    }

    #[test]
    fn linux_argv_dies_with_parent_and_new_session() {
        let argv = linux_bwrap_argv(&policy(), &["ls"]);
        assert!(argv.iter().any(|s| s == "--die-with-parent"));
        assert!(argv.iter().any(|s| s == "--new-session"));
    }

    #[test]
    fn linux_argv_ends_with_command_after_double_dash() {
        let argv = linux_bwrap_argv(&policy(), &["bash", "-c", "echo hi"]);
        let dd = argv.iter().position(|s| s == "--").unwrap();
        assert_eq!(&argv[dd + 1..], &["bash", "-c", "echo hi"]);
    }

    #[test]
    fn linux_argv_includes_approved_read_paths() {
        let pol = policy().allow_read("/var/cache/atelier").unwrap();
        let argv = linux_bwrap_argv(&pol, &["ls"]);
        let mut found = false;
        let mut iter = argv.iter();
        while let Some(a) = iter.next() {
            if a == "--ro-bind" {
                let s = iter.next().unwrap();
                let d = iter.next().unwrap();
                if s == "/var/cache/atelier" && d == "/var/cache/atelier" {
                    found = true;
                    break;
                }
            }
        }
        assert!(found, "approved read path not bound: {argv:?}");
    }

    #[test]
    fn linux_argv_includes_approved_write_paths_read_write() {
        let pol = policy().allow_write("/var/scratch").unwrap();
        let argv = linux_bwrap_argv(&pol, &["ls"]);
        let mut found = false;
        let mut iter = argv.iter();
        while let Some(a) = iter.next() {
            if a == "--bind" {
                let s = iter.next().unwrap();
                let d = iter.next().unwrap();
                if s == "/var/scratch" && d == "/var/scratch" {
                    found = true;
                    break;
                }
            }
        }
        assert!(found, "approved write path not bound rw: {argv:?}");
    }
}
