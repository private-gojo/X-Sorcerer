#!/bin/sh

# =========================
# WAIT-FOR-BRIDGE
# Prevents nginx from starting before both the Python bridge
# (127.0.0.1:8081) and Dropbear backend (127.0.0.1:2224) are ready.
# =========================

MAX_RETRIES=30
RETRY_COUNT=0

while [ $RETRY_COUNT -lt $MAX_RETRIES ]; do
  if nc -z 127.0.0.1 8081 2>/dev/null && nc -z 127.0.0.1 2224 2>/dev/null; then
    exec /usr/sbin/nginx -g "daemon off;"
  fi
  sleep 1
  RETRY_COUNT=$((RETRY_COUNT + 1))
done

echo "ERROR: Bridge or Dropbear backend did not become ready in time" >&2
exit 1


