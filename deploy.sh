#!/bin/bash
# ============================================================
# GOJO X-SORCERER SSH+WS — DEPLOY SCRIPT
# ============================================================
# LABEL: Why this file exists
# My Dockerfile/proxy.py were never the reason for the 503s
# and the rapid "Starting new instance" restart-loop in my
# console logs. That pattern (reason: MANUAL_OR_CUSTOMER_INITIATED,
# cycling every ~3 seconds) matches Cloud Run's own scale-to-zero
# behavior: with min-instances=0, EVERY new connection (including
# Google's own health-check probes) can trigger a fresh cold-start
# instance, which then gets torn down again right after. This is
# confirmed in Google's own Cloud Run WebSockets documentation:
# WebSocket/long-lived connections should raise concurrency and
# keep at least 1 warm instance.
# ============================================================

set -e

PROJECT_ID=$(gcloud config get-value project)
SERVICE_NAME="gojo-x-limit"
REGION=$(gcloud config get-value run/region 2>/dev/null || echo "us-central1")

echo "=== Building and pushing image via Cloud Build ==="
gcloud builds submit --tag "gcr.io/${PROJECT_ID}/${SERVICE_NAME}:latest" .

echo "=== Deploying to Cloud Run ==="
gcloud run deploy "${SERVICE_NAME}" \
    --image "gcr.io/${PROJECT_ID}/${SERVICE_NAME}:latest" \
    --platform managed \
    --region "${REGION}" \
    --allow-unauthenticated \
    --port 8080 \
    --min-instances 1 \
    --max-instances 16 \
    --concurrency 500 \
    --timeout 3600 \
    --memory 2Gi \
    --cpu 1 \
    --execution-environment gen2 \
    --no-cpu-throttling \
    --session-affinity

# ------------------------------------------------------------
# LABEL: Flag-by-flag explanation
#
# --min-instances 1
#   Keeps ONE instance always warm. This is the fix for the
#   restart-loop: without this, Cloud Run scales to zero between
#   connections and cold-starts a new instance per request.
#   Cost tradeoff: you pay for 1 idle instance 24/7. Necessary
#   for a tunnel service where users expect it always-on.
#
# --concurrency 500
#   Allows up to 500 simultaneous connections on the SAME warm
#   instance before Cloud Run spins up a second one. SSH+WS
#   sessions are long-lived but low-CPU-per-connection, so one
#   instance can safely hold many. (Google's own WebSockets docs
#   explicitly recommend raising this above the default of 80.)
#
# --timeout 3600
#   Maximum request duration = 1 hour (Cloud Run's absolute max).
#   Without this, the default 5-minute timeout would silently
#   disconnect any session running longer than 5 minutes —
#   this alone could explain "random disconnects after a while."
#
# --no-cpu-throttling
#   CPU stays allocated even when the instance isn't actively
#   handling a request cycle — required for background relay
#   loops (badvpn, dropbear) to keep running smoothly under
#   Cloud Run's request-based CPU allocation model.
#
# --session-affinity
#   Cloud Run will try to route repeat connections from the same
#   client back to the same instance where possible. Helps with
#   any per-instance state (like your IP-based rate limiter).
# ------------------------------------------------------------

echo "=== Deploy complete ==="
echo "Check logs with:"
echo "gcloud run services logs read ${SERVICE_NAME} --region ${REGION} --limit 100"
