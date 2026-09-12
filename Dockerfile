# ============================================================
# STAGE 1: COMPILATION 
# ============================================================
FROM alpine:latest AS builder

RUN apk add --no-cache build-base wget zlib-dev linux-headers cmake git openssl-dev

# 1. Compile Custom Dropbear (Fixed dropbearkey omission)
RUN wget https://matt.ucc.asn.au/dropbear/releases/dropbear-2024.85.tar.bz2 && \
    tar -xf dropbear-2024.85.tar.bz2 && \
    cd dropbear-2024.85 && \
    grep -rl "SSH-2.0-dropbear_" . | xargs sed -i 's/SSH-2.0-dropbear_/SSH-2.0-/g' && \
    echo '#define DROPBEAR_VERSION "Tectia-SSH_9.5_NVIDIA-RTX-PRO-6000-Blackwell"' > localoptions.h && \
    ./configure && \
    make PROGRAMS="dropbear dropbearkey" && \
    make PROGRAMS="dropbear dropbearkey" install

# 2. Compile BadVPN
RUN git clone --depth 1 https://github.com/ambrop72/badvpn.git /tmp/badvpn && \
    mkdir -p /tmp/badvpn/build && \
    cd /tmp/badvpn/build && \
    cmake .. -DCMAKE_POLICY_VERSION_MINIMUM=3.5 -DBUILD_NOTHING_BY_DEFAULT=1 -DBUILD_UDPGW=1 && \
    make install

# 3. Fetch Websocat (Rust-based WS to TCP bridge)
RUN wget https://github.com/vi/websocat/releases/latest/download/websocat.x86_64-unknown-linux-musl -O /usr/local/bin/websocat && \
    chmod +x /usr/local/bin/websocat

# ============================================================
# STAGE 2: PRODUCTION RUNTIME
# ============================================================
FROM alpine:latest

COPY --from=builder /usr/local/sbin/dropbear /usr/local/sbin/dropbear
COPY --from=builder /usr/local/bin/dropbearkey /usr/local/bin/dropbearkey
COPY --from=builder /usr/local/bin/badvpn-udpgw /usr/local/bin/badvpn-udpgw
COPY --from=builder /usr/local/bin/websocat /usr/local/bin/websocat

# Notice: Python3 is totally removed. Pure binaries only.
RUN apk add --no-cache supervisor bash tzdata zlib

RUN mkdir -p /etc/dropbear /var/log/supervisor /etc/x-sorcerer && \
    cp /usr/share/zoneinfo/Asia/Manila /etc/localtime && \
    echo "Asia/Manila" > /etc/timezone

COPY banner.txt /etc/x-sorcerer/banner.txt
COPY supervisord.conf /etc/supervisord.conf
COPY entrypoint.sh /entrypoint.sh

RUN chmod +x /entrypoint.sh

EXPOSE 8080

ENTRYPOINT ["/entrypoint.sh"]
