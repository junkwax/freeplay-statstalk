use crate::glicko::{self, PeriodResult, Rating};
use rusqlite::{params, Connection};
use std::sync::Mutex;

pub struct Db {
    conn: Mutex<Connection>,
}

impl Db {
    pub fn open(path: &str) -> anyhow::Result<Self> {
        let conn = Connection::open(path)?;
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON;")?;
        let db = Self {
            conn: Mutex::new(conn),
        };
        db.migrate()?;
        Ok(db)
    }

    fn migrate(&self) -> anyhow::Result<()> {
        let conn = self.conn.lock().unwrap();
        // Step 1: create base tables (no applied_at-dependent index yet).
        // CREATE IF NOT EXISTS is the legacy-safe form; an existing
        // production DB will not be re-created here.
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
                applied_at    TEXT
            );

            CREATE INDEX IF NOT EXISTS idx_matches_winner ON matches(winner_id);
            CREATE INDEX IF NOT EXISTS idx_matches_loser  ON matches(loser_id);
            CREATE INDEX IF NOT EXISTS idx_matches_played ON matches(played_at);

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

            CREATE TABLE IF NOT EXISTS replays (
                id              INTEGER PRIMARY KEY AUTOINCREMENT,
                replay_id       TEXT NOT NULL UNIQUE,
                discord_id      TEXT NOT NULL,
                username        TEXT NOT NULL DEFAULT '',
                rom_hash        TEXT NOT NULL DEFAULT '',
                filename        TEXT NOT NULL,
                p1_name         TEXT NOT NULL DEFAULT '',
                p2_name         TEXT NOT NULL DEFAULT '',
                p1_score        INTEGER,
                p2_score        INTEGER,
                winner          TEXT NOT NULL DEFAULT '',
                frame_count     INTEGER NOT NULL DEFAULT 0,
                duration        TEXT NOT NULL DEFAULT '',
                recorded_at     TEXT NOT NULL DEFAULT '',
                session_id      TEXT NOT NULL DEFAULT '',
                completed_games INTEGER NOT NULL DEFAULT 0,
                completed_set   INTEGER NOT NULL DEFAULT 0,
                uploaded_at     TEXT NOT NULL DEFAULT ''
            );

            CREATE INDEX IF NOT EXISTS idx_replays_rom    ON replays(rom_hash);
            CREATE INDEX IF NOT EXISTS idx_replays_upload ON replays(uploaded_at DESC);

            CREATE TABLE IF NOT EXISTS name_registry (
                name_norm  TEXT PRIMARY KEY,
                name       TEXT NOT NULL,
                owner_id   TEXT NOT NULL,
                claimed_at TEXT NOT NULL,
                updated_at TEXT NOT NULL
            );

