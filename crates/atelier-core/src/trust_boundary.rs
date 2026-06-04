//! Shared trust-boundary predicates.
//!
//! This module holds small policy helpers that must not drift between the
//! CLI, GUI, TUI, and core dispatcher paths. The helpers are deliberately
//! narrow: they do not decide UX, consent, or persistence; they only answer
//! whether a value crosses a high-risk boundary.

/// Built-in allowlist for provider endpoints that may receive bearer
/// credentials without an additional per-surface approval prompt.
///
/// Loopback is included for local OpenAI-compatible servers. Arbitrary
/// remote OpenAI-compatible endpoints are still supported when explicitly
/// supplied by the user at the invoking surface; repo-controlled profile
/// files should not silently route `OPENAI_API_KEY` to them.
///
/// Entries may contain a single `*` wildcard that matches one or more
/// characters when compared against an extracted host. This is used to
/// cover the Atelier dev vLLM ALB whose DNS name embeds an ephemeral
/// load-balancer ID (`atelier-gpu-vllm-<env>-<lbid>.<region>.elb.amazonaws.com`);
/// pinning the exact DNS makes the allowlist drift every time the LB is
/// rebuilt or moved to a new region. The wildcard is host-scoped — `*`
/// never matches a path/query and only fires after `http_host` has already
/// stripped the scheme, port, and userinfo, so a hostile URL can't sneak
/// in via path components.
pub const PROVIDER_BASE_URL_ALLOWLIST: &[&str] = &[
    "api.anthropic.com",
    "api.openai.com",
    "atelier-gpu-vllm-*.elb.amazonaws.com",
    "localhost",
    "127.0.0.1",
    "::1",
];

/// Return `true` when `base_url` is absent or its host matches an entry
/// on [`PROVIDER_BASE_URL_ALLOWLIST`]. Entries with a single `*` are
/// treated as wildcard patterns matching one or more characters; entries
/// without `*` require exact equality.
pub fn provider_base_url_allowed(base_url: Option<&str>) -> bool {
    let Some(url) = base_url else {
        return true;
    };
    let Some(host) = http_host(url) else {
        return false;
    };
    PROVIDER_BASE_URL_ALLOWLIST
        .iter()
        .any(|pattern| host_matches_pattern(pattern, &host))
}

/// Match a host string against an allowlist entry. Entries with no `*`
/// must equal `host` exactly; entries with one `*` must satisfy
/// `host.starts_with(prefix) && host.ends_with(suffix)` with at least
/// one character matched by the wildcard. A pattern with more than one
/// `*` is rejected (treated as a non-match) rather than guessed.
fn host_matches_pattern(pattern: &str, host: &str) -> bool {
    match pattern.split_once('*') {
        None => pattern == host,
        Some((prefix, suffix)) => {
            if suffix.contains('*') {
                return false;
            }
            host.len() > prefix.len() + suffix.len()
                && host.starts_with(prefix)
                && host.ends_with(suffix)
        }
    }
}

/// Extract a lower-case host from an explicit HTTP/HTTPS URL.
///
/// This intentionally rejects bare hosts and non-HTTP schemes. It handles
/// user-info, ports, query/fragment suffixes, and bracketed IPv6 literals.
pub fn http_host(url: &str) -> Option<String> {
    let (scheme, rest) = url.split_once("://")?;
    let scheme_lc = scheme.to_ascii_lowercase();
    if scheme_lc != "http" && scheme_lc != "https" {
        return None;
    }
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    let authority = authority
        .rsplit_once('@')
        .map(|(_, h)| h)
        .unwrap_or(authority);
    let host = if let Some(stripped) = authority.strip_prefix('[') {
        stripped.split_once(']').map(|(h, _)| h).unwrap_or(stripped)
    } else {
        authority
            .rsplit_once(':')
            .map(|(h, _)| h)
            .unwrap_or(authority)
    };
    if host.is_empty() {
        None
    } else {
        Some(host.to_ascii_lowercase())
    }
}

