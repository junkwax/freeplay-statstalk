use axum::{
    body::Bytes,
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Json},
};
use serde::Deserialize;

use crate::{auth, models::*, state::AppState};

#[derive(Deserialize)]
pub struct LeaderboardQuery {
    #[serde(default = "default_limit")]
    pub limit: u32,
}

fn default_limit() -> u32 {
    50
}

#[derive(Deserialize)]
pub struct HistoryQuery {
    #[serde(default = "default_limit")]
    pub limit: u32,
}

// ── POST /results ─────────────────────────────────────────────────────────

pub async fn post_result(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(result): Json<MatchResult>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    if let Err(e) = auth::check_api_key(&headers, &state.config.stats_api_key) {
        return Err(e);
    }
    // Name the players (we store the match reporter's Discord username on first
    // encounter for each player so the leaderboard has human-readable names).
    // At this point we only have discord IDs; usernames come later when the
    // signaling server includes them, or we leave them blank until a player
    // looks themselves up (which we could augment later).

    match state.db.process_result(
        &result.winner_id,
        &result.loser_id,
        result.winner_score,
        result.loser_score,
        &result.room_id,
        &result.rom_hash,
    ) {
        Ok((_winner_rating, _loser_rating)) => {
            // Update usernames so leaderboard/history shows readable names.
            if !result.winner_username.is_empty() {
                let _ = state
                    .db
                    .update_username(&result.winner_id, &result.winner_username);
            }
            if !result.loser_username.is_empty() {
                let _ = state
                    .db
                    .update_username(&result.loser_id, &result.loser_username);
            }

            // Apply accepted ranked results immediately so Discord summaries
            // and the leaderboard move after a completed match. The hourly
            // closer still exists as a fallback for inactivity/RD cleanup and
            // rows left pending after a restart.
            if let Err(e) = state.db.close_rating_period() {
                tracing::error!("Failed to close rating period after result: {e}");
            }
            let winner_rating = state.db.get_rating(&result.winner_id).unwrap_or_default();
            let loser_rating = state.db.get_rating(&result.loser_id).unwrap_or_default();

            // Resolve display names: request username > stored username > discord_id.
            let winner_display = if !result.winner_username.is_empty() {
                result.winner_username.clone()
            } else {
                state
                    .db
                    .get_username(&result.winner_id)
                    .unwrap_or_else(|| result.winner_id.clone())
            };
            let loser_display = if !result.loser_username.is_empty() {
                result.loser_username.clone()
            } else {
                state
                    .db
                    .get_username(&result.loser_id)
                    .unwrap_or_else(|| result.loser_id.clone())
            };

            // Fire-and-forget Discord notification.
            state.discord.notify_match(
                &winner_display,
                &loser_display,
                result.winner_score,
                result.loser_score,
                winner_rating.mu,
                loser_rating.mu,
            );

            tracing::info!(
                "Match recorded: {} beat {} {}:{} (room={})",
                result.winner_id,
                result.loser_id,
                result.winner_score,
                result.loser_score,
                result.room_id,
            );

            Ok(Json(serde_json::json!({ "ok": true })))
        }
        Err(e) => {
            tracing::error!("Failed to process match result: {e}");
            Err(StatusCode::INTERNAL_SERVER_ERROR)
        }
    }
}

// ── GET /leaderboard ──────────────────────────────────────────────────────

pub async fn leaderboard(
    State(state): State<AppState>,
    Query(q): Query<LeaderboardQuery>,
) -> Result<Json<LeaderboardResponse>, StatusCode> {
    let limit = q.limit.min(100).max(1);
    match state.db.get_leaderboard(limit) {
        Ok(entries) => Ok(Json(LeaderboardResponse { entries })),
        Err(e) => {
            tracing::error!("Leaderboard query failed: {e}");
            Err(StatusCode::INTERNAL_SERVER_ERROR)
        }
    }
}

// ── GET /player/:discord_id ───────────────────────────────────────────────

