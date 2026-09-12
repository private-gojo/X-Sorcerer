#!/bin/bash

# =========================
# AUTHORIZE TUNNEL SHELL (THE FIX)
# =========================
if ! grep -q "/bin/false" /etc/shells; then
    echo "/bin/false" >> /etc/shells
fi

# =========================
# DYNAMIC ACCOUNT GENERATOR
# =========================
if [ -n "$ACCOUNTS" ]; then
    IFS=',' read -ra ADDR <<< "$ACCOUNTS"
    for i in "${ADDR[@]}"; do
        USER=$(echo $i | cut -d: -f1)
        PASS=$(echo $i | cut -d: -f2)
        # /bin/false prevents terminal shell access, pure tunneling only
        adduser -D -s /bin/false "$USER"
        echo "$USER:$PASS" | chpasswd
    done
else
    # Fallback default kung walang nilagay sa env var
    adduser -D -s /bin/false gojo
    echo "gojo:gojo" | chpasswd
fi

# =========================
# DROPBEAR HOST KEYS
# =========================
if [ ! -f /etc/dropbear/dropbear_rsa_host_key ]; then
    dropbearkey -t rsa -f /etc/dropbear/dropbear_rsa_host_key -s 2048
fi
if [ ! -f /etc/dropbear/dropbear_ecdsa_host_key ]; then
    dropbearkey -t ecdsa -f /etc/dropbear/dropbear_ecdsa_host_key -s 521
fi

# =========================
# VERIFY BADVPN VERSION (debug)
# =========================
badvpn-udpgw --version

exec /usr/bin/supervisord -c /etc/supervisord.conf
