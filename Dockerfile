# ============================================================
# GOJO SSH+WS RELAY — DOCKERFILE (v2, nginx-fronted)
# ============================================================
# Update: Pin the build image to a fixed Alpine 3.24 patch release.
FROM alpine:3.24.3 AS builder

# Update: Pin build tools and development libraries to exact APK revisions.
RUN apk add --no-cache \
    build-base=0.5-r4 \
    wget=1.25.0-r3 \
    zlib-dev=1.3.2-r1 \
    linux-headers=7.0.0-r1 \
    cmake=4.2.3-r0 \
    git=2.54.0-r0 \
    openssl-dev=3.5.9-r0

# Dropbear: SSH server build and configured crypto feature set.
RUN wget https://matt.ucc.asn.au/dropbear/releases/dropbear-2026.94.tar.bz2 && \
    tar -xf dropbear-2026.94.tar.bz2 && \
    cd dropbear-2026.94 && \
    grep -Fq '#define LOCAL_IDENT "SSH-2.0-dropbear" IDENT_VERSION_PART' src/sysoptions.h && \
    sed -i 's|#define LOCAL_IDENT "SSH-2.0-dropbear" IDENT_VERSION_PART|#define LOCAL_IDENT "SSH-2.0-" DROPBEAR_VERSION|' src/sysoptions.h && \
    printf '%s\n' \
      '#define DROPBEAR_VERSION "OpenSSH_10.6p2 Ubuntu-2ubuntu3.6"' \
      '#define DROPBEAR_SMALL_CODE 0' \
      '#define DROPBEAR_USER_ALGO_LIST 1' \
      '#define DROPBEAR_AES128 1' \
      '#define DROPBEAR_AES256 1' \
      '#define DROPBEAR_3DES 0' \
      '#define DROPBEAR_CHACHA20POLY1305 1' \
      '#define DROPBEAR_ENABLE_CTR_MODE 1' \
      '#define DROPBEAR_ENABLE_GCM_MODE 1' \
      '#define DROPBEAR_ENABLE_CBC_MODE 0' \
      '#define DROPBEAR_SHA1_HMAC 0' \
      '#define DROPBEAR_SHA2_256_HMAC 1' \
      '#define DROPBEAR_SHA2_512_HMAC 0' \
      '#define DROPBEAR_SHA1_96_HMAC 0' \
      '#define DROPBEAR_RSA 1' \
      '#define DROPBEAR_RSA_SHA1 0' \
      '#define DROPBEAR_DSS 0' \
      '#define DROPBEAR_ECDSA 1' \
      '#define DROPBEAR_ED25519 1' \
      '#define DROPBEAR_DH_GROUP14_SHA1 0' \
      '#define DROPBEAR_DH_GROUP14_SHA256 1' \
      '#define DROPBEAR_DH_GROUP16 0' \
      '#define DROPBEAR_CURVE25519 1' \
      '#define DROPBEAR_SNTRUP761 1' \
      '#define DROPBEAR_MLKEM768 1' \
      '#define DROPBEAR_ECDH 1' \
      '#define DROPBEAR_DH_GROUP1 0' > localoptions.h && \
    ./configure && \
    make PROGRAMS="dropbear dropbearkey" && \
    make PROGRAMS="dropbear dropbearkey" install

# Update: Pin BadVPN to a fixed upstream commit for reproducible builds.
RUN git clone --depth 1 https://github.com/ambrop72/badvpn.git /tmp/badvpn && \
    git -C /tmp/badvpn checkout --detach 07268f02706e78e282e19641b5d1d41e8e89bf31 && \
    mkdir -p /tmp/badvpn/build && \
    cd /tmp/badvpn/build && \
    cmake .. -DCMAKE_POLICY_VERSION_MINIMUM=3.5 -DBUILD_NOTHING_BY_DEFAULT=1 -DBUILD_UDPGW=1 && \
    make install

# Update: Use the same pinned Alpine release for the runtime image.
FROM alpine:3.24.3

COPY --from=builder /usr/local/sbin/dropbear /usr/local/sbin/dropbear
COPY --from=builder /usr/local/bin/dropbearkey /usr/local/bin/dropbearkey
COPY --from=builder /usr/local/bin/badvpn-udpgw /usr/local/bin/badvpn-udpgw

# Added: Nginx front end and required runtime services.
# Update: Pin runtime packages to exact APK revisions.
RUN apk add --no-cache \
    python3=3.14.10-r1 \
    py3-pip=26.1.2-r0 \
    supervisor=4.3.0-r1 \
    bash=5.3.9-r1 \
    tzdata=2026f-r0 \
    zlib=1.3.2-r1 \
    nginx=1.30.6-r0 \
    nginx-mod-stream=1.30.6-r0 \
    netcat-openbsd=1.234.1-r0
# Fix: Pin uvloop and fail the image build if installation fails.
RUN python3 -m pip install --no-cache-dir --break-system-packages "uvloop==0.23.0"

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

# Fix: Fail during image build when a required runtime binary is missing.
RUN test -x /usr/sbin/nginx && \
    test -x /usr/bin/python3 && \
    test -x /usr/local/sbin/dropbear && \
    test -x /usr/local/bin/badvpn-udpgw && \
    echo "All binary paths verified at build time."

EXPOSE 8080

HEALTHCHECK --interval=30s --timeout=5s --start-period=10s --retries=3 \
  CMD nc -z 127.0.0.1 8081 || exit 1

ENTRYPOINT ["/entrypoint.sh"]
