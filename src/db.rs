use rusqlite::{Connection, params};
use std::sync::Mutex;
use crate::glicko::{self, PeriodResult, Rating};

pub struct Db {
    conn: Mutex<Connection>,
}

impl Db {
    pub fn open(path: &str) -> anyhow::Result<Self> {
        let conn = Connection::open(path)?;
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON;")?;
        let db = Self { conn: Mutex::new(conn) };
        db.migrate()?;
        Ok(db)
    }

    fn migrate(&self) -> anyhow::Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS players (
                discord_id TEXT PRIMARY KEY,
                username   TEXT NOT NULL DEFAULT '',
                mu         REAL NOT NULL DEFAULT 1500.0,
                phi        REAL NOT NULL DEFAULT 350.0,
                sigma      REAL NOT NULL DEFAULT 0.06,
                wins       INTEGER NOT NULL DEFAULT 0,
                losses     INTEGER NOT NULL DEFAULT 0,
                draws      INTEGER NOT NULL DEFAULT 0,
                updated_at TEXT NOT NULL DEFAULT ''
            );

            CREATE TABLE IF NOT EXISTS matches (
                id            INTEGER PRIMARY KEY AUTOINCREMENT,
                room_id       TEXT NOT NULL UNIQUE,
                winner_id     TEXT NOT NULL REFERENCES players(discord_id),
                loser_id      TEXT NOT NULL REFERENCES players(discord_id),
                winner_score  INTEGER NOT NULL,
                loser_score   INTEGER NOT NULL,
                rom_hash      TEXT NOT NULL DEFAULT '',
                played_at     TEXT NOT NULL,
                /// Rating-period close timestamp at which this match's
                /// ratings were applied. NULL = still pending. The periodic
                /// closer aggregates rows with NULL into a Glicko-2 batch.
                applied_at    TEXT
            );

            -- Add applied_at to legacy DBs that pre-date the periodized
            -- Glicko-2 work. Idempotent: SQLite errors if the column already
            -- exists, which we swallow because process_result only ever
            -- inserts NULL after this migration.
            CREATE INDEX IF NOT EXISTS idx_matches_winner ON matches(winner_id);
            CREATE INDEX IF NOT EXISTS idx_matches_loser  ON matches(loser_id);
            CREATE INDEX IF NOT EXISTS idx_matches_played  ON matches(played_at);
            CREATE INDEX IF NOT EXISTS idx_matches_applied ON matches(applied_at);

            CREATE TABLE IF NOT EXISTS rating_periods (
                id          INTEGER PRIMARY KEY AUTOINCREMENT,
                closed_at   TEXT NOT NULL,
                match_count INTEGER NOT NULL,
                player_count INTEGER NOT NULL
            );

            CREATE TABLE IF NOT EXISTS ghosts (
                id          INTEGER PRIMARY KEY AUTOINCREMENT,
                ghost_id    TEXT NOT NULL UNIQUE,
                discord_id  TEXT NOT NULL,
                username    TEXT NOT NULL DEFAULT '',
                rom_hash    TEXT NOT NULL DEFAULT '',
                filename    TEXT NOT NULL,
                file_data   BLOB NOT NULL,
                frame_count INTEGER NOT NULL DEFAULT 0,
                uploaded_at TEXT NOT NULL DEFAULT ''
            );

