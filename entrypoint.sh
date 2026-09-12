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
        USER=$(echo "$i" | cut -d: -f1)
        PASS=$(echo "$i" | cut -d: -f2)
        adduser -D -s /bin/false "$USER"
        echo "$USER:$PASS" | chpasswd
    done
else
    adduser -D -s /bin/false gojo
    echo "gojo:gojo" | chpasswd
fi

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
envsubst '${PORT}' < /etc/x-sorcerer/nginx.conf.template | sed "s/__PORT__/${PORT}/" > /etc/nginx/nginx.conf

echo "=== [BOOT CHECK] badvpn-udpgw version ==="
badvpn-udpgw --version
echo "=========================================="

echo "=== [BOOT CHECK] nginx config test ==="
nginx -t
echo "======================================="

exec /usr/bin/supervisord -c /etc/supervisord.conf
