#!/bin/bash

# ============================================================
# GOJO SSH+WS RELAY — ENTRYPOINT (v2, nginx-fronted)
# ============================================================

# I authorize the tunnel-only shell first, same as before.
if ! grep -q "/bin/false" /etc/shells; then
    echo "/bin/false" >> /etc/shells
fi

# Require explicit credentials; never create a predictable fallback account.
if [ -z "${ACCOUNTS:-}" ]; then
    echo "ACCOUNTS is required; refusing to start without explicit credentials" >&2
    exit 1
fi

if [[ "$ACCOUNTS" =~ [[:cntrl:]] ]]; then
    echo "ACCOUNTS contains unsupported control characters" >&2
    exit 1
fi

IFS=',' read -ra ADDR <<< "$ACCOUNTS"
if [ "${#ADDR[@]}" -eq 0 ]; then
    echo "ACCOUNTS contains no accounts" >&2
    exit 1
fi

for i in "${ADDR[@]}"; do
    USER=${i%%:*}
    PASS=${i#*:}

    if [[ "$i" != *:* || "$i" == *:*:* || -z "$USER" || -z "$PASS" || ! "$USER" =~ ^[a-zA-Z0-9_.-]{1,32}$ || "$PASS" =~ [[:cntrl:]] ]]; then
        echo "ACCOUNTS contains an invalid username/password entry" >&2
        exit 1
    fi

    adduser -D -s /bin/false "$USER" || exit 1
    printf '%s:%s\n' "$USER" "$PASS" | chpasswd || exit 1
done

# I generate dropbear host keys if they don't exist yet.
if [ ! -f /etc/dropbear/dropbear_rsa_host_key ]; then
    dropbearkey -t rsa -f /etc/dropbear/dropbear_rsa_host_key -s 2048
fi
if [ ! -f /etc/dropbear/dropbear_ecdsa_host_key ]; then
    dropbearkey -t ecdsa -f /etc/dropbear/dropbear_ecdsa_host_key -s 521
fi

# I substitute Cloud Run's injected PORT into my nginx template.
# Cloud Run always sets this env var — I never hardcode the port
# anywhere else so this stays portable across environments.
export PORT="${PORT:-8080}"
sed "s/__PORT__/${PORT}/" /etc/x-sorcerer/nginx.conf.template > /etc/nginx/nginx.conf

# Badvpn needs a valid nameserver for reverse DNS lookups.
echo "nameserver 1.1.1.1" > /etc/resolv.conf
echo "nameserver 8.8.8.8" >> /etc/resolv.conf

echo "=== [BOOT CHECK] badvpn-udpgw version ==="
badvpn-udpgw --version
echo "=========================================="

# Start supervisord in background so we can trap SIGTERM
/usr/bin/supervisord -c /etc/supervisord.conf &
SUPERVISOR_PID=$!

shutdown() {
    echo "[ENTRYPOINT] Received SIGTERM, shutting down supervisord..."
    kill -s SIGTERM "$SUPERVISOR_PID"
    wait "$SUPERVISOR_PID"
    echo "[ENTRYPOINT] Shutdown complete"
}

trap shutdown SIGTERM SIGINT

wait "$SUPERVISOR_PID"
\n'* || "$PASS" == *
        echo "ACCOUNTS contains an invalid username/password entry" >&2
        exit 1
    fi

    adduser -D -s /bin/false "$USER" || exit 1
    printf '%s:%s\n' "$USER" "$PASS" | chpasswd || exit 1
done

# I generate dropbear host keys if they don't exist yet.
if [ ! -f /etc/dropbear/dropbear_rsa_host_key ]; then
    dropbearkey -t rsa -f /etc/dropbear/dropbear_rsa_host_key -s 2048
fi
if [ ! -f /etc/dropbear/dropbear_ecdsa_host_key ]; then
    dropbearkey -t ecdsa -f /etc/dropbear/dropbear_ecdsa_host_key -s 521
fi

# I substitute Cloud Run's injected PORT into my nginx template.
# Cloud Run always sets this env var — I never hardcode the port
# anywhere else so this stays portable across environments.
export PORT="${PORT:-8080}"
sed "s/__PORT__/${PORT}/" /etc/x-sorcerer/nginx.conf.template > /etc/nginx/nginx.conf

# Badvpn needs a valid nameserver for reverse DNS lookups.
echo "nameserver 1.1.1.1" > /etc/resolv.conf
echo "nameserver 8.8.8.8" >> /etc/resolv.conf

echo "=== [BOOT CHECK] badvpn-udpgw version ==="
badvpn-udpgw --version
echo "=========================================="

# Start supervisord in background so we can trap SIGTERM
/usr/bin/supervisord -c /etc/supervisord.conf &
SUPERVISOR_PID=$!

shutdown() {
    echo "[ENTRYPOINT] Received SIGTERM, shutting down supervisord..."
    kill -s SIGTERM "$SUPERVISOR_PID"
    wait "$SUPERVISOR_PID"
    echo "[ENTRYPOINT] Shutdown complete"
}

trap shutdown SIGTERM SIGINT

wait "$SUPERVISOR_PID"
\r'* ]]; then
        echo "ACCOUNTS contains an invalid username/password entry" >&2
        exit 1
    fi

    adduser -D -s /bin/false "$USER" || exit 1
    printf '%s:%s\n' "$USER" "$PASS" | chpasswd || exit 1
done

# I generate dropbear host keys if they don't exist yet.
if [ ! -f /etc/dropbear/dropbear_rsa_host_key ]; then
    dropbearkey -t rsa -f /etc/dropbear/dropbear_rsa_host_key -s 2048
fi
if [ ! -f /etc/dropbear/dropbear_ecdsa_host_key ]; then
    dropbearkey -t ecdsa -f /etc/dropbear/dropbear_ecdsa_host_key -s 521
fi

# I substitute Cloud Run's injected PORT into my nginx template.
# Cloud Run always sets this env var — I never hardcode the port
# anywhere else so this stays portable across environments.
export PORT="${PORT:-8080}"
sed "s/__PORT__/${PORT}/" /etc/x-sorcerer/nginx.conf.template > /etc/nginx/nginx.conf

# Badvpn needs a valid nameserver for reverse DNS lookups.
echo "nameserver 1.1.1.1" > /etc/resolv.conf
echo "nameserver 8.8.8.8" >> /etc/resolv.conf

echo "=== [BOOT CHECK] badvpn-udpgw version ==="
badvpn-udpgw --version
echo "=========================================="

# Start supervisord in background so we can trap SIGTERM
/usr/bin/supervisord -c /etc/supervisord.conf &
SUPERVISOR_PID=$!

shutdown() {
    echo "[ENTRYPOINT] Received SIGTERM, shutting down supervisord..."
    kill -s SIGTERM "$SUPERVISOR_PID"
    wait "$SUPERVISOR_PID"
    echo "[ENTRYPOINT] Shutdown complete"
}

trap shutdown SIGTERM SIGINT

wait "$SUPERVISOR_PID"