pub async fn player_profile(
    State(state): State<AppState>,
    Path(discord_id): Path<String>,
) -> Result<Json<PlayerProfile>, StatusCode> {
    match state.db.get_player(&discord_id) {
        Ok(Some(profile)) => Ok(Json(profile)),
        Ok(None) => Err(StatusCode::NOT_FOUND),
        Err(e) => {
            tracing::error!("Player profile query failed: {e}");
            Err(StatusCode::INTERNAL_SERVER_ERROR)
        }
    }
}

// ── GET /player/:discord_id/history ───────────────────────────────────────

pub async fn player_history(
    State(state): State<AppState>,
    Path(discord_id): Path<String>,
    Query(q): Query<HistoryQuery>,
) -> Result<Json<HistoryResponse>, StatusCode> {
    let limit = q.limit.min(100).max(1);
    match state.db.get_match_history(&discord_id, limit) {
        Ok(matches) => Ok(Json(HistoryResponse { matches })),
        Err(e) => {
            tracing::error!("Match history query failed: {e}");
            Err(StatusCode::INTERNAL_SERVER_ERROR)
        }
    }
}

// ── Username registry ─────────────────────────────────────────────────────
//
// No API key: clients hit these directly to claim/verify their display name.
// Claims are keyed by a self-asserted owner_id (discord_id or guest_device_id),
// which is acceptable because the worst case is a low-value name squat — the
// registry exists to stop accidental collisions, not to be an identity vault.

/// Server-side name validation, mirroring the client's `sanitize_username`:
/// 2-24 chars, ASCII alphanumerics plus `_`/`-`. Returns the trimmed name.
fn validate_name(raw: &str) -> Option<String> {
    let name = raw.trim();
    let len = name.chars().count();
    if !(2..=24).contains(&len) {
        return None;
    }
    if name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        Some(name.to_string())
    } else {
        None
    }
}

// POST /name/claim — atomically reserve a display name for an owner.
pub async fn claim_name(
    State(state): State<AppState>,
    Json(req): Json<NameClaimRequest>,
) -> Result<Json<NameClaimResponse>, StatusCode> {
    let Some(name) = validate_name(&req.name) else {
        return Err(StatusCode::BAD_REQUEST);
    };
    if req.owner_id.trim().is_empty() {
        return Err(StatusCode::BAD_REQUEST);
    }
    match state.db.claim_name(&name, req.owner_id.trim()) {
        Ok(outcome) => {
            let status = match outcome {
                crate::db::NameClaimOutcome::Claimed => "claimed",
                crate::db::NameClaimOutcome::Owned => "owned",
                crate::db::NameClaimOutcome::Taken => "taken",
            };
            Ok(Json(NameClaimResponse {
                status: status.to_string(),
                name,
            }))
        }
        Err(e) => {
            tracing::error!("name claim failed: {e}");
            Err(StatusCode::INTERNAL_SERVER_ERROR)
        }
    }
}

// GET /name/check/:name?owner_id=... — availability without reserving.
pub async fn check_name(
    State(state): State<AppState>,
    Path(name): Path<String>,
    Query(q): Query<NameCheckQuery>,
) -> Result<Json<NameCheckResponse>, StatusCode> {
    let Some(name) = validate_name(&name) else {
        return Err(StatusCode::BAD_REQUEST);
    };
    let owner = if q.owner_id.trim().is_empty() {
        None
    } else {
        Some(q.owner_id.trim())
    };
    match state.db.check_name(&name, owner) {
        Ok(o) => Ok(Json(NameCheckResponse {
            available: o.available,
            owned_by_you: o.owned_by_you,
        })),
        Err(e) => {
            tracing::error!("name check failed: {e}");
            Err(StatusCode::INTERNAL_SERVER_ERROR)
        }
    }
}

// ── POST /ghosts/upload ─────────────────────────────────────────────────────
//
// Binary POST with gzip-compressed .ncgh data. Metadata in HTTP headers:
//   X-Freeplay-Ghost-Id, X-Freeplay-Discord-Id, X-Freeplay-Username,
//   X-Freeplay-Rom-Hash, X-Freeplay-Filename, X-Freeplay-Frame-Count
// Content-Encoding: gzip
// No auth — ghosts are community content meant to be shared.

