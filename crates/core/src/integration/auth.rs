//! API token generation, persistence, and verification.

use std::sync::{Arc, RwLock};

use crate::db::SettingsRepository;
use crate::integration::API_TOKEN_KEY;

/// Number of random bytes behind one token (256-bit).
const TOKEN_BYTES: usize = 32;

/// Mutable token slot shared by the server middleware and the settings UI.
///
/// The server reads the slot on every request, so a regenerated token applies
/// without rebinding the listener. Locks guard a single `String` clone and are
/// never held across `.await`.
#[derive(Debug, Clone, Default)]
pub struct TokenCell {
    current: Arc<RwLock<String>>,
}

impl TokenCell {
    /// Creates a slot seeded with `token`.
    #[must_use]
    pub fn new(token: impl Into<String>) -> Self {
        Self {
            current: Arc::new(RwLock::new(token.into())),
        }
    }

    /// Copies the current token out of the slot.
    #[must_use]
    pub fn get(&self) -> String {
        self.current
            .read()
            .expect("token cell lock poisoned")
            .clone()
    }

    /// Replaces the current token.
    pub fn set(&self, token: impl Into<String>) {
        *self.current.write().expect("token cell lock poisoned") = token.into();
    }
}

/// Generate-and-persists API token, returned as base64url without padding.
///
/// # Errors
///
/// Returns an error when the settings table cannot be read or written.
pub async fn load_or_create(repository: &SettingsRepository) -> Result<String, crate::db::DbError> {
    if let Some(existing) = repository.get_raw(API_TOKEN_KEY).await?
        && let Some(token) = parse_stored(&existing)
    {
        return Ok(token);
    }
    let token = generate_token()?;
    let now_ms = now_millis();
    repository.set_raw(API_TOKEN_KEY, &token, now_ms).await?;
    tracing::info!("generated a new integration API token");
    Ok(token)
}

/// Replaces the persisted token with a fresh one.
///
/// # Errors
///
/// Returns an error when randomness or persistence fails.
pub async fn regenerate(repository: &SettingsRepository) -> Result<String, crate::db::DbError> {
    let token = generate_token()?;
    let now_ms = now_millis();
    repository.set_raw(API_TOKEN_KEY, &token, now_ms).await?;
    Ok(token)
}

/// Constant-time-ish comparison of a presented token against the expected one.
///
/// Both sides are compared byte-by-byte with no early exit so the duration does
/// not reveal the matching prefix length.
#[must_use]
pub fn verify(expected: &str, presented: &str) -> bool {
    let expected = expected.as_bytes();
    let presented = presented.as_bytes();
    if expected.len() != presented.len() {
        return false;
    }
    let mut diff = 0_u8;
    for (a, b) in expected.iter().zip(presented.iter()) {
        diff |= a ^ b;
    }
    diff == 0
}

fn parse_stored(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    let unquoted = trimmed
        .strip_prefix('"')
        .and_then(|rest| rest.strip_suffix('"'))
        .unwrap_or(trimmed);
    if unquoted.is_empty() {
        return None;
    }
    Some(unquoted.to_owned())
}

fn generate_token() -> Result<String, crate::db::DbError> {
    let mut bytes = [0_u8; TOKEN_BYTES];
    getrandom::fill(&mut bytes)
        .map_err(|error| crate::db::DbError::TokenGeneration(error.to_string()))?;
    Ok(base64_url(&bytes))
}

fn base64_url(bytes: &[u8]) -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

fn now_millis() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis().try_into().unwrap_or(i64::MAX))
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_cell_should_expose_the_latest_value_to_every_clone() {
        let cell = TokenCell::new("first");
        let shared = cell.clone();
        assert_eq!(shared.get(), "first");

        cell.set("second");
        assert_eq!(shared.get(), "second");
        assert_eq!(TokenCell::default().get(), "");
    }

    #[test]
    fn verify_should_accept_only_exact_matches() {
        assert!(verify("abc123", "abc123"));
        assert!(!verify("abc123", "abc124"));
        assert!(!verify("abc123", "abc12"));
        assert!(!verify("abc123", ""));
    }

    #[test]
    fn generated_token_should_be_url_safe_and_fixed_length() {
        let token = generate_token().expect("entropy available");
        // 32 bytes base64url without padding = 43 characters.
        assert_eq!(token.len(), 43);
        assert!(
            token
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        );
        let other = generate_token().expect("entropy available");
        assert_ne!(token, other);
    }

    #[test]
    fn stored_value_should_tolerate_plain_and_quoted_json() {
        assert_eq!(parse_stored("\"abc\""), Some("abc".to_owned()));
        assert_eq!(parse_stored("abc"), Some("abc".to_owned()));
        assert_eq!(parse_stored("\"\""), None);
        assert_eq!(parse_stored("  "), None);
    }

    #[tokio::test]
    async fn regenerate_should_persist_and_survive_a_reload() {
        let dir = tempfile::tempdir().expect("temp dir must exist");
        let db_path = dir.path().join("raijin-token-test.sqlite");
        let pool = crate::db::bootstrap(&format!("sqlite://{}", db_path.display()))
            .await
            .expect("bootstrap must succeed");
        let repository = crate::db::SettingsRepository::new(pool);

        let first = load_or_create(&repository).await.expect("token created");
        assert_eq!(first.len(), 43);
        // Loading twice must not rotate: the extension keeps its paste.
        assert_eq!(
            load_or_create(&repository).await.expect("token reloaded"),
            first
        );

        let rotated = regenerate(&repository).await.expect("token rotated");
        assert_ne!(rotated, first);
        assert_eq!(
            load_or_create(&repository).await.expect("token reloaded"),
            rotated
        );
    }
}
