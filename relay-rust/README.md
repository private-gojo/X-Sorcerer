# badvpn-udpgw (Rust)

A drop-in replacement for the C [`badvpn-udpgw`](https://github.com/ambrop72/badvpn/tree/master/udpgw)
— the UDP gateway that tun2socks-style clients (HTTP Injector, HTTP Custom, NetMod, …) use to carry
UDP over an SSH/TCP tunnel — reimplemented in Rust on top of [tokio](https://tokio.rs).

Same protocol, same flags, same defaults — with **one deliberate exception**: a UDP connection that
goes 120 s without a packet is closed ([why](#why-idle-udp-connections-are-closed-after-120-s)).
Point your existing setup at it and nothing on the client side changes.

```
badvpn-udpgw --listen-addr 127.0.0.1:7300 --max-clients 10000 --max-connections-for-client 10
```

## Why

`badvpn` has been unmaintained for years and its build (CMake, NSS optional, BLog code generation) is
heavier than the tiny daemon most people actually run from it. This is that daemon alone:

- **Protocol-complete server.** Keepalive, rebind, transparent DNS, IPv4 and IPv6 destinations,
  full-cone UDP sockets, per-client connection cap with LRU eviction, 20 s client disconnect timer —
  every server-side behaviour of the original, checked line by line against the C source.
- **Tested.** 31 unit and end-to-end tests, including byte-exact framing (the udpgw wire format mixes
  little-endian and big-endian fields — the one place where "just port it" goes wrong) and a real
  round trip through a UDP echo server.
- **Small.** ~1 750 lines including tests and comments, 5 dependencies, one static binary.
- **Fixes ephemeral port exhaustion, on by default** — a failure the C original hits on busy
  gateways, where kernel CPU spikes and the host's own DNS stops working; see
  [below](#why-idle-udp-connections-are-closed-after-120-s).
- **Proven in production.** This code has been serving real tunnel traffic (games, DNS, streaming)
  on a fleet of servers since mid-2026 before being extracted into a standalone tool.

## Download

Static binaries (musl, no dependencies — run on any Linux distribution) for **x86_64** and
**aarch64/ARM64** are attached to every release, each one built and tested natively on its own
architecture by CI:

https://github.com/Dean6767/RustBadVpnUdpgw/releases/latest

```
tar -xzf badvpn-udpgw-v*-x86_64-linux-musl.tar.gz     # or -aarch64-
cd badvpn-udpgw-v*-x86_64-linux-musl
./badvpn-udpgw --listen-addr 127.0.0.1:7300
```

## Build from source

```
cargo build --release
# → target/release/badvpn-udpgw
```

For a fully static binary: `rustup target add x86_64-unknown-linux-musl` and
`cargo build --release --target x86_64-unknown-linux-musl`.

Any current stable Rust toolchain works (`rustup` default; edition 2024, so Rust 1.88 or newer). Linux is the primary target; it also
compiles on macOS and Windows (transparent DNS there needs `--dns-server`, since there is no
`/etc/resolv.conf`).

## Usage

```
badvpn-udpgw --listen-addr <ADDR> [--listen-addr <ADDR> ...] [options]
```

| Flag | Default | Same as C original? | Meaning |
|---|---|---|---|
| `--listen-addr <addr>` | *(required)* | ✅ (repeatable, max 16) | TCP address to accept clients on, e.g. `127.0.0.1:7300`, `[::1]:7300` |
| `--max-clients <n>` | **3** | ✅ | Simultaneous clients (TCP connections). Extra clients are **refused**, not queued |
| `--max-connections-for-client <n>` | **256** | ✅ | UDP connections per client; above it the least recently used one is closed |
| `--udp-mtu <bytes>` | 65520 | ✅ | Largest UDP payload accepted from a client |
| `--client-socket-sndbuf <bytes>` | 1048576 | ✅ | `SO_SNDBUF` of each client's TCP socket; `0` leaves the OS default |
| `--loglevel <0-5\|none\|error\|warning\|notice\|info\|debug>` | `info` | ✅ (see note) | Log verbosity |
| `--dns-server <addr>` | *(from `/etc/resolv.conf`)* | ➕ extension | DNS server used for packets the client flags as DNS |
| `--max-total-connections <n>` | 0 = unlimited | ➕ extension | Process-wide cap on open UDP sockets, across all clients |
| `--udp-idle-timeout <seconds>` | **120** | ➕ extension — **on by default** | Close a UDP connection after this long without a datagram in either direction, freeing its port. `0` = never (the C original's behaviour). [Why it is on](#why-idle-udp-connections-are-closed-after-120-s) |
| `--help`, `--version` | | ✅ | |

> ⚠️ **`--max-clients` defaults to 3** because the original was designed to serve one local
> `tun2socks`. If you run this as a shared gateway behind an SSH server (the usual setup), raise it —
> the fourth client would otherwise be refused with `maximum number of clients reached`.

Log level note: badvpn's `notice` and `info` map to the same level here; the default matches the
original's (`info`: client connect/disconnect and timeouts are logged, per-connection details only
at `debug`). Logs go to stderr with a timestamp.

### Not implemented

These flags of the C original are **not** accepted (the process exits with a usage error), so a
misconfiguration is loud rather than silent:

- `--logger syslog` (only stderr; let systemd/journald capture it)
- `--channel-loglevel` (badvpn's per-module BLog channels have no equivalent)
- `--local-udp-addrs`, `--local-udp-ip6-addrs`, `--unique-local-ports` (binding outbound UDP sockets
  to specific local addresses/ports — the OS picks an ephemeral port, as the original does when these
  are omitted)

### systemd

See [`contrib/badvpn-udpgw.service`](contrib/badvpn-udpgw.service). Raise `LimitNOFILE`: every UDP
connection is one file descriptor, and `--max-connections-for-client × clients` adds up quickly.

## Why idle UDP connections are closed after 120 s

This is the one default that differs from the C original, which never closes a quiet UDP
connection. The reason is a failure that the original hits on busy gateways.

### The problem: ephemeral port exhaustion

Every UDP connection of every client is one socket bound to one **ephemeral port**. In the C
original a socket is never closed just because it went quiet: it lives until the client
disconnects, or until the client opens more than `--max-connections-for-client` connections and the
least recently used one is evicted. A long-lived client therefore keeps **close to its maximum
number of sockets open all the time**, most of them dead (finished QUIC flows and the like).

The whole host shares one port range (`sysctl net.ipv4.ip_local_port_range`, typically 28,232
ports by default, or 55,536 if widened to `10000 65535`). When `clients × open connections` reaches
it:

- every new connection makes the kernel **scan the entire range** under a lock looking for a free
  port (`udp_lib_get_port` in a kernel backtrace) — **kernel CPU jumps to 70-80%**, with no increase
  in traffic;
- the host's **own DNS resolution starts failing**, because the resolver also needs an ephemeral
  UDP port.

It is a cliff, not a slope: at 97% of the range everything is fine, at 100% it falls over.

### The fix

Any UDP connection that has seen no datagram **in either direction** for 120 s is closed and its
port goes back to the system (checked about once a second, as long as the client sends anything —
keepalives included; in practice a connection is closed 120-141 s after its last datagram).

### Why it is on by default instead of opt-in

- **The failure gives no warning and takes the whole host down.** Nothing degrades gradually that
  an operator could notice in time: the gateway works until the port range is full, and then the
  kernel CPU spikes and DNS fails for every service on the machine, not only for the gateway.
- **Clients do not notice the timeout.** The udpgw protocol has no "closed" message. If a client
  sends on that connection again, it is simply reopened — with a new source port, which is exactly
  what the client's own mobile carrier already does to idle UDP flows (usually after 30-60 s).
- **Real traffic is never cut.** Games, calls and streams in progress never go 120 s without a
  packet, and packets in *either* direction keep a connection alive. 120 s is the minimum NAT UDP
  timeout recommended by RFC 4787.
- **Memory drops too:** each open connection holds a 64 KB receive buffer.

To get the C original's behaviour back, use `--udp-idle-timeout 0`.

### Measured in production

`--max-connections-for-client 50`, port range 10000-65535, on the same gateway at the same time of
day with the same number of clients online (~1,480):

| | Without the timeout | With `--udp-idle-timeout 120` |
|---|---|---|
| UDP sockets at the evening peak | 53,500-54,600 (97-98% of the range; a sibling gateway hit 55,535 = 100%) | **18,885 (34%)** |
| Kernel CPU (`sy`) when the range filled up | 70-80% | 29% |
| Resident memory of the process | 4.27 GiB | **1.86 GiB** |

To check a host: compare `ss -uan | wc -l` with the size of the port range.

Known limitation: a datagram from **any** source refreshes the timer (the sockets are full-cone), so
a remote host deliberately spraying the gateway's port range can keep sockets from expiring. That
only brings back the behaviour without the timeout; it is not worse than it.

## Behaviour, and where it differs from the C original

Everything below was verified against `udpgw/udpgw.c`, `udpgw/udpgw.h` and
`protocol/{packetproto,udpgw_proto}.h` in the badvpn repository.

**Identical:**

- Wire format: 2-byte **little-endian** length prefix (body only), then `flags:u8`,
  `conid:u16 LE`, then for data packets the address in **network order** (4 or 16 bytes + port),
  then the payload. IPv6 is signalled by the `0x08` flag, not by length.
- `FLAG_KEEPALIVE` (0x01) resets the disconnect timer and is otherwise ignored;
  `FLAG_REBIND` (0x02) closes and reopens the conid's socket; `FLAG_DNS` (0x04) sends the
  datagram to the system DNS server *but the reply still reports the destination the client asked
  for*; a changed destination on an existing conid also reopens it.
- Full-cone: the UDP socket is bound, never connected; replies from any origin are accepted and
  attributed to the conid. The reply header always carries the original destination.
- Per-client connection cap with **LRU eviction** (recency bumped on both directions).
- 20 s of complete silence from a client (no data, no keepalive) disconnects it and closes all of
  its UDP sockets. A client above `--max-clients` is refused immediately.
- A payload above `--udp-mtu` is rejected (`too much data`); a malformed packet (short header,
  truncated address) is logged and **ignored** — the client keeps its connection.
- The system DNS server is the first IPv4 `nameserver` of `/etc/resolv.conf`, re-read every 2 s.
  With no server available, DNS-flagged packets fall through to the destination the client asked
  for (as the original does).

**Different (deliberately):**

- A UDP connection silent for 120 s in both directions is closed (`--udp-idle-timeout`; `0`
  restores the original's "never") — see
  [above](#why-idle-udp-connections-are-closed-after-120-s). The original keeps it until the
  client disconnects or LRU eviction.
- If re-reading `/etc/resolv.conf` fails, the **last known good** DNS server is kept. The original
  clears it and starts failing DNS until the file is readable again.
- Downlink buffering: each client has a bounded queue of frames towards its TCP socket (256), and a
  slow client back-pressures its own UDP sockets. The original keeps a one-packet buffer per
  connection and drops on overflow. Neither affects other clients.
- A client that vanished without FIN/RST is dropped 1 s after its disconnect timer fires instead of
  waiting for the kernel's TCP send timeout, so its slot under `--max-clients` is freed promptly.
- The process is multi-threaded (tokio's default runtime); the original is single-threaded.
  Behaviour per client is unchanged.

## Tests

```
cargo test
```

## License

MIT — see [LICENSE](LICENSE). This is an independent reimplementation of the protocol; it does not
contain code from badvpn (which is BSD-3-Clause).
