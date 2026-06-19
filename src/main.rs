mod auth;
mod config;
mod db;
mod discord;
mod glicko;
mod handlers;
mod models;
mod rating_period;
mod state;

use axum::{
    routing::{get, post},
    Router,
};
use tower_http::cors::{Any, CorsLayer};
use tower_http::trace::TraceLayer;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

use crate::state::AppState;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    dotenvy::dotenv().ok();

    tracing_subscriber::registry()
        .with(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "freeplay_stats=debug,tower_http=debug".into()),
        )
        .with(tracing_subscriber::fmt::layer())
        .init();

    let config = config::Config::from_env()?;
    let state = AppState::new(config).await?;

    // Kick off the Glicko-2 periodic closer. Idempotent — safe even on
    // the first deploy or after restart, since it only operates on rows
    // with applied_at = NULL.
    rating_period::spawn(state.db.clone());

    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods(Any)
        .allow_headers(Any);

    let app = Router::new()
        .route("/health", get(|| async { "ok" }))
        .route("/results", post(handlers::post_result))
        .route("/leaderboard", get(handlers::leaderboard))
        .route("/player/:discord_id", get(handlers::player_profile))
        .route("/player/:discord_id/history", get(handlers::player_history))
        // Username registry — claim a globally-unique display name / check one.
        .route("/name/claim", post(handlers::claim_name))
        .route("/name/check/:name", get(handlers::check_name))
        // Ghost file storage — upload from client, download for playback
        .route("/ghosts/upload", post(handlers::upload_ghost))
        .route("/ghosts/list", get(handlers::list_ghosts))
        .route("/ghosts/download/:ghost_id", get(handlers::download_ghost))
        // Full match replay storage — Fightcade-style replay browser
        .route("/replays/upload", post(handlers::upload_replay))
        .route("/replays/list", get(handlers::list_replays))
        .route(
            "/replays/download/:replay_id",
            get(handlers::download_replay),
        )
        .layer(cors)
        .layer(TraceLayer::new_for_http())
        .with_state(state);

    let port = std::env::var("PORT").unwrap_or_else(|_| "8081".to_string());
    let addr = format!("0.0.0.0:{}", port);
    tracing::info!("Stats service listening on {}", addr);

    let listener = tokio::net::TcpListener::bind(&addr).await?;
    // Graceful shutdown on SIGTERM (Cloud Run sends this on revision swap).
    // Without this, in-flight requests are cut at deploy time.
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;
    Ok(())
}

async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut s) => {
                s.recv().await;
            }
            Err(_) => std::future::pending::<()>().await,
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => tracing::info!("ctrl-c received, shutting down"),
        _ = terminate => tracing::info!("SIGTERM received, shutting down"),
    }
}
