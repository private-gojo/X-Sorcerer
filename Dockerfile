# ============================================================
# GOJO X-SORCERER SSH+WS — DOCKERFILE
# ============================================================
# LABEL: Stage 1 — Builder
# Compiles dropbear (SSH server) + badvpn-udpgw (UDP gateway)
# from source. This stage is discarded after build — only the
# compiled binaries are copied into the final image.
# ============================================================
FROM alpine:latest AS builder

RUN apk add --no-cache build-base wget zlib-dev linux-headers cmake git openssl-dev

# ------------------------------------------------------------
# LABEL: Dropbear build
# - Version pinned to 2024.85 (was already pinned in my setup — this is good practice, keep this)
# - Custom SSH-2.0 banner string strip: removes the literal
#   "SSH-2.0-dropbear_" prefix from source so my banner.txt
#   fully controls what the client sees (fingerprinting evasion)
# - localoptions.h override: sets DROPBEAR_VERSION string
#   (my custom obfuscation label — kept exactly as i had it)
# ------------------------------------------------------------
RUN wget https://matt.ucc.asn.au/dropbear/releases/dropbear-2024.85.tar.bz2 && \
    tar -xf dropbear-2024.85.tar.bz2 && \
    cd dropbear-2024.85 && \
    grep -rl "SSH-2.0-dropbear_" . | xargs sed -i 's/SSH-2.0-dropbear_/SSH-2.0-/g' && \
    echo '#define DROPBEAR_VERSION "Tectia-SSH_9.5_NVIDIA-RTX-PRO-6000-Blackwell"' > localoptions.h && \
    ./configure && \
    make PROGRAMS="dropbear dropbearkey" && \
    make PROGRAMS="dropbear dropbearkey" install

# ------------------------------------------------------------
# LABEL: badvpn-udpgw build
# - Confirmed via source review: latest available version is
#   1.999.130 (repo archived 2021-08-22, no newer releases exist
#   anywhere — this is NOT a stale/wrong version, it's the only one)
# - BUILD_UDPGW=1 only — we don't need tun2socks or the full VPN suite
# ------------------------------------------------------------
RUN git clone --depth 1 https://github.com/ambrop72/badvpn.git /tmp/badvpn && \
    mkdir -p /tmp/badvpn/build && \
    cd /tmp/badvpn/build && \
    cmake .. -DCMAKE_POLICY_VERSION_MINIMUM=3.5 -DBUILD_NOTHING_BY_DEFAULT=1 -DBUILD_UDPGW=1 && \
    make install

# ============================================================
# LABEL: Stage 2 — Runtime image
# Minimal Alpine base + only the compiled binaries copied in.
# No compiler toolchain shipped in the final image (smaller,
# faster cold start, smaller attack surface).
# ============================================================
FROM alpine:latest

COPY --from=builder /usr/local/sbin/dropbear /usr/local/sbin/dropbear
COPY --from=builder /usr/local/bin/dropbearkey /usr/local/bin/dropbearkey
COPY --from=builder /usr/local/bin/badvpn-udpgw /usr/local/bin/badvpn-udpgw

RUN apk add --no-cache python3 supervisor bash tzdata zlib

RUN mkdir -p /etc/dropbear /var/log/supervisor /etc/x-sorcerer && \
    cp /usr/share/zoneinfo/Asia/Manila /etc/localtime && \
    echo "Asia/Manila" > /etc/timezone

# LABEL: My own banner.txt (server identity message) —Fix as-is
COPY banner.txt /etc/x-sorcerer/banner.txt
COPY proxy.py /etc/x-sorcerer/proxy.py
COPY supervisord.conf /etc/supervisord.conf
COPY entrypoint.sh /entrypoint.sh

RUN chmod +x /entrypoint.sh /etc/x-sorcerer/proxy.py

EXPOSE 8080

ENTRYPOINT ["/entrypoint.sh"]
