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

fn default_ghost_limit() -> u32 { 50 }