fn header_str<'a>(headers: &'a HeaderMap, key: &str) -> Option<&'a str> {
    headers.get(key)?.to_str().ok()
}

fn header_u16(headers: &HeaderMap, key: &str) -> Option<u16> {
    header_str(headers, key).and_then(|s| s.parse().ok())
}

fn header_u32(headers: &HeaderMap, key: &str) -> Option<u32> {
    header_str(headers, key).and_then(|s| s.parse().ok())
}

fn header_bool(headers: &HeaderMap, key: &str) -> bool {
    matches!(
        header_str(headers, key).map(|s| s.to_ascii_lowercase()),
        Some(s) if matches!(s.as_str(), "1" | "true" | "yes")
    )
}

fn clean_header_text(raw: &str, max_len: usize) -> String {
    raw.chars()
        .filter(|c| !c.is_control() && *c != '/' && *c != '\\')
        .take(max_len)
        .collect()
}

fn external_base_url(headers: &HeaderMap) -> String {
    let host = header_str(headers, "x-forwarded-host")
        .or_else(|| header_str(headers, "host"))
        .unwrap_or("localhost:8081");
    let scheme = header_str(headers, "x-forwarded-proto").unwrap_or_else(|| {
        if host.starts_with("localhost") || host.starts_with("127.0.0.1") {
            "http"
        } else {
            "https"
        }
    });
    format!("{scheme}://{host}")
}

