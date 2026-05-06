#!/bin/bash
# deploy.sh — freeplay-stats → Cloud Run
#
# Reads config from .env in the same directory as this script.
# Required .env contents:
#   STATS_API_KEY=...                (auto-generated if missing)
#   DISCORD_WEBHOOK_URL=...          (optional)
#
# The stats service stores ratings + match history in SQLite.
# Database lives on a Cloud Storage FUSE mount so it survives restarts.

set -euo pipefail

# ── Config ────────────────────────────────────────────────────────────────────
PROJECT_ID="quarterframe"
REGION="us-central1"
SERVICE_NAME="freeplay-stats"
IMAGE="gcr.io/${PROJECT_ID}/${SERVICE_NAME}"
BUCKET_NAME="${PROJECT_ID}-freeplay-stats-db"

# ── Load .env ─────────────────────────────────────────────────────────────────
SCRIPT_DIR="$( cd "$( dirname "${BASH_SOURCE[0]}" )" &> /dev/null && pwd )"
ENV_FILE="${SCRIPT_DIR}/.env"

if [ -f "${ENV_FILE}" ]; then
  set -a
  source "${ENV_FILE}"
  set +a
fi

STATS_API_KEY="${STATS_API_KEY:-}"
DISCORD_WEBHOOK_URL="${DISCORD_WEBHOOK_URL:-}"

echo "================================================================"
echo "Project: ${PROJECT_ID} | Region: ${REGION} | Service: ${SERVICE_NAME}"
echo "================================================================"
echo ""

# ── 0. Preflight checks ───────────────────────────────────────────────────────
echo "[0/6] Preflight checks..."

if ! gcloud auth list --filter=status:ACTIVE --format="value(account)" | grep -q .; then
  echo "  ❌ Not authenticated. Run: gcloud auth login"
  exit 1
fi
ACTIVE_ACCOUNT=$(gcloud auth list --filter=status:ACTIVE --format="value(account)")
echo "  ✓ Authenticated as: ${ACTIVE_ACCOUNT}"

if ! gcloud projects describe "${PROJECT_ID}" &>/dev/null; then
  echo "  ❌ Cannot access project '${PROJECT_ID}'."
  exit 1
fi
echo "  ✓ Project accessible"

PROJECT_NUMBER=$(gcloud projects describe "${PROJECT_ID}" --format="value(projectNumber)")
COMPUTE_SA="${PROJECT_NUMBER}-compute@developer.gserviceaccount.com"
echo "  ✓ Compute SA: ${COMPUTE_SA}"
echo ""

# ── 1. Enable required GCP APIs ───────────────────────────────────────────────
echo "[1/6] Enabling GCP APIs..."
gcloud services enable \
  run.googleapis.com \
  secretmanager.googleapis.com \
  cloudbuild.googleapis.com \
  containerregistry.googleapis.com \
  --project="${PROJECT_ID}"
echo ""

# ── 2. Create secrets ──────────────────────────────────────────────────────────
echo "[2/6] Writing secrets to Secret Manager..."

create_secret() {
  local name=$1
  local value=$2
  if gcloud secrets describe "${name}" --project="${PROJECT_ID}" &>/dev/null; then
    echo "  Updating: ${name}"
    echo -n "${value}" | gcloud secrets versions add "${name}" --data-file=- --project="${PROJECT_ID}" >/dev/null
  else
    echo "  Creating: ${name}"
    echo -n "${value}" | gcloud secrets create "${name}" --data-file=- --project="${PROJECT_ID}" >/dev/null
  fi
}

if [ -n "${STATS_API_KEY}" ]; then
  create_secret "STATS_API_KEY" "${STATS_API_KEY}"
elif ! gcloud secrets describe "STATS_API_KEY" --project="${PROJECT_ID}" &>/dev/null; then
  create_secret "STATS_API_KEY" "$(openssl rand -base64 32)"
  echo "  ✓ STATS_API_KEY auto-generated"
else
  echo "  ✓ STATS_API_KEY already exists"
fi

if [ -n "${DISCORD_WEBHOOK_URL}" ]; then
  create_secret "DISCORD_WEBHOOK_URL" "${DISCORD_WEBHOOK_URL}"
else
  create_secret "DISCORD_WEBHOOK_URL" ""
fi
echo ""

# ── 3. Grant secret access ─────────────────────────────────────────────────────
echo "[3/6] Granting secret access..."

