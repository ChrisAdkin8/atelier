//! LSP install env-allowlist (v60.34 M23).
//!
//! Future LSP-install subprocesses (`npm install -g
//! typescript-language-server`, `pip install --user pylsp`, …) must spawn
//! through the same `env_clear` + allowlist pattern enforced by
//! `subprocess::run`. Package managers need a small set of extra env
//! vars beyond the base [`crate::subprocess::ENV_PASSTHROUGH`]
//! (`NPM_*` for npm, `PIP_*` / `PYTHONPATH` for pip) — those are
//! opt-in via [`install_env_allowlist`].
//!
//! This module ships the allowlist + a probe entry point; the actual
//! install driver lands with the LSP install track. The contract this
//! file pins (via tests) is "an LSP install subprocess never sees a
//! parent env var that isn't on the merged allowlist".

use std::collections::BTreeMap;

use crate::subprocess::ENV_PASSTHROUGH;

/// Extra env vars an LSP install path may opt into. Kept narrow:
///
///   - `NPM_*` — npm picks up registry, cache, prefix overrides through
///     these. Leaking the parent's `NPM_TOKEN` is the explicit risk;
///     the install path opts in to that only when the user has set the
///     opt-in flag.
///   - `PIP_*` — pip's analogous knobs (index URL, cache, no-binary).
///   - `PYTHONPATH` — pip-installed scripts that wrap Python need this
///     resolved for the wrapper to find its install.
///
/// Anything else (notably `GITHUB_TOKEN`, `AWS_*`, SSH agent vars)
/// stays scrubbed.
pub const LSP_INSTALL_ENV_EXTRAS: &[&str] = &[
    "NPM_CONFIG_REGISTRY",
    "NPM_CONFIG_PREFIX",
    "NPM_CONFIG_CACHE",
    "PIP_INDEX_URL",
    "PIP_CACHE_DIR",
    "PYTHONPATH",
];

/// Build the env map an LSP install subprocess inherits. Equivalent to
/// `subprocess::run`'s baseline (`env_clear` + `ENV_PASSTHROUGH`) plus
/// the [`LSP_INSTALL_ENV_EXTRAS`] when `allow_package_manager_extras`
/// is true.
///
/// The caller hands the result to `tokio::process::Command::envs(...)`
/// after `env_clear()`. Test code can also assert against the returned
/// map directly without spawning a child.
pub fn install_env_allowlist(allow_package_manager_extras: bool) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    for key in ENV_PASSTHROUGH {
        if let Ok(val) = std::env::var(key) {
            out.insert((*key).to_string(), val);
        }
    }
    if allow_package_manager_extras {
        for key in LSP_INSTALL_ENV_EXTRAS {
            if let Ok(val) = std::env::var(key) {
                out.insert((*key).to_string(), val);
            }
        }
    }
    out
}

#[cfg(test)]
mod install_tests {
    use super::*;

    // v60.34 (M23) — a sentinel env var the allowlist doesn't include
    // must not appear in the install subprocess's env. The
    // `install_env_allowlist` builder is the source of truth — any
    // future install driver MUST feed its env through this helper.
    #[test]
    fn allowlist_excludes_unlisted_sentinel_var() {
        let sentinel = "ATELIER_LSP_INSTALL_TEST_DO_NOT_LEAK";
        // SAFETY: per-process env mutation; restored at end of test.
        unsafe { std::env::set_var(sentinel, "leaked-value") };
        let env = install_env_allowlist(false);
        // SAFETY: per-process env mutation.
        unsafe { std::env::remove_var(sentinel) };
        assert!(
            !env.contains_key(sentinel),
            "sentinel env var leaked into install allowlist: {env:?}"
        );
    }

    #[test]
    fn allowlist_excludes_npm_extras_when_opt_in_is_false() {
        let extra = "NPM_CONFIG_REGISTRY";
        // SAFETY: per-process env mutation.
        unsafe { std::env::set_var(extra, "https://example.org") };
        let env = install_env_allowlist(false);
        unsafe { std::env::remove_var(extra) };
        assert!(
            !env.contains_key(extra),
            "{extra} leaked despite opt-in being false"
        );
    }

    #[test]
    fn allowlist_includes_npm_extras_when_opt_in_is_true() {
        let extra = "NPM_CONFIG_REGISTRY";
        // SAFETY: per-process env mutation.
        unsafe { std::env::set_var(extra, "https://example.org") };
        let env = install_env_allowlist(true);
        unsafe { std::env::remove_var(extra) };
        assert_eq!(
            env.get(extra).map(String::as_str),
            Some("https://example.org")
        );
    }

    #[test]
    fn allowlist_passes_path() {
        // PATH is on the base passthrough; without it the install
        // subprocess wouldn't be able to find `npm` / `pip`.
        let env = install_env_allowlist(false);
        // Either PATH was unset in the test env (CI minimal sandbox),
        // or it's in the map.
        if std::env::var("PATH").is_ok() {
            assert!(env.contains_key("PATH"));
        }
    }
}
