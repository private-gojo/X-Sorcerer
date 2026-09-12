#!/bin/bash

# ============================================================
# GOJO X-SORCERER SSH+WS — ENTRYPOINT
# ============================================================

# ------------------------------------------------------------
# LABEL: 0. Authorize tunnel-only shell
# Required so /bin/false is a valid login shell (tunnel-only
# accounts, no interactive shell access) — kept from your setup
# ------------------------------------------------------------
if ! grep -q "/bin/false" /etc/shells; then
    echo "/bin/false" >> /etc/shells
fi

# ------------------------------------------------------------
# LABEL: 1. Dynamic account generator
# Reads ACCOUNTS env var (format: user1:pass1,user2:pass2)
# Falls back to a default gojo:gojo account if unset
# ------------------------------------------------------------
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

# ------------------------------------------------------------
# LABEL: 2. Dropbear host keys
# Regenerated on cold start since Cloud Run has no persistent
# volume between instances — this is expected/normal, not a bug.
# Adds ~1-2 seconds to boot time, not a factor in the crash-loop.
# ------------------------------------------------------------
if [ ! -f /etc/dropbear/dropbear_rsa_host_key ]; then
    dropbearkey -t rsa -f /etc/dropbear/dropbear_rsa_host_key -s 2048
fi
if [ ! -f /etc/dropbear/dropbear_ecdsa_host_key ]; then
    dropbearkey -t ecdsa -f /etc/dropbear/dropbear_ecdsa_host_key -s 521
fi

# ------------------------------------------------------------
# LABEL: 3. Verify badvpn-udpgw binary version (diagnostic)
# Confirms exactly which build shipped in this image — logs
# this on every boot so you can always check via Cloud Run logs
# without needing shell access into the running container.
# ------------------------------------------------------------
echo "=== [BOOT CHECK] badvpn-udpgw version ==="
badvpn-udpgw --version
echo "=========================================="

# ------------------------------------------------------------
# LABEL: 4. Boot supervisord (manages dropbear, proxy, badvpn)
# ------------------------------------------------------------
exec /usr/bin/supervisord -c /etc/supervisord.conf