/// Validate a ghost_id is safe to use as a filesystem name. Must be a
/// short token of safe characters — no dots, slashes, or control chars
/// that could escape `ghosts_dir` via path traversal. UUIDs and short
/// hex digests both pass.
fn is_safe_ghost_id(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 64
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

pub async fn upload_ghost(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<serde_json::Value>, StatusCode> {
    // Authenticate. The discord_id we record is the JWT's sub, NOT the
    // X-Freeplay-Discord-Id header — that header was previously trusted,
    // letting any client upload a ghost claiming any author. The header
    // is now ignored. Same for username.
    let claims = crate::auth::verify_jwt(&headers, state.config.jwt_secret.as_deref())?;
    let discord_id = claims.sub;
    let username = claims.username;

    let ghost_id = header_str(&headers, "x-freeplay-ghost-id")
        .unwrap_or_default()
        .to_string();
    let rom_hash = header_str(&headers, "x-freeplay-rom-hash")
        .unwrap_or_default()
        .to_string();
    let filename_raw = header_str(&headers, "x-freeplay-filename")
        .unwrap_or_default()
        .to_string();
    let frame_count: u32 = header_str(&headers, "x-freeplay-frame-count")
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);

    if !is_safe_ghost_id(&ghost_id) {
        tracing::warn!("[ghost] reject upload — invalid ghost_id={:?}", ghost_id);
        return Err(StatusCode::BAD_REQUEST);
    }
    if body.is_empty() {
        return Err(StatusCode::BAD_REQUEST);
    }
    // Reject obvious filename mischief; the on-disk path doesn't use this
    // string anyway (we name files <ghost_id>.ncgh.gz), but the value is
    // displayed in the ghost browser, so keep it human-printable.
    let filename: String = filename_raw
        .chars()
        .filter(|c| !c.is_control() && *c != '/' && *c != '\\')
        .take(80)
        .collect();

    let ghost_path = state.ghosts_dir.join(format!("{ghost_id}.ncgh.gz"));
    std::fs::write(&ghost_path, &body).map_err(|e| {
        tracing::error!("Failed to write ghost file {}: {e}", ghost_path.display());
        StatusCode::INTERNAL_SERVER_ERROR
    })?;

    state
        .db
        .upload_ghost_meta(
            &ghost_id,
            &discord_id,
            &username,
            &rom_hash,
            &filename,
            frame_count,
        )
        .map_err(|e| {
            tracing::error!("Ghost metadata insert failed: {e}");
            let _ = std::fs::remove_file(&ghost_path);
            StatusCode::INTERNAL_SERVER_ERROR
        })?;

    tracing::info!(
        "Ghost uploaded: {} by {} ({} frames, {} bytes)",
        filename,
        username,
        frame_count,
        body.len()
    );
    Ok(Json(serde_json::json!({ "ok": true })))
}

// ── GET /ghosts/list ────────────────────────────────────────────────────────

pub async fn list_ghosts(
    State(state): State<AppState>,
    Query(q): Query<GhostListQuery>,
) -> Result<Json<GhostListResponse>, StatusCode> {
    match state.db.list_ghosts(&q.rom_hash, q.limit) {
        Ok(ghosts) => Ok(Json(GhostListResponse { ghosts })),
        Err(e) => {
            tracing::error!("Ghost list failed: {e}");
            Err(StatusCode::INTERNAL_SERVER_ERROR)
        }
    }
}

// ── GET /ghosts/download/:ghost_id ──────────────────────────────────────────
//
// Returns the raw ghost file bytes. New uploads return gzip-compressed data
// with Content-Encoding: gzip. Old uploads (legacy SQLite BLOBs) return raw
// .ncgh. No auth — ghosts are public.

pub async fn download_ghost(
    State(state): State<AppState>,
    Path(ghost_id): Path<String>,
) -> Result<impl IntoResponse, StatusCode> {
    // Path traversal guard — db.download_ghost reads from
    // ghosts/<ghost_id>.ncgh.gz; an unsanitized input could escape the
    // ghosts directory.
    if !is_safe_ghost_id(&ghost_id) {
        return Err(StatusCode::BAD_REQUEST);
    }
    match state.db.download_ghost(&ghost_id, &state.ghosts_dir) {
        Ok(Some((filename, encoding, data))) => {
            let content_type = "application/octet-stream";
            let filename_header = format!("attachment; filename=\"{filename}\"");
            let mut response = axum::response::Response::builder()
                .header("Content-Type", content_type)
                .header("Content-Disposition", filename_header);
            if encoding == "gzip" {
                response = response.header("Content-Encoding", "gzip");
            }
            Ok(response.body(axum::body::Body::from(data)).unwrap())
        }
        Ok(None) => Err(StatusCode::NOT_FOUND),
        Err(e) => {
            tracing::error!("Ghost download failed: {e}");
            Err(StatusCode::INTERNAL_SERVER_ERROR)
        }
    }
}

// ── POST /replays/upload ───────────────────────────────────────────────────
//
// Binary POST with gzip-compressed .ncrp data. Metadata in HTTP headers:
//   X-Freeplay-Replay-Id, X-Freeplay-Rom-Hash, X-Freeplay-Filename,
//   X-Freeplay-P1-Name, X-Freeplay-P2-Name, X-Freeplay-P1-Score,
//   X-Freeplay-P2-Score, X-Freeplay-Winner, X-Freeplay-Frame-Count,
//   X-Freeplay-Duration, X-Freeplay-Recorded-At, X-Freeplay-Session-Id,
//   X-Freeplay-Completed-Games, X-Freeplay-Completed-Set
// Content-Encoding: gzip

pub async fn upload_replay(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let claims = crate::auth::verify_jwt(&headers, state.config.jwt_secret.as_deref())?;
    let discord_id = claims.sub;
    let username = claims.username;

    let replay_id = header_str(&headers, "x-freeplay-replay-id")
        .unwrap_or_default()
        .to_string();
    let rom_hash = header_str(&headers, "x-freeplay-rom-hash")
        .unwrap_or_default()
        .to_string();
    let filename_raw = header_str(&headers, "x-freeplay-filename").unwrap_or("replay.ncrp");

    if !is_safe_ghost_id(&replay_id) {
        tracing::warn!("[replay] reject upload - invalid replay_id={:?}", replay_id);
        return Err(StatusCode::BAD_REQUEST);
    }
    if body.is_empty() {
        return Err(StatusCode::BAD_REQUEST);
    }

    let filename = clean_header_text(filename_raw, 96);
    let replay = ReplayMetaInsert {
        replay_id: replay_id.clone(),
        discord_id,
        username,
        rom_hash: clean_header_text(&rom_hash, 64),
        filename: if filename.is_empty() {
            "replay.ncrp".into()
        } else {
            filename
        },
        p1_name: clean_header_text(
            header_str(&headers, "x-freeplay-p1-name").unwrap_or("P1"),
            48,
        ),
        p2_name: clean_header_text(
            header_str(&headers, "x-freeplay-p2-name").unwrap_or("P2"),
            48,
        ),
        p1_score: header_u16(&headers, "x-freeplay-p1-score"),
        p2_score: header_u16(&headers, "x-freeplay-p2-score"),
        winner: clean_header_text(header_str(&headers, "x-freeplay-winner").unwrap_or(""), 48),
        frame_count: header_u32(&headers, "x-freeplay-frame-count").unwrap_or(0),
        duration: clean_header_text(
            header_str(&headers, "x-freeplay-duration").unwrap_or(""),
            32,
        ),
        recorded_at: clean_header_text(
            header_str(&headers, "x-freeplay-recorded-at")
                .or_else(|| header_str(&headers, "x-freeplay-recorded-unix"))
                .unwrap_or(""),
            64,
        ),
        session_id: clean_header_text(
            header_str(&headers, "x-freeplay-session-id").unwrap_or(""),
            96,
        ),
        completed_games: header_u32(&headers, "x-freeplay-completed-games").unwrap_or(0),
        completed_set: header_bool(&headers, "x-freeplay-completed-set"),
    };

    let replay_path = state.replays_dir.join(format!("{replay_id}.ncrp.gz"));
    std::fs::write(&replay_path, &body).map_err(|e| {
        tracing::error!("Failed to write replay file {}: {e}", replay_path.display());
        StatusCode::INTERNAL_SERVER_ERROR
    })?;

    state.db.upload_replay_meta(&replay).map_err(|e| {
        tracing::error!("Replay metadata insert failed: {e}");
        let _ = std::fs::remove_file(&replay_path);
        StatusCode::INTERNAL_SERVER_ERROR
    })?;

    tracing::info!(
        "Replay uploaded: {} by {} ({} frames, {} bytes)",
        replay.filename,
        replay.username,
        replay.frame_count,
        body.len()
    );
    Ok(Json(serde_json::json!({ "ok": true })))
}

// ── GET /replays/list ──────────────────────────────────────────────────────

pub async fn list_replays(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<ReplayListQuery>,
) -> Result<Json<ReplayListResponse>, StatusCode> {
    let base_url = external_base_url(&headers);
    match state.db.list_replays(&q.rom_hash, q.limit) {
        Ok(mut replays) => {
            for replay in &mut replays {
                replay.url = format!("{base_url}/replays/download/{}", replay.replay_id);
            }
            Ok(Json(ReplayListResponse { replays }))
        }
        Err(e) => {
            tracing::error!("Replay list failed: {e}");
            Err(StatusCode::INTERNAL_SERVER_ERROR)
        }
    }
}

// ── GET /replays/download/:replay_id ───────────────────────────────────────

pub async fn download_replay(
    State(state): State<AppState>,
    Path(replay_id): Path<String>,
) -> Result<impl IntoResponse, StatusCode> {
    if !is_safe_ghost_id(&replay_id) {
        return Err(StatusCode::BAD_REQUEST);
    }
    let filename = match state.db.replay_filename(&replay_id) {
        Ok(Some(filename)) => filename,
        Ok(None) => return Err(StatusCode::NOT_FOUND),
        Err(e) => {
            tracing::error!("Replay metadata lookup failed: {e}");
            return Err(StatusCode::INTERNAL_SERVER_ERROR);
        }
    };
    let replay_path = state.replays_dir.join(format!("{replay_id}.ncrp.gz"));
    let data = std::fs::read(&replay_path).map_err(|e| {
        tracing::error!("Replay file read failed {}: {e}", replay_path.display());
        StatusCode::NOT_FOUND
    })?;
    let filename_header = format!("attachment; filename=\"{filename}\"");
    Ok(axum::response::Response::builder()
        .header("Content-Type", "application/octet-stream")
        .header("Content-Encoding", "gzip")
        .header("Content-Disposition", filename_header)
        .body(axum::body::Body::from(data))
        .unwrap())
}