            CREATE INDEX IF NOT EXISTS idx_name_registry_owner ON name_registry(owner_id);
            ",
        )?;

        // Step 2: legacy DBs that pre-date the periodized Glicko-2 work
        // already have a `matches` table without applied_at. Add the column
        // if missing. SQLite errors if it already exists — that's our signal
        // it's fresh and we move on.
        let _ = conn.execute("ALTER TABLE matches ADD COLUMN applied_at TEXT", []);

        // Step 3: now the column is guaranteed present; create the dependent
        // index. Doing this AFTER the ALTER avoids the bootstrap order bug
        // where execute_batch sees a CREATE INDEX referencing a column the
        // legacy schema doesn't have yet.
        conn.execute(
            "CREATE INDEX IF NOT EXISTS idx_matches_applied ON matches(applied_at)",
            [],
        )?;

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
        let mut stmt = conn.prepare("SELECT discord_id, mu, phi, sigma FROM players")?;
        let pre_period: std::collections::HashMap<String, Rating> = stmt
            .query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    Rating {
                        mu: r.get(1)?,
                        phi: r.get(2)?,
                        sigma: r.get(3)?,
                    },
                ))
            })?
            .collect::<Result<_, _>>()?;
        drop(stmt);

        // Pull all pending matches once.
        let mut stmt = conn.prepare(
            "SELECT id, winner_id, loser_id, winner_score, loser_score
             FROM matches WHERE applied_at IS NULL",
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
            let loser_pre = pre_period.get(loser_id).cloned().unwrap_or_default();
            by_player
                .entry(winner_id.clone())
                .or_default()
                .push(PeriodResult {
                    opponent_mu: loser_pre.mu,
                    opponent_phi: loser_pre.phi,
                    score: 1.0,
                });
            by_player
                .entry(loser_id.clone())
                .or_default()
                .push(PeriodResult {
                    opponent_mu: winner_pre.mu,
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

    pub fn get_player(
        &self,
        discord_id: &str,
    ) -> anyhow::Result<Option<crate::models::PlayerProfile>> {
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

    pub fn get_rating(&self, discord_id: &str) -> anyhow::Result<Rating> {
        let conn = self.conn.lock().unwrap();
        read_rating(&conn, discord_id)
    }

    pub fn get_leaderboard(
        &self,
        limit: u32,
    ) -> anyhow::Result<Vec<crate::models::LeaderboardEntry>> {
        // Show players as soon as they have a recorded result. Guest players
        // without a stats email still have a stable guest-device identity, so
        // hiding them behind a placement gate makes their rank look broken.
        const MIN_PLACEMENT_MATCHES: u32 = 1;
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT discord_id, username, mu, wins, losses, draws
             FROM players
             WHERE wins + losses + draws >= ?1
             ORDER BY mu DESC
             LIMIT ?2",
        )?;
        let entries = stmt
            .query_map(params![MIN_PLACEMENT_MATCHES, limit], |r| {
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
            })?
            .collect::<Result<Vec<_>, _>>()?;

        let ranked: Vec<_> = entries
            .into_iter()
            .enumerate()
            .map(|(i, mut e)| {
                e.rank = (i + 1) as u64;
                e
            })
            .collect();
        Ok(ranked)
    }

    pub fn get_match_history(
        &self,
        discord_id: &str,
        limit: u32,
    ) -> anyhow::Result<Vec<crate::models::MatchHistoryEntry>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT room_id, winner_id, loser_id, winner_score, loser_score, played_at
             FROM matches
             WHERE winner_id = ?1 OR loser_id = ?1
             ORDER BY played_at DESC
             LIMIT ?2",
        )?;
        let entries = stmt
            .query_map(params![discord_id, limit], |r| {
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
                let opp_username = conn
                    .query_row(
                        "SELECT username FROM players WHERE discord_id = ?1",
                        params![opp_id],
                        |r| r.get(0),
                    )
                    .unwrap_or_default();

                Ok(crate::models::MatchHistoryEntry {
                    room_id: r.get(0)?,
                    opponent_id: opp_id,
                    opponent_username: opp_username,
                    result,
                    our_score,
                    opponent_score: opp_score,
                    played_at: played,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;

        Ok(entries)
    }

    // ── Username registry (true global reservation) ─────────────────────────

    /// Atomically reserve `name` for `owner_id`. Names are unique
    /// case-insensitively. An owner holds exactly one name: claiming a new one
    /// releases their previous reservation (rename). Returns the outcome.
    pub fn claim_name(&self, name: &str, owner_id: &str) -> anyhow::Result<NameClaimOutcome> {
        let norm = name.trim().to_lowercase();
        let now = chrono::Utc::now().to_rfc3339();
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;

        let existing: Option<String> = tx
            .query_row(
                "SELECT owner_id FROM name_registry WHERE name_norm = ?1",
                params![norm],
                |r| r.get(0),
            )
            .ok();

        let outcome = match existing {
            Some(owner) if owner == owner_id => {
                // Already yours — refresh the display spelling and timestamp.
                tx.execute(
                    "UPDATE name_registry SET name = ?1, updated_at = ?2 WHERE name_norm = ?3",
                    params![name, now, norm],
                )?;
                NameClaimOutcome::Owned
            }
            Some(_) => NameClaimOutcome::Taken,
            None => {
                // Release any name this owner previously held (rename), then claim.
                tx.execute(
                    "DELETE FROM name_registry WHERE owner_id = ?1",
                    params![owner_id],
                )?;
                tx.execute(
                    "INSERT INTO name_registry (name_norm, name, owner_id, claimed_at, updated_at)
                     VALUES (?1, ?2, ?3, ?4, ?4)",
                    params![norm, name, owner_id, now],
                )?;
                NameClaimOutcome::Claimed
            }
        };
        tx.commit()?;
        Ok(outcome)
    }

    /// Is `name` available? Available means unclaimed, or already owned by
    /// `owner_id` (so re-checking your own name reads as available).
    pub fn check_name(&self, name: &str, owner_id: Option<&str>) -> anyhow::Result<NameCheckOutcome> {
        let norm = name.trim().to_lowercase();
        let conn = self.conn.lock().unwrap();
        let owner: Option<String> = conn
            .query_row(
                "SELECT owner_id FROM name_registry WHERE name_norm = ?1",
                params![norm],
                |r| r.get(0),
            )
            .ok();
        Ok(match owner {
            None => NameCheckOutcome {
                available: true,
                owned_by_you: false,
            },
            Some(o) => {
                let mine = owner_id.map(|id| id == o).unwrap_or(false);
                NameCheckOutcome {
                    available: mine,
                    owned_by_you: mine,
                }
            }
        })
    }

    pub fn get_username(&self, discord_id: &str) -> Option<String> {
        let conn = self.conn.lock().unwrap();
        conn.query_row(
            "SELECT username FROM players WHERE discord_id = ?1",
            params![discord_id],
            |r| r.get(0),
        )
        .ok()
        .filter(|s: &String| !s.is_empty())
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
    pub fn download_ghost(
        &self,
        ghost_id: &str,
    ) -> anyhow::Result<Option<(String, String, Vec<u8>)>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt =
            conn.prepare("SELECT filename, file_data FROM ghosts WHERE ghost_id = ?1")?;
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

    pub fn list_ghosts(
        &self,
        rom_hash: &str,
        limit: u32,
    ) -> anyhow::Result<Vec<crate::models::GhostEntry>> {
        let conn = self.conn.lock().unwrap();
        let limit = limit.min(100).max(1) as i64;

        let (sql, params_vec): (&str, Vec<Box<dyn rusqlite::types::ToSql>>) = if rom_hash.is_empty()
        {
            ("SELECT ghost_id, discord_id, username, rom_hash, filename, frame_count, uploaded_at
              FROM ghosts ORDER BY uploaded_at DESC LIMIT ?1",
             vec![Box::new(limit) as Box<dyn rusqlite::types::ToSql>])
        } else {
            ("SELECT ghost_id, discord_id, username, rom_hash, filename, frame_count, uploaded_at
              FROM ghosts WHERE rom_hash = ?1 ORDER BY uploaded_at DESC LIMIT ?2",
             vec![Box::new(rom_hash.to_string()), Box::new(limit)])
        };

        let params_refs: Vec<&dyn rusqlite::types::ToSql> =
            params_vec.iter().map(|p| p.as_ref()).collect();
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

    // ── Full match replay operations ───────────────────────────────────────

    /// Store replay metadata in SQLite. The actual compressed .ncrp file is
    /// written to `replays/<replay_id>.ncrp.gz` on the GCS-FUSE mount by the
    /// handler.
    pub fn upload_replay_meta(
        &self,
        replay: &crate::models::ReplayMetaInsert,
    ) -> anyhow::Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT OR REPLACE INTO replays
             (replay_id, discord_id, username, rom_hash, filename, p1_name, p2_name,
              p1_score, p2_score, winner, frame_count, duration, recorded_at, session_id,
              completed_games, completed_set, uploaded_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, datetime('now'))",
            params![
                &replay.replay_id,
                &replay.discord_id,
                &replay.username,
                &replay.rom_hash,
                &replay.filename,
                &replay.p1_name,
                &replay.p2_name,
                replay.p1_score,
                replay.p2_score,
                &replay.winner,
                replay.frame_count,
                &replay.duration,
                &replay.recorded_at,
                &replay.session_id,
                replay.completed_games,
                if replay.completed_set { 1 } else { 0 },
            ],
        )?;
        Ok(())
    }

    pub fn replay_filename(&self, replay_id: &str) -> anyhow::Result<Option<String>> {
        let conn = self.conn.lock().unwrap();
        let filename = conn.query_row(
            "SELECT filename FROM replays WHERE replay_id = ?1",
            params![replay_id],
            |row| row.get(0),
        );
        match filename {
            Ok(filename) => Ok(Some(filename)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    pub fn list_replays(
        &self,
        rom_hash: &str,
        limit: u32,
    ) -> anyhow::Result<Vec<crate::models::ReplayEntry>> {
        let conn = self.conn.lock().unwrap();
        let limit = limit.min(100).max(1) as i64;

        let (sql, params_vec): (&str, Vec<Box<dyn rusqlite::types::ToSql>>) = if rom_hash.is_empty()
        {
            (
                "SELECT replay_id, discord_id, username, rom_hash, filename, p1_name, p2_name,
                     p1_score, p2_score, winner, frame_count, duration, recorded_at,
                     session_id, completed_games, completed_set, uploaded_at
              FROM replays ORDER BY uploaded_at DESC LIMIT ?1",
                vec![Box::new(limit) as Box<dyn rusqlite::types::ToSql>],
            )
        } else {
            (
                "SELECT replay_id, discord_id, username, rom_hash, filename, p1_name, p2_name,
                     p1_score, p2_score, winner, frame_count, duration, recorded_at,
                     session_id, completed_games, completed_set, uploaded_at
              FROM replays WHERE rom_hash = ?1 ORDER BY uploaded_at DESC LIMIT ?2",
                vec![Box::new(rom_hash.to_string()), Box::new(limit)],
            )
        };

        let params_refs: Vec<&dyn rusqlite::types::ToSql> =
            params_vec.iter().map(|p| p.as_ref()).collect();
        let mut stmt = conn.prepare(sql)?;
        let rows = stmt.query_map(params_refs.as_slice(), |row| {
            let completed_set: i64 = row.get(15)?;
            Ok(crate::models::ReplayEntry {
                replay_id: row.get(0)?,
                discord_id: row.get(1)?,
                username: row.get(2)?,
                rom_hash: row.get(3)?,
                filename: row.get(4)?,
                url: String::new(),
                p1_name: row.get(5)?,
                p2_name: row.get(6)?,
                p1_score: row.get(7)?,
                p2_score: row.get(8)?,
                winner: row.get(9)?,
                frame_count: row.get(10)?,
                duration: row.get(11)?,
                recorded_at: row.get(12)?,
                session_id: row.get(13)?,
                completed_games: row.get(14)?,
                completed_set: completed_set != 0,
                uploaded_at: row.get(16)?,
            })
        })?;

        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }
}

/// Result of [`Db::claim_name`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NameClaimOutcome {
    /// Newly reserved for this owner.
    Claimed,
    /// Already held by this same owner (re-affirmed).
    Owned,
    /// Held by a different owner.
    Taken,
}

/// Result of [`Db::check_name`].
#[derive(Debug, Clone, Copy)]
pub struct NameCheckOutcome {
    pub available: bool,
    pub owned_by_you: bool,
}

fn read_rating(conn: &Connection, discord_id: &str) -> anyhow::Result<Rating> {
    Ok(conn.query_row(
        "SELECT mu, phi, sigma FROM players WHERE discord_id = ?1",
        params![discord_id],
        |r| {
            Ok(Rating {
                mu: r.get(0)?,
                phi: r.get(1)?,
                sigma: r.get(2)?,
            })
        },
    )?)
}
