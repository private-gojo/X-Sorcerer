#!/usr/bin/env python3
import asyncio
import os
import socket

# =========================
# SYSTEM PORT BINDINGS
# =========================
LISTEN_PORT = int(os.environ.get("PORT", "8080"))
SSH_PORT = 2224
BUF_SIZE = 65536

# =========================
# SOCKET OPTIMIZER
# =========================
def optimize_socket(sock: socket.socket):
    if not sock: return
    sock.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
    sock.setsockopt(socket.SOL_SOCKET, socket.SO_RCVBUF, BUF_SIZE)
    sock.setsockopt(socket.SOL_SOCKET, socket.SO_SNDBUF, BUF_SIZE)

# =========================
# RAW TCP RELAY
# =========================
async def relay(reader, writer):
    try:
        while True:
            data = await reader.read(BUF_SIZE)
            if not data:
                break
            writer.write(data)
            await writer.drain()
    except Exception:
        pass
    finally:
        writer.close()

# =========================
# INGRESS TRAFFIC HANDLER
# =========================
async def handle_client(client_reader, client_writer):
    try:
        raw = b""
        while b"\r\n\r\n" not in raw:
            chunk = await asyncio.wait_for(client_reader.read(8192), timeout=3.0)
            if not chunk:
                raise ValueError("Incomplete handshake")
            raw += chunk
            if len(raw) > 16384:
                raise ValueError("Payload too large")

        if b"Upgrade: websocket" in raw or b"upgrade: websocket" in raw:
            response = (
                b"HTTP/1.1 101 Switching Protocols\r\n"
                b"Upgrade: websocket\r\n"
                b"Connection: Upgrade\r\n\r\n"
            )
            client_writer.write(response)
            await client_writer.drain()
        else:
            client_writer.write(b"HTTP/1.1 403 Forbidden\r\n\r\n")
            await client_writer.drain()
            client_writer.close()
            return

        ssh_reader, ssh_writer = await asyncio.open_connection("127.0.0.1", SSH_PORT)
        
        optimize_socket(client_writer.get_extra_info("socket"))
        optimize_socket(ssh_writer.get_extra_info("socket"))

        await asyncio.gather(
            relay(client_reader, ssh_writer),
            relay(ssh_reader, client_writer),
            return_exceptions=True
        )

    except Exception:
        pass
    finally:
        client_writer.close()

async def main():
    server = await asyncio.start_server(handle_client, "0.0.0.0", LISTEN_PORT)
    async with server:
        await server.serve_forever()

if __name__ == "__main__":
    asyncio.run(main())
