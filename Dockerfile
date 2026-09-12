FROM alpine:latest AS builder

RUN apk add --no-cache build-base wget zlib-dev linux-headers cmake git openssl-dev

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

RUN apk add --no-cache python3 supervisor bash tzdata zlib

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