# JWT_SECRET is shared with the signaling server — stats verifies client-issued
# JWTs locally for ghost upload (rather than round-tripping through signaling).
# We expect signaling's deploy to have created the secret first; if missing,
# warn but don't fail — stats just returns 503 on JWT-protected endpoints.
SECRETS_TO_BIND=(STATS_API_KEY DISCORD_WEBHOOK_URL)
if gcloud secrets describe JWT_SECRET --project="${PROJECT_ID}" &>/dev/null; then
  SECRETS_TO_BIND+=(JWT_SECRET)
else
  echo "  ⚠ JWT_SECRET not in Secret Manager — ghost upload will return 503"
  echo "    Run signaling-server deploy.sh first (it creates the JWT_SECRET secret)."
fi

for SECRET in "${SECRETS_TO_BIND[@]}"; do
  gcloud secrets add-iam-policy-binding "${SECRET}" \
    --member="serviceAccount:${COMPUTE_SA}" \
    --role="roles/secretmanager.secretAccessor" \
    --project="${PROJECT_ID}" \
    --condition=None \
    >/dev/null 2>&1 || echo "    (binding already exists for ${SECRET})"
done
echo "  ✓ Access granted"
echo ""

# ── 4. Set up Cloud Storage bucket for SQLite DB ───────────────────────────────
echo "[4/6] Setting up database storage..."

# Create the bucket if it doesn't exist.
if ! gcloud storage buckets describe "gs://${BUCKET_NAME}" --project="${PROJECT_ID}" &>/dev/null; then
  gcloud storage buckets create "gs://${BUCKET_NAME}" \
    --project="${PROJECT_ID}" \
    --location="${REGION}" \
    --uniform-bucket-level-access
  echo "  ✓ Bucket created: gs://${BUCKET_NAME}"
else
  echo "  ✓ Bucket exists: gs://${BUCKET_NAME}"
fi

# Grant the compute SA access to the bucket.
gcloud storage buckets add-iam-policy-binding "gs://${BUCKET_NAME}" \
  --member="serviceAccount:${COMPUTE_SA}" \
  --role="roles/storage.objectAdmin" \
  --condition=None \
  >/dev/null 2>&1 || true
echo "  ✓ Bucket access granted"
echo ""

# ── 5. Build image via Cloud Build ────────────────────────────────────────────
echo "[5/6] Building image on Cloud Build..."
gcloud builds submit --tag "${IMAGE}" --project="${PROJECT_ID}"
echo "  ✓ Image pushed: ${IMAGE}"
echo ""

# ── 6. Deploy to Cloud Run ────────────────────────────────────────────────────
echo "[6/6] Deploying to Cloud Run..."

SECRETS_LIST="STATS_API_KEY=STATS_API_KEY:latest"
SECRETS_LIST="${SECRETS_LIST},DISCORD_WEBHOOK_URL=DISCORD_WEBHOOK_URL:latest"

# Conditionally include JWT_SECRET (shared with signaling). Without this,
# ghost-upload returns 503 — that's the intended degrade path so a stats
# deploy without signaling running first still serves leaderboards.
if gcloud secrets describe JWT_SECRET --project="${PROJECT_ID}" &>/dev/null; then
  SECRETS_LIST="${SECRETS_LIST},JWT_SECRET=JWT_SECRET:latest"
fi

gcloud run deploy "${SERVICE_NAME}" \
  --image="${IMAGE}" \
  --platform=managed \
  --region="${REGION}" \
  --project="${PROJECT_ID}" \
  --allow-unauthenticated \
  --min-instances=0 \
  --max-instances=1 \
  --memory=512Mi \
  --cpu=1 \
  --set-secrets="${SECRETS_LIST}" \
  --set-env-vars="DB_PATH=/db/stats.db" \
  --add-volume=name=stats-db,type=cloud-storage,bucket="${BUCKET_NAME}" \
  --add-volume-mount=volume=stats-db,mount-path=/db

SERVICE_URL=$(gcloud run services describe "${SERVICE_NAME}" \
  --region="${REGION}" \
  --project="${PROJECT_ID}" \
  --format="value(status.url)")

echo ""
echo "================================================================"
echo "✅  Deployed: ${SERVICE_URL}"
echo "================================================================"
echo ""
echo "Next steps:"
echo ""
echo "  1. Set STATS_SERVICE_URL and STATS_API_KEY in the signaling server's .env:"
echo "     STATS_SERVICE_URL=${SERVICE_URL}/results"
echo "     STATS_API_KEY=<value from GCP Secret Manager>"
echo ""
echo "  2. Test the health endpoint:"
echo "     curl ${SERVICE_URL}/health"
echo ""
echo "  3. Test the leaderboard:"
echo "     curl ${SERVICE_URL}/leaderboard"
echo ""
echo "  4. Watch live logs:"
echo "     gcloud beta run services logs tail ${SERVICE_NAME} --region=${REGION}"
