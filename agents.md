# agents.md — freeplay-stats

Notes for AI agents (or future-you) working on this service. Companion docs:
`README.md` for general usage, `claude.md` if it exists for repo-specific
conventions, `deploy.sh` for the source of truth on deployment.

## Last session: 2026-06-12 (full replay upload endpoint)

### Full match replay storage
- Added `POST /replays/upload`, `GET /replays/list`, and
  `GET /replays/download/:replay_id`.
- Replay uploads mirror ghost uploads: binary gzip body, JWT-authenticated
  uploader identity, metadata in `X-Freeplay-*` headers.
- Replay files are written to `/db/replays/<replay_id>.ncrp.gz` on the
  GCS-FUSE mount. SQLite stores metadata only.
- `GET /replays/list` returns the same shape the app and GitHub Pages replay
  browser use: `{ "replays": [{ "file", "url", "p1", "p2", "frames", ... }] }`.
- Service split: `freeplay-signaling-server` still handles auth, matchmaking,
  match results, spectate state, and relay credential minting. `freeplay-relay`
  is UDP-only. Persistent replay blobs belong here in `freeplay-stats`.

## Previous session: 2026-04-28 (ghost upload refactor)

### Binary upload with filesystem storage
- Ghost uploads switched from JSON base64 to binary POST with gzip
  (`Content-Encoding: gzip`, `application/octet-stream`). Metadata moves from
  JSON body to HTTP headers: `X-Freeplay-Ghost-Id`, `X-Freeplay-Discord-Id`,
  `X-Freeplay-Username`, `X-Freeplay-Rom-Hash`, `X-Freeplay-Filename`,
  `X-Freeplay-Frame-Count`.
- Ghost files written to `/db/ghosts/<ghost_id>.ncgh.gz` (GCS-FUSE mount).
  SQLite stores only metadata (empty `file_data` BLOB).
- `download_ghost` tries filesystem first, falls back to SQLite BLOB for
  legacy uploads. Returns `Content-Encoding: gzip` header for compressed files.
- Removed `GhostUpload` JSON struct, `base64` crate. Added `ghosts_dir` to
  `AppState`, created on startup.
- `upload_ghost_meta` replaces old `upload_ghost` in Db.
- Client sends `~200-500KB` gzip'd payloads instead of `~3-6MB` base64 JSON.

### Deploy
Run `bash deploy.sh` to redeploy. Ghost files persist on GCS-FUSE mount at
`/db/ghosts/`. No DB migration needed — old BLOBs still readable via fallback.

## What this service is

`freeplay-stats` — Rust/Axum HTTP service that ingests match results from the
signaling server, runs Glicko rating updates, persists to SQLite, and posts
match summaries to Discord. Deployed to Cloud Run.

- Project: `quarterframe`
- Region: `us-central1`
- Service: `freeplay-stats`
- URL: `https://freeplay-stats-681135711161.us-central1.run.app`
- DB: SQLite at `/db/stats.db`, mounted via GCS-FUSE from
  `gs://quarterframe-freeplay-stats-db`
- Image: `gcr.io/quarterframe/freeplay-stats`

The signaling server (`freeplay-signaling-server`) calls `/results` on this
service with `STATS_API_KEY` for auth. Both services share the same Compute SA
in the same project, so secrets created by one deploy are accessible to the
other.

## Deploy

`bash deploy.sh` from this directory. It reads `.env` (gitignored) for
`DISCORD_WEBHOOK_URL` and optionally `STATS_API_KEY`. The key is
auto-generated on first deploy and stored in Secret Manager.

After the stats service deploys, the signaling server needs
`STATS_SERVICE_URL` and `STATS_API_KEY` in its environment. Either re-run
its `deploy.sh`, or for a fast restart without rebuilding the image:

```
gcloud run services update xband-signaling \
  --region=us-central1 --project=quarterframe \
  --update-env-vars="STATS_SERVICE_URL=https://freeplay-stats-681135711161.us-central1.run.app/results" \
  --update-secrets="STATS_API_KEY=STATS_API_KEY:latest"
```

## Gotchas hit during deployment (2026-04-28)

### 1. GCS-FUSE volumes need ≥512Mi memory

The original `deploy.sh` used `--memory=256Mi`. Cloud Run rejected this with:

> spec.template.spec.containers.resources.limits.memory: Invalid value
> specified for memory. Total memory < 512 Mi is not supported with gen2
> execution environment.

Cloud Storage volume mounts force gen2, and gen2 has a 512Mi minimum.
**Fixed** in `deploy.sh` — bumped to 512Mi. Don't drop it back down.

### 2. Git Bash on Windows mangles unix paths in gcloud args

Running `deploy.sh` from Git Bash, the `--add-volume-mount=...,mount-path=/db`
argument arrives at gcloud as `mount-path=C:/Program Files/Git/db`, producing:

> service.spec.template.spec.containers[0].volume_mounts[0].mount_path:
> should be a valid unix absolute path

This is MSYS path translation. Workarounds, in order of preference:
- **Run the deploy from PowerShell**, not Git Bash. PowerShell does no path
  conversion. (This is what unblocked the first deploy.)
- Set `MSYS_NO_PATHCONV=1` *only on the gcloud invocation*, not for the whole
  script — setting it shell-wide breaks gcloud's own path resolution
  (it then can't find `gcloud.py`).
- Double the leading slash: `mount-path=//db`. MSYS leaves this alone, and
  gcloud treats it as `/db`.

If you are an agent re-running this deploy on Windows, use PowerShell.

### 3. PowerShell parses `--flag=k=v,k=v` as multiple args

If you fall back to invoking gcloud directly from PowerShell, quote the whole
flag value or PowerShell splits it at commas:

```powershell
# Wrong — PowerShell splits at the commas:
--add-volume=name=stats-db,type=cloud-storage,bucket=...

# Right:
"--add-volume=name=stats-db,type=cloud-storage,bucket=..."
```

The bash version of `deploy.sh` is fine because bash doesn't do this.
