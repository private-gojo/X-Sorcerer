# ============================================================
# GOJO SSH+WS RELAY — DOCKERFILE (v2, nginx-fronted)
# ============================================================
FROM alpine:latest AS builder

RUN apk add --no-cache build-base wget zlib-dev linux-headers cmake git openssl-dev

# I kept my dropbear build exactly as it was — this part was
# never broken, so I'm not touching it.
RUN wget https://matt.ucc.asn.au/dropbear/releases/dropbear-2024.85.tar.bz2 && \
    tar -xf dropbear-2024.85.tar.bz2 && \
    cd dropbear-2024.85 && \
    grep -rl "SSH-2.0-dropbear_" . | xargs sed -i 's/SSH-2.0-dropbear_/SSH-2.0-/g' && \
    echo '#define DROPBEAR_VERSION "Tectia-SSH_9.5_NVIDIA-RTX-PRO-6000-Blackwell"' > localoptions.h && \
    ./configure && \
    make PROGRAMS="dropbear dropbearkey" && \
    make PROGRAMS="dropbear dropbearkey" install

RUN git clone --depth 1 https://github.com/ambrop72/badvpn.git /tmp/badvpn && \
    mkdir -p /tmp/badvpn/build && \
    cd /tmp/badvpn/build && \
    cmake .. -DCMAKE_POLICY_VERSION_MINIMUM=3.5 -DBUILD_NOTHING_BY_DEFAULT=1 -DBUILD_UDPGW=1 && \
    make install

FROM alpine:latest

COPY --from=builder /usr/local/sbin/dropbear /usr/local/sbin/dropbear
COPY --from=builder /usr/local/bin/dropbearkey /usr/local/bin/dropbearkey
COPY --from=builder /usr/local/bin/badvpn-udpgw /usr/local/bin/badvpn-udpgw

# I added nginx here — this is the new piece.
RUN apk add --no-cache python3 supervisor bash tzdata zlib nginx nginx-mod-stream netcat-openbsd

RUN mkdir -p /etc/dropbear /var/log/supervisor /etc/x-sorcerer /run/nginx && \
    cp /usr/share/zoneinfo/Asia/Manila /etc/localtime && \
    echo "Asia/Manila" > /etc/timezone

COPY banner.txt /etc/x-sorcerer/banner.txt
COPY proxy.py /etc/x-sorcerer/proxy.py
COPY supervisord.conf /etc/supervisord.conf
COPY nginx.conf.template /etc/x-sorcerer/nginx.conf.template
COPY entrypoint.sh /entrypoint.sh

COPY wait-for-bridge.sh /etc/x-sorcerer/wait-for-bridge.sh
RUN chmod +x /etc/x-sorcerer/wait-for-bridge.sh

RUN chmod +x /entrypoint.sh /etc/x-sorcerer/proxy.py

# I added this check so a wrong binary path fails the BUILD
# immediately, instead of costing me a 4-minute Cloud Run
# startup-probe timeout every time I get this wrong.
RUN test -x /usr/sbin/nginx && \
    test -x /usr/bin/python3 && \
    test -x /usr/local/sbin/dropbear && \
    test -x /usr/local/bin/badvpn-udpgw && \
    echo "All binary paths verified at build time."

EXPOSE 8080

HEALTHCHECK --interval=30s --timeout=5s --start-period=10s --retries=3 \
  CMD nc -z 127.0.0.1 8081 || exit 1

ENTRYPOINT ["/entrypoint.sh"]