            CREATE INDEX IF NOT EXISTS idx_ghosts_rom    ON ghosts(rom_hash);
            CREATE INDEX IF NOT EXISTS idx_ghosts_upload ON ghosts(uploaded_at DESC);
            ",
        )?;
        // Legacy DBs predating the periodized Glicko-2 work won't have the
        // applied_at column. Add it if missing. SQLite will error if it
        // already exists; we treat that as success.
        let _ = conn.execute("ALTER TABLE matches ADD COLUMN applied_at TEXT", []);
        Ok(())
    }

    /// Record a match result. Does NOT apply Glicko-2 here — instead the
    /// match is queued for the next `close_rating_period` call. Per
    /// Glickman's paper, Glicko-2 only produces correct rating dynamics
    /// when matches are batched into rating periods. Per-match updates
    /// (the previous behavior) overweight active streaks and underweight
    /// consistent play.
    ///
    /// Win/loss/draw counters are still bumped immediately so the live
    /// leaderboard reflects accurate W/L without waiting for the period
    /// closer. The `mu`/`phi`/`sigma` columns lag by up to one period.
    ///
    /// Returns the players' *current* ratings (i.e. as of the last
    /// closed period). The match is recorded with `applied_at = NULL`,
    /// signaling it's pending for the next batch.
    pub fn process_result(
        &self,
        winner_id: &str,
        loser_id: &str,
        winner_score: u16,
        loser_score: u16,
        room_id: &str,
        rom_hash: &str,
    ) -> anyhow::Result<(Rating, Rating)> {
        let conn = self.conn.lock().unwrap();

        // Upsert players.
        conn.execute(
            "INSERT OR IGNORE INTO players (discord_id) VALUES (?1)",
            params![winner_id],
        )?;
        conn.execute(
            "INSERT OR IGNORE INTO players (discord_id) VALUES (?1)",
            params![loser_id],
        )?;

        // Insert match — applied_at NULL means "pending, will be picked
        // up by the next rating-period close". INSERT OR IGNORE on
        // room_id ensures duplicate /results posts can't double-count.
        let now = chrono::Utc::now().to_rfc3339();
        let inserted = conn.execute(
            "INSERT OR IGNORE INTO matches
             (room_id, winner_id, loser_id, winner_score, loser_score, rom_hash, played_at, applied_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, NULL)",
            params![room_id, winner_id, loser_id, winner_score, loser_score, rom_hash, now],
        )?;

        // Only bump W/L if this row was newly inserted (not a duplicate).
        if inserted > 0 {
            conn.execute(
                "UPDATE players SET wins = wins + 1, updated_at = ?1 WHERE discord_id = ?2",
                params![now, winner_id],
            )?;
            conn.execute(
                "UPDATE players SET losses = losses + 1, updated_at = ?1 WHERE discord_id = ?2",
                params![now, loser_id],
            )?;
        }

        // Read back current (last-period) ratings for the response.
        let winner_rating = read_rating(&conn, winner_id)?;
        let loser_rating = read_rating(&conn, loser_id)?;
        Ok((winner_rating, loser_rating))
    }

    /// Close a rating period. Aggregates all pending matches per player,
    /// applies Glicko-2 batched updates, and inflates RD on inactive
    /// players (per Glickman §3.1 step 6 for non-competitors). Returns
    /// (matches_applied, players_updated).
    ///
    /// Idempotent: matches already marked `applied_at` are skipped.
    /// Designed to be called by a periodic task (default: every hour).
    pub fn close_rating_period(&self) -> anyhow::Result<(usize, usize)> {
        let conn = self.conn.lock().unwrap();
        let now = chrono::Utc::now().to_rfc3339();

        // Snapshot every player's pre-period rating once. We need a
        // consistent view because each player's results in this period
        // reference opponents' pre-period ratings, not running ones.
        let mut stmt = conn.prepare(
            "SELECT discord_id, mu, phi, sigma FROM players"
        )?;
        let pre_period: std::collections::HashMap<String, Rating> = stmt
            .query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    Rating {
                        mu:    r.get(1)?,
                        phi:   r.get(2)?,
                        sigma: r.get(3)?,
                    },
                ))
            })?
            .collect::<Result<_, _>>()?;
        drop(stmt);

        // Pull all pending matches once.
        let mut stmt = conn.prepare(
            "SELECT id, winner_id, loser_id, winner_score, loser_score
             FROM matches WHERE applied_at IS NULL"
        )?;
        let pending: Vec<(i64, String, String, u16, u16)> = stmt
            .query_map([], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
            })?
            .collect::<Result<_, _>>()?;
        drop(stmt);

        // Bucket per player.
        let mut by_player: std::collections::HashMap<String, Vec<PeriodResult>> =
            std::collections::HashMap::new();
        for (_id, winner_id, loser_id, _ws, _ls) in &pending {
            let winner_pre = pre_period.get(winner_id).cloned().unwrap_or_default();
            let loser_pre  = pre_period.get(loser_id).cloned().unwrap_or_default();
            by_player.entry(winner_id.clone()).or_default().push(PeriodResult {
                opponent_mu:  loser_pre.mu,
                opponent_phi: loser_pre.phi,
                score: 1.0,
            });
            by_player.entry(loser_id.clone()).or_default().push(PeriodResult {
                opponent_mu:  winner_pre.mu,
                opponent_phi: winner_pre.phi,
                score: 0.0,
            });
        }

        // Apply Glicko-2 to active players, RD-inflate inactives.
        let mut updated_count = 0usize;
        let active_set: std::collections::HashSet<&String> = by_player.keys().collect();
        for (discord_id, mut rating) in pre_period.iter().map(|(k, v)| (k.clone(), v.clone())) {
            let needs_write = if let Some(results) = by_player.get(&discord_id) {
                glicko::update_player_with_results(&mut rating, results);
                true
            } else if !active_set.contains(&discord_id) {
                // Tighten phi inflation: only meaningful if any phi growth
                // would occur. For a brand-new player at default phi=350,
                // continued inflation is silly (already maximal-uncertainty),
                // so cap inflation at 350. No-op for a player at exactly 350.
                let before = rating.phi;
                glicko::decay_inactive_player(&mut rating);
                if rating.phi > 350.0 {
                    rating.phi = 350.0;
                }
                // Still write if RD changed measurably.
                rating.phi != before
            } else {
                false
            };
            if needs_write {
                conn.execute(
                    "UPDATE players SET mu=?1, phi=?2, sigma=?3, updated_at=?4 WHERE discord_id=?5",
                    params![rating.mu, rating.phi, rating.sigma, now, discord_id],
                )?;
                updated_count += 1;
            }
        }

        // Mark pending matches as applied.
        for (id, _, _, _, _) in &pending {
            conn.execute(
                "UPDATE matches SET applied_at = ?1 WHERE id = ?2",
                params![now, id],
            )?;
        }

        // Audit row.
        conn.execute(
            "INSERT INTO rating_periods (closed_at, match_count, player_count)
             VALUES (?1, ?2, ?3)",
            params![now, pending.len() as i64, updated_count as i64],
        )?;

        Ok((pending.len(), updated_count))
    }

    pub fn get_player(&self, discord_id: &str) -> anyhow::Result<Option<crate::models::PlayerProfile>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT discord_id, username, mu, phi, sigma, wins, losses, draws FROM players WHERE discord_id = ?1"
        )?;
        let mut rows = stmt.query_map(params![discord_id], |r| {
            let wins: u64 = r.get(5)?;
            let losses: u64 = r.get(6)?;
            let draws: u64 = r.get(7)?;
            Ok(crate::models::PlayerProfile {
                discord_id: r.get(0)?,
                username: r.get(1)?,
                rating: r.get(2)?,
                deviation: r.get(3)?,
                volatility: r.get(4)?,
                wins,
                losses,
                draws,
                matches_played: wins + losses + draws,
            })
        })?;
        Ok(rows.next().transpose()?)
    }

    pub fn get_leaderboard(&self, limit: u32) -> anyhow::Result<Vec<crate::models::LeaderboardEntry>> {
        // Require N placement matches before appearing publicly. Standard
        // ranked-ladder hygiene: a single lucky win against a 1500-rated
        // opponent would otherwise put a player on top with provisional
        // rating + huge RD, which is misleading on a community board.
        const MIN_PLACEMENT_MATCHES: u32 = 5;
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT discord_id, username, mu, wins, losses, draws
             FROM players
             WHERE wins + losses + draws >= ?1
             ORDER BY mu DESC
             LIMIT ?2"
        )?;
        let entries = stmt.query_map(params![MIN_PLACEMENT_MATCHES, limit], |r| {
            let wins: u64 = r.get(3)?;
            let losses: u64 = r.get(4)?;
            let draws: u64 = r.get(5)?;
            Ok(crate::models::LeaderboardEntry {
                rank: 0, // filled in later
                discord_id: r.get(0)?,
                username: r.get(1)?,
                rating: r.get(2)?,
                wins,
                losses,
                matches_played: wins + losses + draws,
            })
        })?.collect::<Result<Vec<_>, _>>()?;

        let ranked: Vec<_> = entries.into_iter().enumerate().map(|(i, mut e)| {
            e.rank = (i + 1) as u64;
            e
        }).collect();
        Ok(ranked)
    }

    pub fn get_match_history(&self, discord_id: &str, limit: u32) -> anyhow::Result<Vec<crate::models::MatchHistoryEntry>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT room_id, winner_id, loser_id, winner_score, loser_score, played_at
             FROM matches
             WHERE winner_id = ?1 OR loser_id = ?1
             ORDER BY played_at DESC
             LIMIT ?2"
        )?;
        let entries = stmt.query_map(params![discord_id, limit], |r| {
            let winner: String = r.get(1)?;
            let loser: String = r.get(2)?;
            let w_score: u16 = r.get(3)?;
            let l_score: u16 = r.get(4)?;
            let played: String = r.get(5)?;

            let (result, our_score, opp_score, opp_id) = if winner == discord_id {
                ("win".to_string(), w_score, l_score, loser.clone())
            } else {
                ("loss".to_string(), l_score, w_score, winner.clone())
            };

            // Look up opponent username.
            let opp_username = conn.query_row(
                "SELECT username FROM players WHERE discord_id = ?1",
                params![opp_id],
                |r| r.get(0),
            ).unwrap_or_default();

            Ok(crate::models::MatchHistoryEntry {
                room_id: r.get(0)?,
                opponent_id: opp_id,
                opponent_username: opp_username,
                result,
                our_score,
                opponent_score: opp_score,
                played_at: played,
            })
        })?.collect::<Result<Vec<_>, _>>()?;

        Ok(entries)
    }

    pub fn get_username(&self, discord_id: &str) -> Option<String> {
        let conn = self.conn.lock().unwrap();
        conn.query_row(
            "SELECT username FROM players WHERE discord_id = ?1",
            params![discord_id],
            |r| r.get(0),
        ).ok().filter(|s: &String| !s.is_empty())
    }

    pub fn update_username(&self, discord_id: &str, username: &str) -> anyhow::Result<()> {
        // Refresh the cached display name. Previously we only wrote when the
        // existing value was empty, which froze a player's leaderboard name to
        // their first-ever match's username and never picked up Discord
        // username changes afterwards.
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE players SET username = ?1 WHERE discord_id = ?2",
            params![username, discord_id],
        )?;
        Ok(())
    }

    // ── Ghost file operations ───────────────────────────────────────────────

    /// Store ghost metadata in SQLite. The actual compressed file is written
    /// to `ghosts/<ghost_id>.ncgh.gz` on the GCS-FUSE mount by the handler.
    pub fn upload_ghost_meta(
        &self,
        ghost_id: &str,
        discord_id: &str,
        username: &str,
        rom_hash: &str,
        filename: &str,
        frame_count: u32,
    ) -> anyhow::Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT OR REPLACE INTO ghosts (ghost_id, discord_id, username, rom_hash, filename, file_data, frame_count, uploaded_at)
             VALUES (?1, ?2, ?3, ?4, ?5, X'', ?6, datetime('now'))",
            params![ghost_id, discord_id, username, rom_hash, filename, frame_count],
        )?;
        Ok(())
    }

    /// Read a ghost file. Tries filesystem first (new gzip'd format at
    /// `ghosts/<ghost_id>.ncgh.gz`), then falls back to the SQLite BLOB
    /// (old base64-encoded uploads). Returns the raw bytes (still gzip'd
    /// for new uploads, raw .ncgh for old).
    pub fn download_ghost(&self, ghost_id: &str) -> anyhow::Result<Option<(String, String, Vec<u8>)>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT filename, file_data FROM ghosts WHERE ghost_id = ?1"
        )?;
        let mut rows = stmt.query_map(params![ghost_id], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, Vec<u8>>(1)?))
        })?;
        let (filename, blob) = match rows.next().transpose()? {
            Some(v) => v,
            None => return Ok(None),
        };
        let fs_path = format!("ghosts/{ghost_id}.ncgh.gz");
        if let Ok(data) = std::fs::read(&fs_path) {
            return Ok(Some((filename, "gzip".into(), data)));
        }
        if !blob.is_empty() {
            Ok(Some((filename, "raw".into(), blob)))
        } else {
            Ok(None)
        }
    }

    pub fn list_ghosts(&self, rom_hash: &str, limit: u32) -> anyhow::Result<Vec<crate::models::GhostEntry>> {
        let conn = self.conn.lock().unwrap();
        let limit = limit.min(100).max(1) as i64;

        let (sql, params_vec): (&str, Vec<Box<dyn rusqlite::types::ToSql>>) = if rom_hash.is_empty() {
            ("SELECT ghost_id, discord_id, username, rom_hash, filename, frame_count, uploaded_at
              FROM ghosts ORDER BY uploaded_at DESC LIMIT ?1",
             vec![Box::new(limit) as Box<dyn rusqlite::types::ToSql>])
        } else {
            ("SELECT ghost_id, discord_id, username, rom_hash, filename, frame_count, uploaded_at
              FROM ghosts WHERE rom_hash = ?1 ORDER BY uploaded_at DESC LIMIT ?2",
             vec![Box::new(rom_hash.to_string()), Box::new(limit)])
        };

        let params_refs: Vec<&dyn rusqlite::types::ToSql> = params_vec.iter().map(|p| p.as_ref()).collect();
        let mut stmt = conn.prepare(sql)?;
        let rows = stmt.query_map(params_refs.as_slice(), |row| {
            Ok(crate::models::GhostEntry {
                ghost_id: row.get(0)?,
                discord_id: row.get(1)?,
                username: row.get(2)?,
                rom_hash: row.get(3)?,
                filename: row.get(4)?,
                frame_count: row.get(5)?,
                uploaded_at: row.get(6)?,
            })
        })?;

        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }
}

fn read_rating(conn: &Connection, discord_id: &str) -> anyhow::Result<Rating> {
    Ok(conn.query_row(
        "SELECT mu, phi, sigma FROM players WHERE discord_id = ?1",
        params![discord_id],
        |r| Ok(Rating { mu: r.get(0)?, phi: r.get(1)?, sigma: r.get(2)? }),
    )?)
}
