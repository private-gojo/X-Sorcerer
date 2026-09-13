#!/usr/bin/env python3
# ============================================================
# GOJO SSH+WS RELAY — INTERNAL BRIDGE
# ============================================================
# Nginx sits in front of me now using the stream module — it
# does raw Layer 4 TCP passthrough only, no HTTP or WebSocket
# parsing at all. So I handle the fake-WS handshake myself here,
# same as I always did, then relay straight into dropbear.
#
# I'm leaving out rate limiting and IP banning for now — I want
# the path clean while I confirm this is stable. I'll bring the
# anti-DDoS layer back once this is confirmed working.
# ============================================================

import asyncio
import os
import socket
import logging
import signal
import sys

logging.basicConfig(level=logging.INFO, format='%(asctime)s - [BRIDGE] - %(message)s')
 
def shutdown_handler(signal_num, frame):
    logging.info("Received SIGTERM, shutting down bridge...")
    sys.exit(0)

signal.signal(signal.SIGTERM, shutdown_handler)


LISTEN_PORT = 8081       # internal only — nginx is the public listener now
SSH_PORT = 2224
BUF_SIZE = 262144        # bumped from 64KB — I want fewer read/write syscalls during a speedtest burst
SOCK_BUF = 262144        # explicit kernel socket buffer size, same reasoning
IDLE_TIMEOUT = 25
HANDSHAKE_TIMEOUT = 3.0


def optimize_socket(sock):
    if not sock:
        return
    try:
        sock.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
        sock.setsockopt(socket.SOL_SOCKET, socket.SO_KEEPALIVE, 1)
        # I widened these explicitly instead of relying on OS defaults —
        # the occasional 800ms-1500ms ping spikes I saw during sustained
        # speedtest load point to the kernel socket buffer filling up
        # faster than my app-level read/write loop could drain it.
        sock.setsockopt(socket.SOL_SOCKET, socket.SO_RCVBUF, SOCK_BUF)
        sock.setsockopt(socket.SOL_SOCKET, socket.SO_SNDBUF, SOCK_BUF)
    except OSError:
        pass


async def relay(reader, writer, label=""):
    try:
        while True:
            data = await asyncio.wait_for(reader.read(BUF_SIZE), timeout=IDLE_TIMEOUT)
            if not data:
                break
            writer.write(data)
            await writer.drain()
    except asyncio.TimeoutError:
        logging.info(f"Idle timeout on {label}, closing")
    except Exception:
        pass
    finally:
        writer.close()


async def handle_client(client_reader, client_writer):
    peer = client_writer.get_extra_info('peername')
    logging.info(f"New connection from nginx, upstream peer info: {peer}")

    try:
        # I read the fake-WS handshake myself here — nginx's stream
        # module doesn't touch this, it just passes raw bytes through.
        raw = b""
        while b"\r\n\r\n" not in raw:
            chunk = await asyncio.wait_for(client_reader.read(4096), timeout=HANDSHAKE_TIMEOUT)
            if not chunk:
                client_writer.close()
                return
            raw += chunk
            if len(raw) > 16384:
                client_writer.close()
                return

        logging.info(f"RAW REQUEST: {raw!r}")

        header_end = raw.index(b"\r\n\r\n") + 4
        leftover = raw[header_end:]

        if b"upgrade: websocket" in raw.lower():
            client_writer.write(
                b"HTTP/1.1 101 Switching Protocols\r\n"
                b"Upgrade: websocket\r\n"
                b"Connection: Upgrade\r\n\r\n"
            )
            await client_writer.drain()
        else:
            logging.info("No websocket upgrade header found, dropping")
            client_writer.close()
            return

    except asyncio.TimeoutError:
        logging.info("Handshake read timed out, dropping")
        client_writer.close()
        return
    except Exception as e:
        logging.error(f"Handshake error: {e!r}")
        client_writer.close()
        return

    try:
        ssh_reader, ssh_writer = await asyncio.open_connection("127.0.0.1", SSH_PORT)
    except Exception as e:
        logging.error(f"Could not reach dropbear on 127.0.0.1:{SSH_PORT}: {e!r}")
        client_writer.close()
        return

    optimize_socket(client_writer.get_extra_info('socket'))
    optimize_socket(ssh_writer.get_extra_info('socket'))

    if leftover:
        ssh_writer.write(leftover)
        await ssh_writer.drain()

    await asyncio.gather(
        relay(client_reader, ssh_writer, label="client->ssh"),
        relay(ssh_reader, client_writer, label="ssh->client"),
        return_exceptions=True
    )


async def main():
    server = await asyncio.start_server(handle_client, "127.0.0.1", LISTEN_PORT)
    logging.info(f"Bridge listening on 127.0.0.1:{LISTEN_PORT} -> forwarding to 127.0.0.1:{SSH_PORT}")
    async with server:
        await server.serve_forever()


if __name__ == "__main__":
    asyncio.run(main())
