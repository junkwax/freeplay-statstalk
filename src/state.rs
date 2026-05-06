use std::path::PathBuf;
use std::sync::Arc;
use crate::{config::Config, db::Db, discord::Discord};

#[derive(Clone)]
pub struct AppState {
    pub config: Config,
    pub db: Arc<Db>,
    pub discord: Arc<Discord>,
    pub ghosts_dir: PathBuf,
}

impl AsRef<AppState> for AppState {
    fn as_ref(&self) -> &AppState { self }
}

impl AppState {
    pub async fn new(config: Config) -> anyhow::Result<Self> {
        let db = Db::open(&config.db_path)?;
        let discord = Discord::new(config.discord_webhook_url.clone());
        let ghosts_dir = std::path::Path::new(&config.db_path).parent()
            .unwrap_or(std::path::Path::new("/db"))
            .join("ghosts");
        std::fs::create_dir_all(&ghosts_dir)?;
        tracing::info!("Ghost storage dir: {}", ghosts_dir.display());
        tracing::info!("SQLite database ready at {}", config.db_path);
        Ok(Self {
            config,
            db: Arc::new(db),
            discord: Arc::new(discord),
            ghosts_dir,
        })
    }
}
