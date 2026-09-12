#!/usr/bin/env python3
import asyncio
import os
import socket
import logging

# =========================
# DEBUGGING LOGGER SETUP
# =========================
logging.basicConfig(
    level=logging.INFO,
    format='%(asctime)s - [X-PROXY] - %(levelname)s - %(message)s',
    datefmt='%Y-%m-%d %H:%M:%S'
)

LISTEN_PORT = int(os.environ.get("PORT", "8080"))
SSH_PORT = 2224
# Pinaliit ang buffer para iwas ping spike / bufferbloat burst
BUF_SIZE = 8192 

def optimize_socket(sock: socket.socket):
    if not sock: return
    try:
        sock.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
        sock.setsockopt(socket.SOL_SOCKET, socket.SO_KEEPALIVE, 1)
        sock.setsockopt(socket.SOL_SOCKET, socket.SO_RCVBUF, BUF_SIZE)
        sock.setsockopt(socket.SOL_SOCKET, socket.SO_SNDBUF, BUF_SIZE)
    except Exception as e:
        logging.warning(f"Socket optimization warning: {e}")

async def relay(reader, writer, direction):
    try:
        while True:
            data = await reader.read(BUF_SIZE)
            if not data:
                logging.info(f"Connection closed by {direction}")
                break
            writer.write(data)
            await writer.drain()
    except asyncio.TimeoutError:
        logging.warning(f"Relay timeout on {direction}")
    except ConnectionResetError:
        logging.error(f"Connection reset by peer on {direction}")
    except Exception as e:
        logging.error(f"Relay error ({direction}): {e}")
    finally:
        writer.close()

async def handle_client(client_reader, client_writer):
    addr = client_writer.get_extra_info('peername')
    logging.info(f"New client connected from {addr}")
    
    try:
        raw = b""
        # Added explicit timeout para hindi mag-hang ang thread
        while b"\r\n\r\n" not in raw:
            chunk = await asyncio.wait_for(client_reader.read(8192), timeout=5.0)
            if not chunk:
                raise ValueError("Incomplete handshake payload")
            raw += chunk
            if len(raw) > 16384:
                raise ValueError("Payload size limit exceeded")

        if b"Upgrade: websocket" in raw or b"upgrade: websocket" in raw:
            response = (
                b"HTTP/1.1 101 Switching Protocols\r\n"
                b"Upgrade: websocket\r\n"
                b"Connection: Upgrade\r\n\r\n"
            )
            client_writer.write(response)
            await client_writer.drain()
            logging.info(f"WebSocket handshake successful for {addr}")
        else:
            logging.warning(f"Invalid payload/Not a websocket request from {addr}")
            client_writer.write(b"HTTP/1.1 403 Forbidden\r\n\r\n")
            await client_writer.drain()
            client_writer.close()
            return

        ssh_reader, ssh_writer = await asyncio.open_connection("127.0.0.1", SSH_PORT)
        
        optimize_socket(client_writer.get_extra_info("socket"))
        optimize_socket(ssh_writer.get_extra_info("socket"))

        await asyncio.gather(
            relay(client_reader, ssh_writer, "Client->SSH"),
            relay(ssh_reader, client_writer, "SSH->Client"),
            return_exceptions=True
        )

    except asyncio.TimeoutError:
        logging.error(f"Handshake timeout from {addr}")
    except Exception as e:
        logging.error(f"Client handler error ({addr}): {e}")
    finally:
        logging.info(f"Terminating connection for {addr}")
        client_writer.close()

async def main():
    server = await asyncio.start_server(handle_client, "0.0.0.0", LISTEN_PORT)
    logging.info(f"X-Sorcerer Python Proxy started on port {LISTEN_PORT}")
    async with server:
        await server.serve_forever()

if __name__ == "__main__":
    asyncio.run(main())
