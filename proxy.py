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
import itertools
import time
import resource
from email.utils import formatdate

logging.basicConfig(level=logging.INFO, format='%(asctime)s - [BRIDGE] - %(message)s')
 
def shutdown_handler(signal_num, frame):
    logging.info("Received SIGTERM, shutting down bridge...")
    sys.exit(0)

signal.signal(signal.SIGTERM, shutdown_handler)


LISTEN_PORT = 8081       # internal only — nginx is the public listener now
SSH_PORT = 2224
BUF_SIZE = 262144        # 256 KiB chunks reduce per-transfer read/write overhead.
IDLE_TIMEOUT = 600
HANDSHAKE_TIMEOUT = 4.0

SESSION_IDS = itertools.count(1)
ACTIVE_SESSIONS = 0
TOTAL_SESSIONS = 0
TOTAL_CLIENT_TO_SSH = 0
TOTAL_SSH_TO_CLIENT = 0


def _read_proc_metrics():
    rss_kb = "n/a"
    open_fds = "n/a"
    try:
        with open("/proc/self/status", "r", encoding="ascii") as status_file:
            for line in status_file:
                if line.startswith("VmRSS:"):
                    rss_kb = line.split()[1]
                    break
    except (OSError, IndexError):
        pass
    try:
        open_fds = str(len(os.listdir("/proc/self/fd")))
    except OSError:
        pass
    return rss_kb, open_fds


def _read_cgroup_cpu_metrics():
    quota = "n/a"
    throttled = None
    try:
        with open("/sys/fs/cgroup/cpu.max", "r", encoding="ascii") as cpu_file:
            quota = cpu_file.read().strip()
    except OSError:
        pass
    for path in ("/sys/fs/cgroup/cpu.stat", "/sys/fs/cgroup/cpu/cpu.stat"):
        try:
            with open(path, "r", encoding="ascii") as cpu_file:
                values = dict(line.split() for line in cpu_file if len(line.split()) == 2)
            value = values.get("throttled_usec", values.get("throttled_time"))
            if value is not None:
                throttled = int(value)
            break
        except (OSError, ValueError):
            continue
    return quota, throttled


async def metrics_loop():
    loop = asyncio.get_running_loop()
    interval = 10.0
    next_tick = loop.time() + interval
    previous_wall = time.monotonic()
    usage = resource.getrusage(resource.RUSAGE_SELF)
    previous_cpu = usage.ru_utime + usage.ru_stime
    _, previous_throttled = _read_cgroup_cpu_metrics()

    while True:
        await asyncio.sleep(max(0.0, next_tick - loop.time()))
        now_loop = loop.time()
        lag_ms = max(0.0, now_loop - next_tick) * 1000.0
        now_wall = time.monotonic()
        usage = resource.getrusage(resource.RUSAGE_SELF)
        current_cpu = usage.ru_utime + usage.ru_stime
        elapsed = max(now_wall - previous_wall, 0.001)
        cpu_cores = max(0.0, current_cpu - previous_cpu) / elapsed
        cpu_quota, current_throttled = _read_cgroup_cpu_metrics()
        throttled_delta = "n/a"
        if current_throttled is not None and previous_throttled is not None:
            throttled_delta = str(max(0, current_throttled - previous_throttled))
        rss_kb, open_fds = _read_proc_metrics()

        logging.info(
            "[METRICS] active_sessions=%d total_sessions=%d "
            "client_to_ssh_bytes=%d ssh_to_client_bytes=%d "
            "process_cpu_cores=%.3f cpu_quota=%s cpu_throttled_delta=%s "
            "rss_kb=%s open_fds=%s event_loop_lag_ms=%.2f",
            ACTIVE_SESSIONS, TOTAL_SESSIONS,
            TOTAL_CLIENT_TO_SSH, TOTAL_SSH_TO_CLIENT,
            cpu_cores, cpu_quota, throttled_delta,
            rss_kb, open_fds, lag_ms
        )

        previous_wall = now_wall
        previous_cpu = current_cpu
        previous_throttled = current_throttled
        next_tick += interval
        if next_tick <= now_loop:
            next_tick = now_loop + interval


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


async def relay(reader, writer, label="", stats=None):
    global TOTAL_CLIENT_TO_SSH, TOTAL_SSH_TO_CLIENT
    end_reason = "eof"
    try:
        while True:
            data = await asyncio.wait_for(reader.read(BUF_SIZE), timeout=IDLE_TIMEOUT)
            if not data:
                end_reason = "eof"
                break
            if writer.is_closing():
                end_reason = "destination_closing"
                break
            writer.write(data)
            if stats is not None:
                stats[label + "_bytes"] += len(data)
            if label == "client->ssh":
                TOTAL_CLIENT_TO_SSH += len(data)
            elif label == "ssh->client":
                TOTAL_SSH_TO_CLIENT += len(data)
            await asyncio.wait_for(writer.drain(), timeout=IDLE_TIMEOUT)
    except asyncio.TimeoutError:
        end_reason = "idle_timeout"
        logging.info(f"Idle timeout on {label}, closing")
    except Exception as e:
        end_reason = "error:" + type(e).__name__
        logging.info(f"Relay on {label} ended: {e!r}")
    finally:
        if stats is not None:
            stats[label + "_end"] = end_reason
        # Half-close the destination write side so reverse traffic can continue.
        try:
            if writer.can_write_eof():
                writer.write_eof()
            else:
                writer.close()
        except (OSError, RuntimeError, AttributeError, NotImplementedError):
            writer.close()