/// Guard against repo-controlled provider profiles silently exfiltrating
/// cloud credentials. Returns `Ok(())` when either:
///
/// - no credential is present,
/// - the base URL was explicitly supplied by the user for this invocation, or
/// - the profile URL is on the built-in allowlist.
pub fn provider_profile_base_url_may_receive_credential(
    base_url: Option<&str>,
    supplied_explicitly: bool,
    credential_present: bool,
) -> Result<(), String> {
    if !credential_present || supplied_explicitly || provider_base_url_allowed(base_url) {
        return Ok(());
    }
    Err(format!(
        "profile base_url {:?} is not in the provider credential allowlist; \
         pass --base-url explicitly for this run or use an allowlisted host",
        base_url.unwrap_or("<none>")
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn http_host_requires_explicit_http_scheme() {
        assert_eq!(
            http_host("https://api.openai.com/v1").as_deref(),
            Some("api.openai.com")
        );
        assert_eq!(
            http_host("HTTP://LOCALHOST:11434/v1").as_deref(),
            Some("localhost")
        );
        assert_eq!(http_host("localhost:11434/v1"), None);
        assert_eq!(http_host("file:///etc/passwd"), None);
        assert_eq!(http_host("gopher://api.openai.com/v1"), None);
    }

    #[test]
    fn provider_allowlist_accepts_loopback_and_known_cloud_hosts() {
        assert!(provider_base_url_allowed(Some("https://api.openai.com/v1")));
        assert!(provider_base_url_allowed(Some(
            "https://api.anthropic.com/v1"
        )));
        // Wildcard `atelier-gpu-vllm-*.elb.amazonaws.com` covers both the
        // original us-east-1 LB and the post-region-migration us-west-2 LB
        // (whose LB ID changed). New deployments of the same module pattern
        // are also accepted without an allowlist edit.
        assert!(provider_base_url_allowed(Some(
            "http://atelier-gpu-vllm-dev-1460977764.us-east-1.elb.amazonaws.com/v1"
        )));
        assert!(provider_base_url_allowed(Some(
            "http://atelier-gpu-vllm-dev-654802396.us-west-2.elb.amazonaws.com/v1"
        )));
        assert!(provider_base_url_allowed(Some(
            "http://atelier-gpu-vllm-prod-99999.eu-west-1.elb.amazonaws.com/v1"
        )));
        assert!(provider_base_url_allowed(Some("http://localhost:11434/v1")));
        assert!(provider_base_url_allowed(Some("http://127.0.0.1:8080/v1")));
        assert!(provider_base_url_allowed(Some("http://[::1]:8080/v1")));
        assert!(provider_base_url_allowed(None));
    }

    #[test]
    fn provider_allowlist_rejects_unknown_hosts() {
        assert!(!provider_base_url_allowed(Some("https://evil.example/v1")));
        assert!(!provider_base_url_allowed(Some("http://attacker.test/v1")));
    }

    #[test]
    fn provider_allowlist_wildcard_does_not_match_prefix_injection() {
        // An attacker who controls `evil.example` cannot bypass the allowlist
        // by appending the legit suffix to their hostname — the wildcard is
        // host-scoped and requires the prefix to match from the start.
        assert!(!provider_base_url_allowed(Some(
            "https://evil.atelier-gpu-vllm-dev.elb.amazonaws.com/v1"
        )));
        assert!(!provider_base_url_allowed(Some(
            "https://atelier-gpu-vllm-dev.elb.amazonaws.com.evil.example/v1"
        )));
    }

    #[test]
    fn provider_allowlist_wildcard_requires_at_least_one_char() {
        // The wildcard must match at least one character so the prefix and
        // suffix can't simply concatenate without a separator.
        assert!(!host_matches_pattern(
            "atelier-gpu-vllm-*.elb.amazonaws.com",
            "atelier-gpu-vllm-.elb.amazonaws.com"
        ));
    }

    #[test]
    fn provider_allowlist_pattern_rejects_multiple_wildcards() {
        // Defence-in-depth: an entry the developer wrote with two `*` is
        // ambiguous, so the predicate refuses it rather than guessing.
        // (PROVIDER_BASE_URL_ALLOWLIST has no such entries today; this
        // pins the behaviour for any future addition.)
        assert!(!host_matches_pattern("a-*-b-*", "a-X-b-Y"));
    }

    #[test]
    fn profile_credential_guard_allows_explicit_or_uncredentialed_urls() {
        assert!(provider_profile_base_url_may_receive_credential(
            Some("https://evil.example/v1"),
            true,
            true
        )
        .is_ok());
        assert!(provider_profile_base_url_may_receive_credential(
            Some("https://evil.example/v1"),
            false,
            false
        )
        .is_ok());
    }

    #[test]
    fn profile_credential_guard_rejects_repo_profile_exfiltration() {
        let err = provider_profile_base_url_may_receive_credential(
            Some("https://evil.example/v1"),
            false,
            true,
        )
        .unwrap_err();
        assert!(err.contains("allowlist"), "got: {err}");
        assert!(err.contains("evil.example"), "got: {err}");
    }
}
