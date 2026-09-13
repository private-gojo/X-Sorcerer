#!/bin/sh

# =========================
# WAIT-FOR-BRIDGE
# Prevents nginx from starting before the Python bridge (proxy.py)
# is ready on port 8081. Fixes the startup race condition where
# nginx tries to proxy to 127.0.0.1:8081 before it's listening.
# =========================

MAX_RETRIES=30
RETRY_COUNT=0

while [ $RETRY_COUNT -lt $MAX_RETRIES ]; do
  if nc -z 127.0.0.1 8081 2>/dev/null; then
    exec /usr/sbin/nginx -g "daemon off;"
  fi
  sleep 1
  RETRY_COUNT=$((RETRY_COUNT + 1))
done

echo "ERROR: Bridge did not become ready in time" >&2
exit 1


