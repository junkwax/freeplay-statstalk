//! Periodic Glicko-2 rating-period closer.
//!
//! The DB stores match results with `applied_at = NULL` until a rating
//! period closes. This task wakes up on a fixed cadence, calls
//! `Db::close_rating_period()`, and logs the outcome. It is the *only*
//! place Glicko-2 math runs at production scale — keeping the math
//! single-threaded by construction.
//!
//! The tradeoff: leaderboards lag by up to one period. We pick a short-
//! enough period (1 hour) that the lag is invisible to users in practice
//! while still giving the math enough match volume per player to behave
//! correctly.

use crate::db::Db;
use std::sync::Arc;
use std::time::Duration;

/// Default rating period length. Glickman recommends "the rating period
/// should be set so that the average player has 5–10 games per period."
/// We're not at that volume yet; 1 hour is a deliberate compromise — short
/// enough that leaderboards feel live, long enough that batched updates
/// produce meaningfully different math from per-match updates.
const PERIOD: Duration = Duration::from_secs(60 * 60);

pub fn spawn(db: Arc<Db>) {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(PERIOD);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        // First tick fires immediately; skip it so we don't close a period
        // on a freshly-started instance with nothing pending.
        ticker.tick().await;

        loop {
            ticker.tick().await;
            let db = db.clone();
            // close_rating_period() takes the SQLite mutex. Run it on a
            // blocking thread so we don't stall the runtime if the batch
            // is large or sqlite has to do meaningful I/O.
            let result = tokio::task::spawn_blocking(move || db.close_rating_period()).await;
            match result {
                Ok(Ok((matches, players))) => {
                    if matches > 0 || players > 0 {
                        tracing::info!(
                            "[rating] closed period — matches={} players_updated={}",
                            matches,
                            players,
                        );
                    }
                }
                Ok(Err(e)) => {
                    tracing::error!("[rating] close_rating_period failed: {e}");
                }
                Err(e) => {
                    tracing::error!("[rating] join error: {e}");
                }
            }
        }
    });
}
