#!/usr/bin/env python3
# ============================================================
# GOJO SSH+WS RELAY — INTERNAL BRIDGE
# ============================================================
# I stripped this down on purpose. Nginx now sits in front and
# handles the actual WebSocket handshake (upgrade headers,
# accept key, etc). By the time a connection reaches me here on
# 127.0.0.1:8081, Nginx has already upgraded it — I just need to
# bridge raw bytes straight into dropbear. This removes my own
# hand-rolled handshake parsing from the equation entirely, so
# I can tell for certain whether that parsing was ever the
# actual problem.
#
# I'm leaving out rate limiting and IP banning for now, like I
# said — I want the path clean while I confirm the 503 is fixed.
# I'll bring the anti-DDoS layer back once this is confirmed
# stable.
# ============================================================

import asyncio
import os
import socket
import logging

logging.basicConfig(level=logging.INFO, format='%(asctime)s - [BRIDGE] - %(message)s')

LISTEN_PORT = 8081       # internal only — nginx is the public listener now
SSH_PORT = 2224
BUF_SIZE = 65536
IDLE_TIMEOUT = 300


def optimize_socket(sock):
    if not sock:
        return
    try:
        sock.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
        sock.setsockopt(socket.SOL_SOCKET, socket.SO_KEEPALIVE, 1)
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
        ssh_reader, ssh_writer = await asyncio.open_connection("127.0.0.1", SSH_PORT)
    except Exception as e:
        logging.error(f"Could not reach dropbear on 127.0.0.1:{SSH_PORT}: {e!r}")
        client_writer.close()
        return

    optimize_socket(client_writer.get_extra_info('socket'))
    optimize_socket(ssh_writer.get_extra_info('socket'))

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
