//! Provider trust-boundary helpers for the Tauri backend.
//!
//! Keep endpoint parsing and allowlist checks out of command handlers so GUI
//! provider policy cannot drift from the CLI/core trust-boundary predicates.

pub const SWAP_BASE_URL_ALLOWLIST: &[&str] = atelier_core::PROVIDER_BASE_URL_ALLOWLIST;

/// Predicate for whether a provider `base_url` is allowed. `None` base_url
/// (e.g. Anthropic uses no `base_url`) is allowed; only an explicit value off
/// the shared provider allowlist is refused.
pub fn is_base_url_allowed(base_url: Option<&str>) -> bool {
    atelier_core::provider_base_url_allowed(base_url)
}

pub(crate) fn effective_openai_base_url(base_url: Option<String>) -> String {
    base_url.unwrap_or_else(|| {
        std::env::var("OPENAI_BASE_URL").unwrap_or_else(|_| "https://api.openai.com/v1".to_string())
    })
}

pub(crate) fn ensure_base_url_allowed(base_url: Option<&str>) -> Result<(), String> {
    if is_base_url_allowed(base_url) {
        Ok(())
    } else {
        Err(format!(
            "base_url {:?} not in swap_adapter allowlist",
            base_url.unwrap_or("<none>")
        ))
    }
}

fn extract_host_port(base_url: &str) -> Option<String> {
    let rest = base_url
        .strip_prefix("https://")
        .or_else(|| base_url.strip_prefix("http://"))?;
    let host_port = rest.split('/').next()?;
    if host_port.is_empty() {
        return None;
    }
    if host_port.contains(':') {
        Some(host_port.to_string())
    } else {
        let default_port = if base_url.starts_with("https://") {
            "443"
        } else {
            "80"
        };
        Some(format!("{host_port}:{default_port}"))
    }
}

/// Returns `true` when the host:port in `base_url` accepts a TCP connection
/// within 1 second. Optimistically returns `true` if the URL can't be parsed.
pub(crate) fn preflight_base_url(base_url: &str) -> bool {
    use std::net::ToSocketAddrs;
    let Some(addr_str) = extract_host_port(base_url) else {
        return true;
    };
    let addrs: Vec<_> = match addr_str.to_socket_addrs() {
        Ok(a) => a.collect(),
        Err(_) => return true,
    };
    for sa in addrs {
        if std::net::TcpStream::connect_timeout(&sa, std::time::Duration::from_secs(1)).is_ok() {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allowlist_delegates_to_core_trust_boundary() {
        assert!(is_base_url_allowed(Some("https://api.openai.com/v1")));
        assert!(is_base_url_allowed(Some("http://localhost:11434/v1")));
        assert!(!is_base_url_allowed(Some("https://evil.example/v1")));
        assert!(!is_base_url_allowed(Some("gopher://api.openai.com/v1")));
    }

    #[test]
    fn effective_url_prefers_explicit_value() {
        assert_eq!(
            effective_openai_base_url(Some("http://localhost:11434/v1".into())),
            "http://localhost:11434/v1"
        );
    }
}
