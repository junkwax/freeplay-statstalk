use anyhow::Context;

#[derive(Clone, Debug)]
pub struct Config {
    pub stats_api_key: String,
    pub discord_webhook_url: Option<String>,
    pub db_path: String,
    /// Shared with the signaling server. Lets stats verify client-issued
    /// JWTs (e.g. for ghost upload) without round-tripping through signaling.
    /// Optional during rollout — when missing, JWT-protected endpoints
    /// return 503.
    pub jwt_secret: Option<String>,
}

impl Config {
    pub fn from_env() -> anyhow::Result<Self> {
        Ok(Self {
            stats_api_key: std::env::var("STATS_API_KEY")
                .context("STATS_API_KEY not set")?,
            discord_webhook_url: std::env::var("DISCORD_WEBHOOK_URL").ok(),
            db_path: std::env::var("DB_PATH")
                .unwrap_or_else(|_| "stats.db".to_string()),
            jwt_secret: std::env::var("JWT_SECRET").ok().filter(|s| !s.is_empty()),
        })
    }
}
