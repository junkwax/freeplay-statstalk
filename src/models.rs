use serde::{Deserialize, Serialize};

// ── Inbound from signaling server ──────────────────────────────────────────

#[derive(Debug, Clone, Deserialize)]
pub struct MatchResult {
    pub room_id: String,
    pub winner_id: String,
    pub loser_id: String,
    pub winner_score: u16,
    pub loser_score: u16,
    pub rom_hash: String,
    #[serde(default)]
    pub winner_username: String,
    #[serde(default)]
    pub loser_username: String,
}

// ── API responses ──────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize)]
pub struct PlayerProfile {
    pub discord_id: String,
    pub username: String,
    pub rating: f64,
    pub deviation: f64,
    pub volatility: f64,
    pub wins: u64,
    pub losses: u64,
    pub draws: u64,
    pub matches_played: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct LeaderboardEntry {
    pub rank: u64,
    pub discord_id: String,
    pub username: String,
    pub rating: f64,
    pub wins: u64,
    pub losses: u64,
    pub matches_played: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct MatchHistoryEntry {
    pub room_id: String,
    pub opponent_id: String,
    pub opponent_username: String,
    pub result: String,
    pub our_score: u16,
    pub opponent_score: u16,
    pub played_at: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct LeaderboardResponse {
    pub entries: Vec<LeaderboardEntry>,
}

#[derive(Debug, Clone, Serialize)]
pub struct HistoryResponse {
    pub matches: Vec<MatchHistoryEntry>,
}

// ── Ghost file storage ─────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize)]
pub struct GhostEntry {
    pub ghost_id: String,
    pub discord_id: String,
    pub username: String,
    pub rom_hash: String,
    pub filename: String,
    pub frame_count: u32,
    pub uploaded_at: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct GhostListResponse {
    pub ghosts: Vec<GhostEntry>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct GhostListQuery {
    #[serde(default)]
    pub rom_hash: String,
    #[serde(default = "default_ghost_limit")]
    pub limit: u32,
}

fn default_ghost_limit() -> u32 {
    50
}

// ── Full match replay storage ──────────────────────────────────────────────

#[derive(Debug, Clone, Serialize)]
pub struct ReplayEntry {
    pub replay_id: String,
    pub discord_id: String,
    pub username: String,
    pub rom_hash: String,
    #[serde(rename = "file")]
    pub filename: String,
    pub url: String,
    #[serde(rename = "p1")]
    pub p1_name: String,
    #[serde(rename = "p2")]
    pub p2_name: String,
    pub p1_score: Option<u16>,
    pub p2_score: Option<u16>,
    pub winner: String,
    #[serde(rename = "frames")]
    pub frame_count: u32,
    pub duration: String,
    pub recorded_at: String,
    pub session_id: String,
    pub completed_games: u32,
    pub completed_set: bool,
    pub uploaded_at: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct ReplayListResponse {
    pub replays: Vec<ReplayEntry>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ReplayListQuery {
    #[serde(default)]
    pub rom_hash: String,
    #[serde(default = "default_replay_limit")]
    pub limit: u32,
}

#[derive(Debug, Clone)]
pub struct ReplayMetaInsert {
    pub replay_id: String,
    pub discord_id: String,
    pub username: String,
    pub rom_hash: String,
    pub filename: String,
    pub p1_name: String,
    pub p2_name: String,
    pub p1_score: Option<u16>,
    pub p2_score: Option<u16>,
    pub winner: String,
    pub frame_count: u32,
    pub duration: String,
    pub recorded_at: String,
    pub session_id: String,
    pub completed_games: u32,
    pub completed_set: bool,
}

fn default_replay_limit() -> u32 {
    50
}
