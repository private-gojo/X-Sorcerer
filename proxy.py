#!/usr/bin/env python3
# ============================================================
# GOJO SSH+WS RELAY — INTERNAL BRIDGE
# ============================================================
# Nginx uses the stream module for raw Layer 4 TCP passthrough.
# HTTP and WebSocket parsing remain in this bridge.
# The bridge returns the fake-101 response, then relays traffic to Dropbear.
#
# Rate limiting and IP banning are currently disabled.
# The bridge keeps the transport path minimal during stability validation.
# ============================================================

import asyncio
import os
import socket
import logging
import signal
import sys
from email.utils import formatdate

logging.basicConfig(level=logging.INFO, format='%(asctime)s - [BRIDGE] - %(message)s')
 
def shutdown_handler(signal_num, frame):
    logging.info("Received SIGTERM, shutting down bridge...")
    sys.exit(0)

signal.signal(signal.SIGTERM, shutdown_handler)


LISTEN_PORT = 8081       # internal only — nginx is the public listener now
SSH_PORT = 2224
BUF_SIZE = 262144        # bumped from 64KB — I want fewer read/write syscalls during a speedtest burst
IDLE_TIMEOUT = 600
HANDSHAKE_TIMEOUT = 4.0


def optimize_socket(sock):
    if not sock:
        return
    try:
        sock.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
        sock.setsockopt(socket.SOL_SOCKET, socket.SO_KEEPALIVE, 1)
        sock.setsockopt(socket.IPPROTO_TCP, socket.TCP_KEEPIDLE, 15)
        sock.setsockopt(socket.IPPROTO_TCP, socket.TCP_KEEPINTVL, 10)
        sock.setsockopt(socket.IPPROTO_TCP, socket.TCP_KEEPCNT, 3)
        # Leave receive/send buffer sizing to kernel TCP autotuning.
        # Fixed values can constrain buffers for varying bandwidth-delay products.
    except OSError:
        pass


async def relay(reader, writer, label=""):
    try:
        while True:
            data = await asyncio.wait_for(reader.read(BUF_SIZE), timeout=IDLE_TIMEOUT)
            if not data:
                break
            if writer.is_closing():
                break
            writer.write(data)
            await asyncio.wait_for(writer.drain(), timeout=IDLE_TIMEOUT)
    except asyncio.TimeoutError:
      logging.info(f"Idle timeout on {label}, closing")
    except Exception as e:
      logging.info(f"Relay on {label} ended: {e!r}")
    finally:
        # Half-close the destination write side so reverse traffic can continue.
        try:
            if writer.can_write_eof():
                writer.write_eof()
            else:
                writer.close()
        except (OSError, RuntimeError, AttributeError, NotImplementedError):
            writer.close()

async def handle_client(client_reader, client_writer):
    peer = client_writer.get_extra_info('peername')
    logging.info(f"New connection from nginx, upstream peer info: {peer}")

    try:
        # Read the fake-WS handshake; Nginx stream passes the bytes unchanged.
        raw = b""
        loop = asyncio.get_running_loop()
        handshake_deadline = loop.time() + HANDSHAKE_TIMEOUT
        header_end = -1

        while header_end < 0:
            remaining = handshake_deadline - loop.time()
            if remaining <= 0:
                raise asyncio.TimeoutError

            chunk = await asyncio.wait_for(
                client_reader.read(4096),
                timeout=remaining
            )
            if not chunk:
                client_writer.close()
                return

            raw += chunk
            header_end = raw.find(b"\r\n\r\n")
            if header_end < 0 and len(raw) > 16384:
                client_writer.close()
                return

        header_end += 4
        if header_end > 16384:
            client_writer.close()
            return

        header_lines = raw[:header_end - 4].split(b"\r\n")[1:]
        websocket_upgrade = any(
            name.strip().lower() == b"upgrade"
            and separator
            and any(
                token.strip().lower() == b"websocket"
                for token in value.split(b",")
            )
            for name, separator, value in (
                line.partition(b":") for line in header_lines
            )
        )
        leftover = raw[header_end:]

        if websocket_upgrade:
             # Generate a Date header dynamically for each fake-101 response.
             date_str = formatdate(timeval=None, localtime=False, usegmt=True)
             client_writer.write(
                 b"HTTP/1.1 101 Switching Protocols\r\n"
                 b"Upgrade: websocket\r\n"
                 b"Connection: keep-alive, Upgrade\r\n"
                 b"Date: " + date_str.encode() + b"\r\n"
                 b"Server: Google Frontend\r\n"
                 b"X-Powered-By: X-Sorcerer\r\n"
                 b"\r\n"
             )
             await client_writer.drain()
        else:
            logging.info("No websocket upgrade header found, sending 403 and dropping")
            client_writer.write(
                b"HTTP/1.1 403 Forbidden\r\n"
                b"Content-Length: 0\r\n"
                b"Connection: close\r\n"
                b"\r\n"
            )
            await client_writer.drain()
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
        ssh_reader, ssh_writer = await asyncio.open_connection("127.0.0.1", SSH_PORT, limit=BUF_SIZE)
    except Exception as e:
        logging.error(f"Could not reach dropbear on 127.0.0.1:{SSH_PORT}: {e!r}")
        client_writer.close()
        return

    try:
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
    finally:
        client_writer.close()
        ssh_writer.close()
        await asyncio.gather(
            client_writer.wait_closed(),
            ssh_writer.wait_closed(),
            return_exceptions=True
        )


async def main():
    server = await asyncio.start_server(handle_client, "127.0.0.1", LISTEN_PORT, limit=BUF_SIZE)
    logging.info(f"Bridge listening on 127.0.0.1:{LISTEN_PORT} -> forwarding to 127.0.0.1:{SSH_PORT}")
    async with server:
        await server.serve_forever()


if __name__ == "__main__":
    try:
        import uvloop
    except ImportError:
        uvloop = None

    if uvloop is not None:
        logging.info("uvloop available — using uvloop event loop")
        uvloop.run(main())
    else:
        logging.info("uvloop unavailable — using default asyncio event loop")
        asyncio.run(main())
