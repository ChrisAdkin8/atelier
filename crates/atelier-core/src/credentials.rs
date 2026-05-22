//! Credential resolution for provider profiles.

pub const DEFAULT_KEYRING_SERVICE: &str = "atelier";
pub const OPENAI_API_KEY_ENV: &str = "OPENAI_API_KEY";

#[derive(Debug, thiserror::Error)]
pub enum CredentialError {
    #[error("credential reference {reference:?} is invalid: expected env:NAME, keyring:USER, or keyring:SERVICE/USER")]
    InvalidReference { reference: String },

    #[error("environment variable {name} is not set or is empty")]
    MissingEnv { name: String },

    #[error("keyring reference {reference:?} is invalid: {message}")]
    InvalidKeyringReference { reference: String, message: String },

    #[error("failed to open keyring entry {service}/{user}: {source}")]
    KeyringEntry {
        service: String,
        user: String,
        #[source]
        source: keyring::Error,
    },

    #[error("failed to read keyring entry {service}/{user}: {source}")]
    KeyringRead {
        service: String,
        user: String,
        #[source]
        source: keyring::Error,
    },

    #[error("failed to write keyring entry {service}/{user}: {source}")]
    KeyringWrite {
        service: String,
        user: String,
        #[source]
        source: keyring::Error,
    },
}

pub fn default_provider_api_key_ref(profile_name: &str) -> String {
    format!("keyring:{DEFAULT_KEYRING_SERVICE}/providers/{profile_name}")
}

pub fn validate_api_key_ref(reference: &str) -> Result<(), CredentialError> {
    if let Some(name) = reference.strip_prefix("env:") {
        if is_valid_env_name(name) {
            return Ok(());
        }
        return Err(CredentialError::InvalidReference {
            reference: reference.to_string(),
        });
    }
    if reference.strip_prefix("keyring:").is_some() {
        parse_keyring_ref(reference).map(|_| ())
    } else {
        Err(CredentialError::InvalidReference {
            reference: reference.to_string(),
        })
    }
}

pub fn resolve_openai_api_key(reference: Option<&str>) -> Result<String, CredentialError> {
    resolve_api_key(reference, OPENAI_API_KEY_ENV)
}

pub fn resolve_api_key(
    reference: Option<&str>,
    fallback_env: &str,
) -> Result<String, CredentialError> {
    if let Ok(value) = std::env::var(fallback_env) {
        if !value.is_empty() {
            return Ok(value);
        }
    }
    match reference {
        Some(reference) => resolve_api_key_ref(reference),
        None => Ok(String::new()),
    }
}

pub fn resolve_api_key_ref(reference: &str) -> Result<String, CredentialError> {
    if let Some(name) = reference.strip_prefix("env:") {
        if !is_valid_env_name(name) {
            return Err(CredentialError::InvalidReference {
                reference: reference.to_string(),
            });
        }
        return std::env::var(name)
            .ok()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| CredentialError::MissingEnv {
                name: name.to_string(),
            });
    }
    let Some(_) = reference.strip_prefix("keyring:") else {
        return Err(CredentialError::InvalidReference {
            reference: reference.to_string(),
        });
    };
    let (service, user) = parse_keyring_ref(reference)?;
    let entry =
        keyring::Entry::new(&service, &user).map_err(|source| CredentialError::KeyringEntry {
            service: service.clone(),
            user: user.clone(),
            source,
        })?;
    entry
        .get_password()
        .map_err(|source| CredentialError::KeyringRead {
            service,
            user,
            source,
        })
}

pub fn store_api_key_ref(reference: &str, secret: &str) -> Result<(), CredentialError> {
    let (service, user) = parse_keyring_ref(reference)?;
    let entry =
        keyring::Entry::new(&service, &user).map_err(|source| CredentialError::KeyringEntry {
            service: service.clone(),
            user: user.clone(),
            source,
        })?;
    entry
        .set_password(secret)
        .map_err(|source| CredentialError::KeyringWrite {
            service,
            user,
            source,
        })
}

pub fn api_key_ref_may_resolve(reference: Option<&str>, fallback_env: &str) -> bool {
    reference.is_some()
        || std::env::var(fallback_env)
            .map(|s| !s.is_empty())
            .unwrap_or(false)
}

fn parse_keyring_ref(reference: &str) -> Result<(String, String), CredentialError> {
    let body =
        reference
            .strip_prefix("keyring:")
            .ok_or_else(|| CredentialError::InvalidReference {
                reference: reference.to_string(),
            })?;
    if body.is_empty() {
        return Err(CredentialError::InvalidKeyringReference {
            reference: reference.to_string(),
            message: "missing key name".to_string(),
        });
    }
    let (service, user) = match body.split_once('/') {
        Some((service, user)) => (service, user),
        None => (DEFAULT_KEYRING_SERVICE, body),
    };
    if service.is_empty() || user.is_empty() {
        return Err(CredentialError::InvalidKeyringReference {
            reference: reference.to_string(),
            message: "service and user must both be non-empty".to_string(),
        });
    }
    Ok((service.to_string(), user.to_string()))
}

fn is_valid_env_name(name: &str) -> bool {
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    (first == '_' || first.is_ascii_alphabetic())
        && chars.all(|c| c == '_' || c.is_ascii_alphanumeric())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_supported_reference_schemes() {
        validate_api_key_ref("env:OPENAI_API_KEY").unwrap();
        validate_api_key_ref("keyring:providers/local").unwrap();
        validate_api_key_ref("keyring:atelier/providers/local").unwrap();
        assert!(validate_api_key_ref("sk-plaintext").is_err());
        assert!(validate_api_key_ref("env:").is_err());
        assert!(validate_api_key_ref("keyring:").is_err());
    }

    #[test]
    fn default_provider_ref_uses_atelier_service() {
        assert_eq!(
            default_provider_api_key_ref("qwen"),
            "keyring:atelier/providers/qwen"
        );
    }
}
