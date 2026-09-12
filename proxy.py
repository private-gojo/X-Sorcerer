#!/usr/bin/env python3
import asyncio
import os
import logging

logging.basicConfig(level=logging.INFO, format='%(asctime)s - [X-PROXY] - %(message)s')
LISTEN_PORT = int(os.environ.get("PORT", "8080"))
SSH_PORT = 2224

async def relay(reader, writer):
    try:
        while True:
            data = await reader.read(65536) # Max speed buffer
            if not data: break
            writer.write(data)
            await writer.drain()
    except: pass
    finally:
        writer.close()

async def handle_client(client_reader, client_writer):
    addr = client_writer.get_extra_info('peername')
    # Ignore Google Health Checks silently
    if addr and addr[0] == '169.254.169.126':
        client_writer.close()
        return
        
    try:
        raw = b""
        while b"\r\n\r\n" not in raw:
            chunk = await asyncio.wait_for(client_reader.read(4096), timeout=3.0)
            if not chunk: return
            raw += chunk
            
        # Accept NetMod's "Fake" WebSocket Handshake
        if b"Upgrade: websocket" in raw or b"upgrade: websocket" in raw:
            client_writer.write(
                b"HTTP/1.1 101 Switching Protocols\r\n"
                b"Upgrade: websocket\r\n"
                b"Connection: Upgrade\r\n\r\n"
            )
            await client_writer.drain()
        else:
            client_writer.close()
            return

        ssh_reader, ssh_writer = await asyncio.open_connection("127.0.0.1", SSH_PORT)
        
        await asyncio.gather(
            relay(client_reader, ssh_writer),
            relay(ssh_reader, client_writer),
            return_exceptions=True
        )
    except: pass
    finally:
        client_writer.close()

async def main():
    server = await asyncio.start_server(handle_client, "0.0.0.0", LISTEN_PORT)
    async with server:
        await server.serve_forever()

if __name__ == "__main__":
    asyncio.run(main())

