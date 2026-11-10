#!/bin/bash

# ============================================================
# GOJO SSH+WS RELAY — ENTRYPOINT (v2, nginx-fronted)
# ============================================================

# I authorize the tunnel-only shell first, same as before.
if ! grep -q "/bin/false" /etc/shells; then
    echo "/bin/false" >> /etc/shells
fi

# I generate accounts from the ACCOUNTS env var, or fall back
# to my default gojo:gojo if nothing is set.
if [ -n "$ACCOUNTS" ]; then
    IFS=',' read -ra ADDR <<< "$ACCOUNTS"
    for i in "${ADDR[@]}"; do
        USER=${i%%:*}
        PASS=${i#*:}
        if ! id -u "$USER" >/dev/null 2>&1; then
            adduser -D -s /bin/false "$USER"
        fi
        echo "$USER:$PASS" | chpasswd
    done
else
    if ! id -u gojo >/dev/null 2>&1; then
        adduser -D -s /bin/false gojo
    fi
    echo "gojo:gojo" | chpasswd
fi

# I generate dropbear host keys if they don't exist yet.
if [ ! -f /etc/dropbear/dropbear_rsa_host_key ]; then
    dropbearkey -t rsa -f /etc/dropbear/dropbear_rsa_host_key -s 2048
fi
if [ ! -f /etc/dropbear/dropbear_ecdsa_host_key ]; then
    dropbearkey -t ecdsa -f /etc/dropbear/dropbear_ecdsa_host_key -s 521
fi
if [ ! -f /etc/dropbear/dropbear_ed25519_host_key ]; then
    dropbearkey -t ed25519 -f /etc/dropbear/dropbear_ed25519_host_key
fi

# I substitute Cloud Run's injected PORT into my nginx template.
# Cloud Run always sets this env var — I never hardcode the port
# anywhere else so this stays portable across environments.
export PORT="${PORT:-8080}"
sed "s/__PORT__/${PORT}/" /etc/x-sorcerer/nginx.conf.template > /etc/nginx/nginx.conf

echo "=== [STARTUP DIAGNOSTICS] ==="
echo "[DIAG] resolver configuration:"
if [ -r /etc/resolv.conf ]; then
    cat /etc/resolv.conf
else
    echo "[DIAG] /etc/resolv.conf is unavailable"
fi
echo "[DIAG] configured nameservers:"
grep -E '^[[:space:]]*nameserver[[:space:]]+' /etc/resolv.conf 2>/dev/null || true
echo "[DIAG] online CPU entries: $(grep -c '^processor' /proc/cpuinfo 2>/dev/null || echo unknown)"
echo "[DIAG] file descriptor limit: $(ulimit -n)"
echo "[DIAG] cgroup cpu.max:"
cat /sys/fs/cgroup/cpu.max 2>/dev/null || true
echo "[DIAG] cgroup cpu.stat:"
cat /sys/fs/cgroup/cpu.stat 2>/dev/null || cat /sys/fs/cgroup/cpu/cpu.stat 2>/dev/null || true
echo "[DIAG] cgroup memory.current / memory.max:"
cat /sys/fs/cgroup/memory.current /sys/fs/cgroup/memory.max 2>/dev/null || true
echo "[DIAG] process status:"
grep -E '^(VmRSS|Threads):' /proc/self/status 2>/dev/null || true
echo "=== [STARTUP DIAGNOSTICS END] ==="

echo "=== [BOOT CHECK] Rust UDPGW version ==="
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
