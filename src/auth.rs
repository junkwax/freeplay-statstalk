use axum::http::{HeaderMap, StatusCode};
use jsonwebtoken::{decode, DecodingKey, Validation};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Claims {
    pub sub: String,
    pub username: String,
    pub exp: usize,
}

pub fn check_api_key(headers: &HeaderMap, expected: &str) -> Result<(), StatusCode> {
    let auth = headers.get("Authorization")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let token = auth.strip_prefix("Bearer ").unwrap_or("");
    if token == expected {
        Ok(())
    } else {
        Err(StatusCode::UNAUTHORIZED)
    }
}

/// Verify a Discord-OAuth-issued JWT minted by the signaling server.
/// Returns the verified claims so handlers can read the discord_id (`sub`)
/// and use it as the authoritative identity rather than trusting headers.
pub fn verify_jwt(headers: &HeaderMap, secret: Option<&str>) -> Result<Claims, StatusCode> {
    let Some(secret) = secret else {
        // Server hasn't been configured with JWT_SECRET yet — refuse rather
        // than silently letting requests through.
        tracing::error!("JWT verification requested but JWT_SECRET is not configured");
        return Err(StatusCode::SERVICE_UNAVAILABLE);
    };
    let auth = headers.get("Authorization")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let token = auth.strip_prefix("Bearer ").unwrap_or("");
    if token.is_empty() {
        return Err(StatusCode::UNAUTHORIZED);
    }
    decode::<Claims>(
        token,
        &DecodingKey::from_secret(secret.as_bytes()),
        &Validation::default(),
    )
    .map(|d| d.claims)
    .map_err(|e| {
        tracing::debug!("JWT verify failed: {e}");
        StatusCode::UNAUTHORIZED
    })
}
