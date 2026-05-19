//! v60.37 A2 — size-capped config-file reads.
//!
//! Every persistent config the harness loads (providers.toml, mcp_servers.toml,
//! .atelier/hooks/*.json, dod.v1.json, session.json, model_profile/*.json) is
//! deserialised by reading the whole file into memory. Without an explicit
//! size cap, a pathologically large file — written by a runaway model, a
//! hostile commit, or a corrupted disk — can OOM the agent at startup.
//!
//! This module centralises the cap-check so every loader can opt in with a
//! single line:
//!
//! ```ignore
//! let bytes = atelier_core::io_caps::read_capped(&path, atelier_core::io_caps::CAP_HOOK_CONFIG)?;
//! ```
//!
//! The check is `metadata().len() > cap` BEFORE any allocation, so an attacker
//! can't trigger a multi-GB allocation just to have us reject it after the
//! fact. Files whose size can't be determined ahead of time (named pipes,
//! sockets) fall back to a streamed read with a wrapping `take(cap+1)` so the
//! cap still holds. Returns `std::io::Result<Vec<u8>>` so callers can map into
//! their preferred error type.

use std::io::Read as _;
use std::path::Path;

// Per-call size caps. Tuned by the maximum legitimate size a well-behaved
// caller would ever produce; well under any actual OOM threshold so a cap
// hit always signals a problem.

/// Hook manifest (`.atelier/hooks/*.json`). One row + a few callable
/// references in a sane case; 1 MiB is generous.
pub const CAP_HOOK_CONFIG: usize = 1 << 20;

/// `providers.toml`. A handful of named profiles; 1 MiB is generous.
pub const CAP_PROVIDERS_TOML: usize = 1 << 20;

/// `mcp_servers.toml` + `mcp_catalog.json`. ~8 bundled catalog entries +
/// per-user server list; 1 MiB is generous.
pub const CAP_MCP_CONFIG: usize = 1 << 20;

/// `dod.v1.json`. The DoD config is a short list of acceptance criteria.
/// 1 MiB caps the malicious case while leaving room for verbose configs.
pub const CAP_DOD: usize = 1 << 20;

/// Model-profile cache (one file per `<provider>:<model>` hash). A single
/// capability matrix + probe outcome; 1 MiB is generous.
pub const CAP_MODEL_PROFILE: usize = 1 << 20;

/// `session.json`. Sessions accumulate conversation + tool fixtures so we
/// allow more headroom. 16 MiB caps the runaway case (a 100k-turn session
/// with a few-KB per turn would still fit).
pub const CAP_SESSION: usize = 16 << 20;

/// Recovery log (`recovery_log.json`). Append-only ledger of cancelled /
/// partial tool calls. 64 MiB allows a long-running session with many
/// crashes; over that, something is wrong.
pub const CAP_RECOVERY_LOG: usize = 64 << 20;

/// Read a file with a size cap. The cap is checked against `metadata().len()`
/// BEFORE any allocation. If metadata is unavailable (named pipes, sockets,
/// devices that report size 0), falls back to a streamed read with a wrapping
/// `take(cap+1)` so the cap still bounds memory.
///
/// Returns `std::io::Result<Vec<u8>>`. An over-cap file produces
/// `io::Error::new(InvalidData, "<path>: file size N exceeds cap M")` so
/// callers can pattern-match on the error if they want to surface a more
/// specific error type.
pub fn read_capped(path: &Path, cap: usize) -> std::io::Result<Vec<u8>> {
    let mut f = std::fs::File::open(path)?;
    // Fast path: stat tells us the size. Reject before allocating anything.
    if let Ok(meta) = f.metadata() {
        let len = meta.len();
        if len as usize > cap || (len > cap as u64) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("{}: file size {} exceeds cap {}", path.display(), len, cap),
            ));
        }
    }
    // Streamed read with a hard limit one byte past the cap. If we read
    // more bytes than `cap`, the metadata check didn't catch it (named
    // pipe / device / racy resize) — surface the cap error.
    let mut buf = Vec::with_capacity(cap.min(8 * 1024));
    let n = (&mut f).take(cap as u64 + 1).read_to_end(&mut buf)?;
    if n > cap {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("{}: read exceeded cap {}", path.display(), cap),
        ));
    }
    Ok(buf)
}

/// Convenience wrapper: read + UTF-8 decode + size cap.
pub fn read_capped_to_string(path: &Path, cap: usize) -> std::io::Result<String> {
    let bytes = read_capped(path, cap)?;
    String::from_utf8(bytes).map_err(|e| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("{}: invalid utf-8: {e}", path.display()),
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn read_capped_accepts_within_cap() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("ok.bin");
        std::fs::write(&path, b"hello world").unwrap();
        let got = read_capped(&path, 1024).unwrap();
        assert_eq!(got, b"hello world");
    }

    #[test]
    fn read_capped_rejects_over_cap() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("big.bin");
        std::fs::write(&path, vec![b'x'; 100]).unwrap();
        let err = read_capped(&path, 50).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
        assert!(err.to_string().contains("exceeds cap"), "got: {err}");
    }

    #[test]
    fn read_capped_at_exact_cap_accepts() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("exact.bin");
        std::fs::write(&path, vec![b'x'; 50]).unwrap();
        let got = read_capped(&path, 50).unwrap();
        assert_eq!(got.len(), 50);
    }

    #[test]
    fn read_capped_to_string_round_trip() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("text.txt");
        std::fs::write(&path, "héllo").unwrap();
        let s = read_capped_to_string(&path, 1024).unwrap();
        assert_eq!(s, "héllo");
    }

    #[test]
    fn read_capped_to_string_rejects_invalid_utf8() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("bad.bin");
        std::fs::write(&path, [0xFF, 0xFE, 0xFD]).unwrap();
        let err = read_capped_to_string(&path, 1024).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
        assert!(err.to_string().contains("invalid utf-8"), "got: {err}");
    }
}
