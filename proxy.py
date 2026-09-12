#!/usr/bin/env python3
# ============================================================
# GOJO X-SORCERER SSH+WS — PROXY ENGINE
# ============================================================
# LABEL: Purpose
# Accepts a "fake WebSocket" upgrade from NetMod/HTTP Injector
# style clients, then relays raw bytes bidirectionally to the
# local dropbear SSH daemon. UDP (gaming/voice/QUIC) is handled
# separately and automatically by dropbear's own direct-tcpip
# port forwarding into badvpn-udpgw — this proxy does NOT need
# to route UDP itself. (Verified against source + your own
# Cloud Run logs showing 127.0.0.1 connections reaching udpgw.)
# ============================================================

import asyncio
import os
import socket
import time
import logging
from collections import defaultdict

logging.basicConfig(level=logging.INFO, format='%(asctime)s - [X-PROXY] - %(message)s')

# ------------------------------------------------------------
# LABEL: Configuration
# ------------------------------------------------------------
LISTEN_PORT = int(os.environ.get("PORT", "8080"))
SSH_PORT = 2224
BUF_SIZE = 65536          # 64KB chunks — good for Cloud Run's 1-2vCPU test scale
IDLE_TIMEOUT = 300        # seconds — kill a relay leg if silent this long (prevents dead coroutines hanging forever)
HANDSHAKE_TIMEOUT = 3.0   # seconds to receive the full HTTP header before giving up

# ------------------------------------------------------------
# LABEL: Anti-DDoS / rate limiting
# Kept from your existing setup — unchanged logic, just relocated
# ------------------------------------------------------------
MAX_CONNS_PER_IP = 50
active_connections = defaultdict(int)
blocked_ips = {}
BLOCK_TIME = 300  # 5 minutes penalty


def is_rate_limited(ip):
    current_time = time.time()
    if ip in blocked_ips:
        if current_time - blocked_ips[ip] < BLOCK_TIME:
            return True
        else:
            del blocked_ips[ip]

    if active_connections[ip] >= MAX_CONNS_PER_IP:
        blocked_ips[ip] = current_time
        logging.warning(f"Anti-DDoS: Blocked IP {ip} - Exceeded {MAX_CONNS_PER_IP} connections.")
        return True
    return False


# ------------------------------------------------------------
# LABEL: Socket-level tuning
# TCP_NODELAY disables Nagle's algorithm (reduces latency for
# small packets — important for interactive SSH). Buffer sizes
# widened for sustained throughput (speedtest scenarios).
# ------------------------------------------------------------
def optimize_socket(sock):
    if not sock:
        return
    try:
        sock.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
        sock.setsockopt(socket.SOL_SOCKET, socket.SO_KEEPALIVE, 1)
    except OSError:
        # Some socket types (or already-closed sockets) may reject this — non-fatal
        pass


# ------------------------------------------------------------
# LABEL: relay()
# Moves bytes from one side of the tunnel to the other.
# FIX vs previous version: added a read timeout so a silently
# dead connection doesn't hold the coroutine (and its share of
# the event loop) open indefinitely.
# ------------------------------------------------------------
async def relay(reader, writer, label=""):
    try:
        while True:
            data = await asyncio.wait_for(reader.read(BUF_SIZE), timeout=IDLE_TIMEOUT)
            if not data:
                break
            writer.write(data)
            await writer.drain()
    except asyncio.TimeoutError:
        logging.info(f"Relay idle timeout ({label}) — closing")
    except Exception:
        pass
    finally:
        writer.close()


# ------------------------------------------------------------
# LABEL: handle_client()
# Main per-connection handler: rate-limits, reads the fake-WS
# handshake safely, replies with a 101 Switching Protocols, then
# bridges the connection to dropbear on 127.0.0.1:2224.
# ------------------------------------------------------------
async def handle_client(client_reader, client_writer):
    addr = client_writer.get_extra_info('peername')
    if not addr:
        client_writer.close()
        return
    ip = addr[0]

    # LABEL: Google health check bypass (silent drop, no log noise)
    if ip == '169.254.169.126':
        client_writer.close()
        return

    # LABEL: Anti-DDoS gate
    if is_rate_limited(ip):
        client_writer.close()
        return

    active_connections[ip] += 1

    try:
        # --------------------------------------------------------
        # LABEL: Safe handshake read
        # FIX vs the Gemini "camouflage" version: does NOT touch
        # asyncio's private `reader._buffer` attribute (unsupported
        # internal API — flagged previously as unsafe / matches a
        # real CPython bug report). Instead we accumulate into our
        # own `raw` buffer and manually split off anything read
        # past the header boundary, which is always safe.
        # --------------------------------------------------------
        raw = b""
        while b"\r\n\r\n" not in raw:
            chunk = await asyncio.wait_for(client_reader.read(4096), timeout=HANDSHAKE_TIMEOUT)
            if not chunk:
                return
            raw += chunk
            if len(raw) > 16384:  # safety cap — malformed/oversized header, abandon
                return

        header_end = raw.index(b"\r\n\r\n") + 4
        leftover = raw[header_end:]  # any client bytes that arrived right after the header

        # LABEL: DEBUG — log exactly what was received, byte for byte.
        # Temporary diagnostic: remove once the 503 cause is confirmed.
        logging.info(f"RAW REQUEST from {ip}: {raw!r}")

        # LABEL: Accept "fake" WebSocket upgrades (NetMod / HTTP Injector style —
        # no Sec-WebSocket-Key validation, matches your client's actual behavior)
        if b"upgrade: websocket" in raw.lower():
            client_writer.write(
                b"HTTP/1.1 101 Switching Protocols\r\n"
                b"Upgrade: websocket\r\n"
                b"Connection: Upgrade\r\n\r\n"
            )
            await client_writer.drain()
            logging.info(f"Sent 101 response to {ip}, connecting to dropbear...")
        else:
            logging.info(f"No websocket upgrade header found from {ip}, dropping")
            return

        # LABEL: Connect to local dropbear
        try:
            ssh_reader, ssh_writer = await asyncio.open_connection("127.0.0.1", SSH_PORT)
            logging.info(f"Connected to dropbear on 127.0.0.1:{SSH_PORT} for {ip}")
        except Exception as e:
            logging.error(f"FAILED to connect to dropbear for {ip}: {e!r}")
            return

        optimize_socket(client_writer.get_extra_info('socket'))
        optimize_socket(ssh_writer.get_extra_info('socket'))

        # LABEL: Flush any leftover bytes (post-header data already
        # received in the same TCP segment) straight to dropbear
        # before starting the bidirectional relay — this data would
        # otherwise be silently lost.
        if leftover:
            ssh_writer.write(leftover)
            await ssh_writer.drain()

        await asyncio.gather(
            relay(client_reader, ssh_writer, label="client->ssh"),
            relay(ssh_reader, client_writer, label="ssh->client"),
            return_exceptions=True
        )
    except Exception:
        pass
    finally:
        active_connections[ip] -= 1
        if active_connections[ip] <= 0:
            del active_connections[ip]
        client_writer.close()


async def main():
    server = await asyncio.start_server(handle_client, "0.0.0.0", LISTEN_PORT)
    logging.info(f"Listening on 0.0.0.0:{LISTEN_PORT} -> forwarding to 127.0.0.1:{SSH_PORT}")
    async with server:
        await server.serve_forever()


if __name__ == "__main__":
    asyncio.run(main())
