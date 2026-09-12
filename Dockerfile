# =========================
# STAGE 1: THE TECTIA/BLACKWELL BUILDER
# =========================
FROM alpine:latest AS builder

RUN apk add --no-cache build-base wget zlib-dev linux-headers

RUN wget https://matt.ucc.asn.au/dropbear/releases/dropbear-2024.85.tar.bz2 && \
    tar -xf dropbear-2024.85.tar.bz2 && \
    cd dropbear-2024.85 && \
    echo '#define DROPBEAR_VERSION "Tectia-SSH_9.5_NVIDIA-RTX-PRO-6000-Blackwell"' > localoptions.h && \
    ./configure && \
    make PROGRAMS="dropbear" && \
    make PROGRAMS="dropbear" install

# =========================
# STAGE 2: PRODUCTION RUNTIME
# =========================
FROM alpine:latest

COPY --from=builder /usr/local/sbin/dropbear /usr/local/sbin/dropbear

RUN apk add --no-cache python3 supervisor bash tzdata zlib badvpn
RUN mkdir -p /etc/dropbear /var/log/supervisor /etc/x-sorcerer && \
    cp /usr/share/zoneinfo/Asia/Manila /etc/localtime && \
    echo "Asia/Manila" > /etc/timezone

COPY banner.txt /etc/x-sorcerer/banner.txt
COPY proxy.py /etc/x-sorcerer/proxy.py
COPY supervisord.conf /etc/supervisord.conf
COPY entrypoint.sh /entrypoint.sh

RUN chmod +x /entrypoint.sh /etc/x-sorcerer/proxy.py

EXPOSE 8080

ENTRYPOINT ["/entrypoint.sh"]