async def handle_client(client_reader, client_writer):
    global ACTIVE_SESSIONS, TOTAL_SESSIONS
    session_id = next(SESSION_IDS)
    ACTIVE_SESSIONS += 1
    TOTAL_SESSIONS += 1
    session_stats = {
        "client->ssh_bytes": 0,
        "ssh->client_bytes": 0,
        "client->ssh_end": "not_started",
        "ssh->client_end": "not_started",
        "handshake_ms": None,
        "ssh_connect_ms": None,
        "end_reason": "handler_exit"
    }
    started = time.monotonic()
    try:
        await _handle_client(client_reader, client_writer, session_id, session_stats)
    except Exception as e:
        session_stats["end_reason"] = "handler_error:" + type(e).__name__
        logging.exception("[SESSION %s] Unhandled bridge error: %r", session_id, e)
    finally:
        ACTIVE_SESSIONS -= 1
        duration_ms = (time.monotonic() - started) * 1000.0
        logging.info(
            "[SESSION %s] closed duration_ms=%.1f handshake_ms=%s ssh_connect_ms=%s "
            "client_to_ssh_bytes=%d ssh_to_client_bytes=%d "
            "client_to_ssh_end=%s ssh_to_client_end=%s reason=%s",
            session_id, duration_ms,
            session_stats["handshake_ms"], session_stats["ssh_connect_ms"],
            session_stats["client->ssh_bytes"], session_stats["ssh->client_bytes"],
            session_stats["client->ssh_end"], session_stats["ssh->client_end"],
            session_stats["end_reason"]
        )


async def _handle_client(client_reader, client_writer, session_id, session_stats):
    peer = client_writer.get_extra_info('peername')
    logging.info(f"[SESSION {session_id}] New connection from nginx, upstream peer info: {peer}")
    handshake_started = time.monotonic()

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
                session_stats["end_reason"] = "client_closed_during_handshake"
                client_writer.close()
                return

            raw += chunk
            header_end = raw.find(b"\r\n\r\n")
            if header_end < 0 and len(raw) > 16384:
                session_stats["end_reason"] = "handshake_header_too_large"
                client_writer.close()
                return

        header_end += 4
        session_stats["handshake_ms"] = round((time.monotonic() - handshake_started) * 1000.0, 1)
        if header_end > 16384:
            session_stats["end_reason"] = "handshake_header_too_large"
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
            session_stats["end_reason"] = "http_403_no_websocket_upgrade"
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
        session_stats["end_reason"] = "handshake_timeout"
        logging.info("Handshake read timed out, dropping")
        client_writer.close()
        return
    except Exception as e:
        session_stats["end_reason"] = "handshake_error:" + type(e).__name__
        logging.error(f"Handshake error: {e!r}")
        client_writer.close()
        return

    ssh_connect_started = time.monotonic()
    try:
        ssh_reader, ssh_writer = await asyncio.open_connection("127.0.0.1", SSH_PORT, limit=BUF_SIZE)
        session_stats["ssh_connect_ms"] = round((time.monotonic() - ssh_connect_started) * 1000.0, 1)
    except Exception as e:
        session_stats["end_reason"] = "ssh_connect_error:" + type(e).__name__
        session_stats["ssh_connect_ms"] = round((time.monotonic() - ssh_connect_started) * 1000.0, 1)
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
            relay(client_reader, ssh_writer, label="client->ssh", stats=session_stats),
            relay(ssh_reader, client_writer, label="ssh->client", stats=session_stats),
            return_exceptions=True
        )
        session_stats["end_reason"] = "relay_complete"
    finally:
        client_writer.close()
        ssh_writer.close()
        await asyncio.gather(
            client_writer.wait_closed(),
            ssh_writer.wait_closed(),
            return_exceptions=True
        )


async def main():
    metrics_task = asyncio.create_task(metrics_loop())
    try:
        server = await asyncio.start_server(handle_client, "127.0.0.1", LISTEN_PORT, limit=BUF_SIZE)
        logging.info(f"Bridge listening on 127.0.0.1:{LISTEN_PORT} -> forwarding to 127.0.0.1:{SSH_PORT}")
        async with server:
            await server.serve_forever()
    finally:
        metrics_task.cancel()
        await asyncio.gather(metrics_task, return_exceptions=True)


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
