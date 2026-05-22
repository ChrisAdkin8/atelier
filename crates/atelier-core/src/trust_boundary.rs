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
/// The Atelier dev vLLM ALB is included as a project-owned OpenAI-compatible
/// endpoint used by the GUI and repo profile during local development.
pub const PROVIDER_BASE_URL_ALLOWLIST: &[&str] = &[
    "api.anthropic.com",
    "api.openai.com",
    "atelier-gpu-vllm-dev-1460977764.us-east-1.elb.amazonaws.com",
    "localhost",
    "127.0.0.1",
    "::1",
];

/// Return `true` when `base_url` is absent or its host is on
/// [`PROVIDER_BASE_URL_ALLOWLIST`].
pub fn provider_base_url_allowed(base_url: Option<&str>) -> bool {
    let Some(url) = base_url else {
        return true;
    };
    let Some(host) = http_host(url) else {
        return false;
    };
    PROVIDER_BASE_URL_ALLOWLIST.iter().any(|h| *h == host)
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
        assert!(provider_base_url_allowed(Some(
            "http://atelier-gpu-vllm-dev-1460977764.us-east-1.elb.amazonaws.com/v1"
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
