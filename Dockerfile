FROM alpine:latest

# =========================
# SYSTEM DEPENDENCIES
# Dropbear = Lightweight SSH daemon | Python3 = WS Proxy
# Supervisor = Process Manager | BadVPN = UDP over TCP
# =========================
RUN apk add --no-cache \
    dropbear \
    python3 \
    supervisor \
    badvpn \
    curl \
    bash \
    tzdata

# =========================
# DIRECTORY & SECURITY SETUP
# =========================
RUN mkdir -p /etc/dropbear /var/log/supervisor /etc/x-sorcerer && \
    cp /usr/share/zoneinfo/Asia/Manila /etc/localtime && \
    echo "Asia/Manila" > /etc/timezone

# COPY configs and scripts (I-u-uncomment natin ito habang ginagawa ang mga files)
# COPY supervisord.conf /etc/supervisord.conf
# COPY proxy.py /etc/x-sorcerer/proxy.py
# COPY entrypoint.sh /entrypoint.sh
# RUN chmod +x /entrypoint.sh

EXPOSE 8080

# ENTRYPOINT ["/entrypoint.sh"]
