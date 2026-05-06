# freeplay-stats

Match history, Glicko-2 ratings, and ghost-replay storage for Freeplay.
A small Rust/Axum service that the signaling server calls when a match
ends, plus public read endpoints for leaderboards and a public
upload/download API for ghost files.

## What it does

When a multiplayer match finishes, the signaling server POSTs the result
to `/results`. This service:

1. Upserts both players in SQLite (creating new entries at default Glicko
   1500 / RD 350 / σ 0.06).
2. Runs a Glicko-2 update on both ratings.
3. Records the match in the `matches` table (idempotent on `room_id`).
4. Increments W/L counters.
5. Posts a summary to Discord via webhook (fire-and-forget).

It also stores **ghost files** — `.ncgh` replays uploaded by the Freeplay
client — keyed by ROM hash so other players can list and download them
for solo playback.

## Endpoints

### Match recording
- `POST /results` — record a match. Auth: `X-API-Key` must equal
  `STATS_API_KEY`. Body:
  ```json
  {
    "room_id": "...",
    "winner_id": "<discord-id>",
    "loser_id": "<discord-id>",
    "winner_score": 2,
    "loser_score": 1,
    "rom_hash": "...",
    "winner_username": "alice",
    "loser_username": "bob"
  }
  ```
  Re-posting the same `room_id` is a no-op (UNIQUE constraint).

### Leaderboard / players (public reads)
- `GET /health` — returns `ok`.
- `GET /leaderboard?limit=50` — top players by Glicko `mu`, descending.
  Limit clamped to `[1, 100]`. Players with zero matches are excluded.
- `GET /player/:discord_id` — full profile (rating, RD, volatility, W/L/D).
  404 if unknown.
- `GET /player/:discord_id/history?limit=50` — recent matches with
  win/loss flag and opponent info. Limit clamped to `[1, 100]`.

### Ghost replays (public, no auth)
- `POST /ghosts/upload` — body has `ghost_id`, `discord_id`, `username`,
  `rom_hash`, `filename`, `file_base64`, `frame_count`. INSERT OR REPLACE
  on `ghost_id`.
- `GET /ghosts/list?rom_hash=<hash>&limit=50` — list ghosts, optionally
  filtered by ROM. Most recent first.
- `GET /ghosts/download/:ghost_id` — raw bytes,
  `Content-Type: application/octet-stream`. 404 if unknown.

## Storage

SQLite at `DB_PATH` (default `/db/stats.db` in production), opened with
`journal_mode=WAL`. Tables:

- `players` — `discord_id` PK, `username`, Glicko `mu`/`phi`/`sigma`,
  `wins`/`losses`/`draws`, `updated_at`.
- `matches` — `room_id` UNIQUE, `winner_id`/`loser_id` FK to players,
  scores, `rom_hash`, `played_at`. Indexed on winner, loser, and time.
- `ghosts` — `ghost_id` UNIQUE, owner identity, `rom_hash`, `filename`,
  `file_data` BLOB, `frame_count`, `uploaded_at`. Indexed on ROM and
  upload time.

Migrations are `CREATE TABLE IF NOT EXISTS` only — there is no migration
framework. Adding a column means hand-rolling an `ALTER TABLE` in
[src/db.rs](src/db.rs).

## Glicko-2

Standard Glicko-2 with `τ = 0.5`, scale factor `173.7178`, default rating
1500, RD 350, volatility 0.06. Implementation in
[src/glicko.rs](src/glicko.rs). Only win/loss outcomes are produced
today — `draws` is in the schema but never incremented.

## Configuration

Loaded from environment via [src/config.rs](src/config.rs).

| Var | Purpose |
| --- | --- |
| `STATS_API_KEY` | Required. Auth for `POST /results`. |
| `DB_PATH` | SQLite path. Defaults to `./stats.db` locally; set to `/db/stats.db` in Cloud Run. |
| `DISCORD_WEBHOOK_URL` | Optional. Empty disables match notifications. |
| `PORT` | HTTP listen port. Defaults to 8081 locally; Cloud Run sets 8080. |
| `RUST_LOG` | Tracing filter. Defaults to `freeplay_stats=debug,tower_http=debug`. |

Local dev reads `.env` via `dotenvy`.

## Running locally

```bash
cargo run
# or
DB_PATH=/tmp/stats.db STATS_API_KEY=dev cargo run
```

Then:

```bash
curl localhost:8081/health
curl -H "X-API-Key: dev" -X POST localhost:8081/results -d '{ ... }'
```

## Deploying

`bash deploy.sh` from this directory. Reads `.env` here for
`DISCORD_WEBHOOK_URL` and (optionally) `STATS_API_KEY`. The script:

1. Enables required GCP APIs.
2. Creates / updates `STATS_API_KEY` and `DISCORD_WEBHOOK_URL` secrets in
   Secret Manager (auto-generates the API key on first run).
3. Grants the Compute SA `secretmanager.secretAccessor` on those secrets.
4. Creates the GCS bucket `${PROJECT_ID}-freeplay-stats-db` and grants
   `storage.objectAdmin` to the Compute SA.
5. Builds the image on Cloud Build (`gcr.io/${PROJECT_ID}/freeplay-stats`).
6. Deploys to Cloud Run with the bucket mounted at `/db` via GCS-FUSE.

Production target:
- Project `quarterframe`, region `us-central1`.
- URL: `https://freeplay-stats-681135711161.us-central1.run.app`.
- 512Mi memory, 1 CPU, min 0 / max 1 instance (gen2 — required for
  Cloud Storage volume mounts).
- DB persisted to `gs://quarterframe-freeplay-stats-db`.

After deploying, propagate `STATS_SERVICE_URL` and `STATS_API_KEY` to
the signaling server's environment (see [agents.md](agents.md) for the
fast no-rebuild restart command).

> ⚠️ On Windows, run `deploy.sh` from PowerShell, not Git Bash —
> Git Bash mangles unix paths in gcloud volume-mount args. Other
> deployment gotchas are documented in [agents.md](agents.md).

## Code layout

- [src/main.rs](src/main.rs) — router, tracing, CORS.
- [src/handlers.rs](src/handlers.rs) — HTTP handlers.
- [src/auth.rs](src/auth.rs) — `X-API-Key` check for `/results`.
- [src/db.rs](src/db.rs) — SQLite + migrations + all queries.
- [src/glicko.rs](src/glicko.rs) — Glicko-2 update math.
- [src/discord.rs](src/discord.rs) — webhook poster (fire-and-forget).
- [src/models.rs](src/models.rs) — request/response types.
- [src/config.rs](src/config.rs) — env loading.
- [src/state.rs](src/state.rs) — shared `AppState`.

## What it does *not* do (yet)

- No draws — `draws` exists in the schema but `process_result` only
  handles win/loss.
- No auth on ghost endpoints — anyone can upload a ghost claiming any
  `discord_id`. Trusts the client.
- No rate limiting, no quota on ghost uploads or BLOB size.
- No backfill / batch rating recomputation — Glicko-2 is technically
  rating-period-based, but here it updates incrementally on each match.
- Single-instance only (`max-instances=1`) because SQLite + GCS-FUSE
  doesn't tolerate concurrent writers.
