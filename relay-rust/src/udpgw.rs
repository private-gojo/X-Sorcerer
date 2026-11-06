//! The badvpn `udpgw` protocol (github.com/ambrop72/badvpn, `udpgw/` + `protocol/`) — wire format
//! and server logic.
//!
//! A client (normally tun2socks) opens ONE TCP connection to the gateway and multiplexes all of its
//! UDP traffic over it. Every UDP "connection" is identified by a 16-bit `conid` chosen by the
//! client; the server keeps one UDP socket per conid and relays datagrams in both directions.
//!
//! ENDIANNESS (verified byte by byte against `protocol/{packetproto,udpgw_proto}.h`) — a single
//! message MIXES both byte orders, so this cannot be copied from an "everything big-endian"
//! protocol:
//!   • packetproto length prefix → uint16 **LITTLE-ENDIAN**; counts only the body that follows
//!     (it does NOT include the 2 prefix bytes themselves).
//!   • `conid` (in `udpgw_header`) → uint16 **LITTLE-ENDIAN** (badvpn converts it with `ltoh16`).
//!   • `addr_ip` / `addr_port` (in the addr structs) → **BIG-ENDIAN / network order** (copied
//!     straight out of the BAddr, no conversion).
//! IPv4 vs IPv6 is decided by the IPV6 bit in the flags byte — NOT by the length.
//!
//! On-the-wire layout of one message (the block after the prefix, i.e. what `parse_msg` receives):
//!   [flags:u8][conid:u16 LE]( [addr_ip][addr_port BE]  — data packets only )[ UDP payload ]
//! The structs are B_PACKED (no padding): header = 3 B, addr_v4 = 6 B, addr_v6 = 18 B.

use std::collections::HashMap;
use std::io::{self, ErrorKind};
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use log::{debug, error, info, warn};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::UdpSocket;
use tokio::sync::{OwnedSemaphorePermit, Semaphore, mpsc};
use tokio::task::JoinHandle;

// ── Flags (protocol/udpgw_proto.h) ──
pub const FLAG_KEEPALIVE: u8 = 0x01; // 1<<0 — header only (3 B); resets the disconnect timer, nothing else
pub const FLAG_REBIND: u8 = 0x02; // 1<<1 — force close + reopen of that conid's socket (fresh local port)
pub const FLAG_DNS: u8 = 0x04; // 1<<2 — ignore the destination and send to the system DNS server
pub const FLAG_IPV6: u8 = 0x08; // 1<<3 — address is udpgw_addr_ipv6 (18 B) instead of ipv4 (6 B)

// ── Fixed sizes (B_PACKED, no padding) ──
pub const HDR_LEN: usize = 3; // flags(1) + conid(2)
pub const ADDR_V4_LEN: usize = 6; // addr_ip(4) + addr_port(2)
pub const ADDR_V6_LEN: usize = 18; // addr_ip(16) + addr_port(2)

// ── Limits (udpgw/udpgw.h — server defaults) ──
/// `DEFAULT_UDP_MTU`: the largest UDP payload accepted from a client ("too much data" above it).
pub const DEFAULT_UDP_MTU: usize = 65_520;
/// Largest downlink payload that still fits the u16 packetproto prefix together with the
/// header (3) + an IPv6 address (18).
const MAX_DL_PAYLOAD: usize = u16::MAX as usize - HDR_LEN - ADDR_V6_LEN; // 65514
/// `CLIENT_DISCONNECT_TIMEOUT` (20000 ms): idle timeout of the WHOLE client connection, NOT of a
/// single UDP connection. 20 s without anything from the client (not even a keepalive) tears the
/// client down together with all of its UDP sockets. badvpn has NO per-connection idle timeout —
/// a UDP socket lives until LRU eviction / rebind / socket error / client disconnect. Here each
/// connection ALSO has one, on by default: see `DEFAULT_IDLE_TIMEOUT`.
const CLIENT_DISCONNECT: Duration = Duration::from_secs(20);
/// Default per-connection idle timeout (`--udp-idle-timeout`, `Config::idle_timeout`) — the ONE
/// default that deliberately differs from the C original, which has none.
///
/// Why on by default: without it, every client keeps up to `--max-connections-for-client` sockets
/// (= ephemeral ports) open for as long as it stays connected, mostly dead ones. On a busy gateway
/// the total reaches the size of the host's port range, and from that point on every `bind()` scans
/// the whole range under a kernel lock: kernel CPU at 70-80% and the host's own DNS failing. It
/// gives no warning (fine at 97% of the range, down at 100%), and the timeout is invisible to
/// clients — the protocol has no "closed" message; a connection that talks again is simply reopened
/// with a new source port, which is what mobile carriers' NATs already do to idle UDP (30-60 s).
/// 120 s is the minimum NAT UDP timeout recommended by RFC 4787. `0` restores badvpn's behaviour.
pub const DEFAULT_IDLE_TIMEOUT: Duration = Duration::from_secs(120);
/// Backpressure of the per-client aggregated downlink queue (frames waiting for the writer).
const DL_CHAN_CAP: usize = 256;
/// Read buffer in front of each client's stream (see `serve`). One max-size message plus prefix,
/// = `PACKETPROTO_ENCLEN(udpgw_mtu)` that badvpn allocates per client for the same purpose.
const UPLINK_BUF: usize = 2 + u16::MAX as usize;
/// `DNS_UPDATE_TIME` (2000 ms): how often `/etc/resolv.conf` is re-read.
const DNS_REFRESH: Duration = Duration::from_secs(2);
/// How long the writer gets to flush queued downlink frames after the client is torn down. After
/// that it is aborted: a peer that vanished without FIN/RST would otherwise keep the client slot
/// busy until the kernel gives up on the TCP send.
const WRITER_GRACE: Duration = Duration::from_secs(1);

static ACTIVE_UDP_CONNECTIONS: AtomicU64 = AtomicU64::new(0);
static UDP_UP_PACKETS: AtomicU64 = AtomicU64::new(0);
static UDP_UP_BYTES: AtomicU64 = AtomicU64::new(0);
static UDP_DOWN_PACKETS: AtomicU64 = AtomicU64::new(0);
static UDP_DOWN_BYTES: AtomicU64 = AtomicU64::new(0);
static UDP_SOCKET_ERRORS: AtomicU64 = AtomicU64::new(0);
static UDP_SEND_ERRORS: AtomicU64 = AtomicU64::new(0);
static UDP_IDLE_CLOSED: AtomicU64 = AtomicU64::new(0);
static UDP_LRU_EVICTIONS: AtomicU64 = AtomicU64::new(0);

pub fn stats_line(active_clients: usize) -> String {
    format!(
        "active_clients={} active_udp_connections={} udp_up_packets={} udp_up_bytes={} \
         udp_down_packets={} udp_down_bytes={} socket_errors={} send_errors={} \
         idle_closed={} lru_evictions={}",
        active_clients,
        ACTIVE_UDP_CONNECTIONS.load(Ordering::Relaxed),
        UDP_UP_PACKETS.load(Ordering::Relaxed),
        UDP_UP_BYTES.load(Ordering::Relaxed),
        UDP_DOWN_PACKETS.load(Ordering::Relaxed),
        UDP_DOWN_BYTES.load(Ordering::Relaxed),
        UDP_SOCKET_ERRORS.load(Ordering::Relaxed),
        UDP_SEND_ERRORS.load(Ordering::Relaxed),
        UDP_IDLE_CLOSED.load(Ordering::Relaxed),
        UDP_LRU_EVICTIONS.load(Ordering::Relaxed),
    )
}

/// Server settings shared by every client.
#[derive(Debug, Clone)]
pub struct Config {
    /// `--max-connections-for-client`: UDP connections (conids) per client. Exceeding it closes the
    /// least recently used one (badvpn semantics: evict, don't reject).
    pub max_connections: usize,
    /// `--udp-mtu`: largest UDP payload accepted from a client.
    pub udp_mtu: usize,
    /// DNS server used for packets flagged `FLAG_DNS`. `None` = first `nameserver` of
    /// `/etc/resolv.conf`, re-read every 2 s.
    pub dns_server: Option<SocketAddr>,
    /// Process-wide cap on open UDP sockets (one permit per socket). Use `Semaphore::MAX_PERMITS`
    /// for "unlimited".
    pub socket_permits: Arc<Semaphore>,
    /// `--udp-idle-timeout`: close a UDP connection (and free its ephemeral port) after this long
    /// without a datagram in either direction. `Duration::ZERO` = never (badvpn behaviour).
    ///
    /// Why it exists: every connection holds one ephemeral port, and without a timeout a client
    /// keeps up to `max_connections` sockets open for as long as it stays connected, most of them
    /// dead (finished QUIC flows and the like). With many clients the total can reach the size of
    /// `net.ipv4.ip_local_port_range`; from then on every `bind()` scans the whole range under a
    /// kernel lock (`udp_lib_get_port`) — kernel CPU goes to 70-80% and the host's own DNS
    /// resolution starts failing. Seen in production: ~1,400 clients with 50 connections each
    /// filled 55,535 of 55,536 ports; a 120 s timeout brought the evening peak down to ~19k.
    pub idle_timeout: Duration,
}

/// One udpgw message, already de-framed (without the packetproto length prefix). Borrows the
/// payload from the source buffer (zero-copy).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UdpgwMsg<'a> {
    pub flags: u8,
    pub conid: u16,
    /// Destination of the datagram. `None` for a keepalive (which carries no address).
    pub dest: Option<SocketAddr>,
    /// Raw UDP payload. Empty for a keepalive.
    pub payload: &'a [u8],
}

impl UdpgwMsg<'_> {
    pub fn is_keepalive(&self) -> bool {
        self.flags & FLAG_KEEPALIVE != 0
    }
    pub fn is_rebind(&self) -> bool {
        self.flags & FLAG_REBIND != 0
    }
    pub fn is_dns(&self) -> bool {
        self.flags & FLAG_DNS != 0
    }
}

/// Parses ONE udpgw block (the content AFTER the packetproto length prefix). Does not allocate —
/// it borrows the payload from `block`.
pub fn parse_msg(block: &[u8]) -> io::Result<UdpgwMsg<'_>> {
    if block.len() < HDR_LEN {
        return Err(io::Error::new(ErrorKind::InvalidData, "missing header"));
    }
    let flags = block[0];
    let conid = u16::from_le_bytes([block[1], block[2]]); // conid is LITTLE-ENDIAN
    let rest = &block[HDR_LEN..];

    // Keepalive: just the header (flags + conid), no address, no payload. badvpn ignores anything
    // that follows; so do we.
    if flags & FLAG_KEEPALIVE != 0 {
        return Ok(UdpgwMsg {
            flags,
            conid,
            dest: None,
            payload: &[],
        });
    }

    // Data packet: address (IPv4 or IPv6 according to the IPV6 flag, NOT the length) + payload.
    let (dest, addr_len) = if flags & FLAG_IPV6 != 0 {
        if rest.len() < ADDR_V6_LEN {
            return Err(io::Error::new(
                ErrorKind::InvalidData,
                "missing ipv6 address",
            ));
        }
        let octets = <[u8; 16]>::try_from(&rest[0..16]).unwrap(); // 16 bytes network order = natural order
        let ip = Ipv6Addr::from(octets);
        let port = u16::from_be_bytes([rest[16], rest[17]]); // port BIG-ENDIAN
        (SocketAddr::new(ip.into(), port), ADDR_V6_LEN)
    } else {
        if rest.len() < ADDR_V4_LEN {
            return Err(io::Error::new(
                ErrorKind::InvalidData,
                "missing ipv4 address",
            ));
        }
        let ip = Ipv4Addr::new(rest[0], rest[1], rest[2], rest[3]); // 4 octets network order
        let port = u16::from_be_bytes([rest[4], rest[5]]); // port BIG-ENDIAN
        (SocketAddr::new(ip.into(), port), ADDR_V4_LEN)
    };

    Ok(UdpgwMsg {
        flags,
        conid,
        dest: Some(dest),
        payload: &rest[addr_len..],
    })
}

/// Builds a COMPLETE downlink frame (server → client), packetproto length prefix included, ready
/// to be written to the client stream:
///   [len:u16 LE][flags:u8][conid:u16 LE][addr_ip][addr_port:u16 BE][payload]
///
/// `report` = the SOURCE address reported to the client: the destination the client originally
/// asked for (badvpn ignores the real origin of the reply, see `downlink_task`). The client matches
/// the reply to its own socket by `conid`, which echoes the value it sent.
///
/// Precondition: `payload.len()` must fit the u16 prefix together with header + addr. The caller
/// caps datagrams at `MAX_DL_PAYLOAD`, so `body_len <= 65535`. A violation trips the
/// `debug_assert` in debug builds; in release the `as u16` would truncate — hence the cap upstream.
pub fn build_downlink_frame(conid: u16, report: SocketAddr, payload: &[u8]) -> Vec<u8> {
    let (flags, addr_len) = match report {
        SocketAddr::V4(_) => (0u8, ADDR_V4_LEN),
        SocketAddr::V6(_) => (FLAG_IPV6, ADDR_V6_LEN),
    };
    let body_len = HDR_LEN + addr_len + payload.len();
    debug_assert!(
        body_len <= u16::MAX as usize,
        "udpgw frame exceeds the u16 packetproto prefix"
    );

    let mut out = Vec::with_capacity(2 + body_len);
    // packetproto prefix: length of the BODY, LITTLE-ENDIAN, NOT counting these 2 bytes.
    out.extend_from_slice(&(body_len as u16).to_le_bytes());
    out.push(flags);
    out.extend_from_slice(&conid.to_le_bytes()); // conid LITTLE-ENDIAN
    match report {
        SocketAddr::V4(a) => {
            out.extend_from_slice(&a.ip().octets()); // 4 octets network order
            out.extend_from_slice(&a.port().to_be_bytes()); // port BIG-ENDIAN
        }
        SocketAddr::V6(a) => {
            out.extend_from_slice(&a.ip().octets()); // 16 bytes network order
            out.extend_from_slice(&a.port().to_be_bytes()); // port BIG-ENDIAN
        }
    }
    out.extend_from_slice(payload);
    out
}

/// Reads packetproto-framed udpgw messages from the read side of the client stream.
pub struct FrameReader<R> {
    rd: R,
}

impl<R: AsyncRead + Unpin> FrameReader<R> {
    pub fn new(rd: R) -> Self {
        FrameReader { rd }
    }

    /// Reads the next udpgw block (WITHOUT the length prefix — what `parse_msg` expects).
    /// `Ok(None)` = clean EOF on a message boundary (the client closed normally). `Err` = I/O error
    /// or EOF in the middle of a message → the caller drops the client.
    ///
    /// The prefix itself is not validated here: a block shorter than the header is returned as is
    /// and rejected by `parse_msg` ("missing header"), so the client keeps its connection — that is
    /// what badvpn does, which only drops the client on decoder (framing) errors.
    pub async fn next_msg(&mut self) -> io::Result<Option<Vec<u8>>> {
        let mut lenb = [0u8; 2];
        // read_exact on the prefix tells a clean EOF (client closed between messages → None) from
        // an EOF halfway through (truncated message → error, in the read_exact of the body below).
        // Only EOF before the first prefix byte is clean; a partial prefix is a framing error.
        if self.rd.read(&mut lenb[..1]).await? == 0 {
            return Ok(None);
        }
        self.rd.read_exact(&mut lenb[1..]).await?;
        let len = u16::from_le_bytes(lenb) as usize; // prefix LITTLE-ENDIAN; <= 65535 by construction
        let mut block = vec![0u8; len];
        self.rd.read_exact(&mut block).await?; // EOF here = truncated message = error (not None)
        Ok(Some(block))
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Server: per-client state machine + full-cone UDP sockets.
//
// Fidelity to badvpn-udpgw (checked against the C source):
//   • full-cone: the socket is bound to the destination's address family and NOT connected —
//     recv_from accepts ANY origin, and the real origin is IGNORED. The reply header always reports
//     the ORIGINAL destination the client asked for (`orig_dest`), so the client can match the
//     reply by conid.
//   • per-client connection cap with LRU EVICTION (not reject) — recency is bumped on uplink AND
//     downlink. On overflow the least recently used connection is closed and the new one opened.
//   • DIFFERENT on purpose: a per-connection idle timeout (`Config::idle_timeout`,
//     `--udp-idle-timeout`, 120 s by default; 0 = badvpn's "never") — see DEFAULT_IDLE_TIMEOUT.
//   • transparent DNS (FLAG_DNS): sent to the system DNS server, but the reply reports the original
//     destination.
// ─────────────────────────────────────────────────────────────────────────────

/// One live UDP connection (one `conid`): its own full-cone socket + its downlink task.
struct UdpConn {
    sock: Arc<UdpSocket>,
    /// ORIGINAL destination requested by the client. It is the comparison key (rebind / changed
    /// destination) and the address reported in the downlink header (= badvpn, which ignores the
    /// real origin of the datagram).
    orig_dest: SocketAddr,
    /// REAL send destination: the DNS server for a DNS packet; = orig_dest otherwise.
    send_dest: SocketAddr,
    /// Recency for the LRU (monotonic per-client tick). Bumped on uplink and on downlink.
    last_use: Arc<AtomicU64>,
    /// Milliseconds (since the client's `epoch`) of the last datagram in EITHER direction — the
    /// clock of the idle timeout. Shared with the downlink task, which stamps it on every receive.
    last_activity_ms: Arc<AtomicU64>,
    /// Generation (unique tick taken at open). Identifies THIS socket instance: the socket-error
    /// cleanup only removes the entry if the conid still points at this generation — otherwise it
    /// was reopened (conid reuse) and the new socket must be left alone.
    gen_id: u64,
    down: JoinHandle<()>,
    /// Permit of the process-wide socket cap, returned on drop.
    _permit: OwnedSemaphorePermit,
}

impl Drop for UdpConn {
    fn drop(&mut self) {
        // LRU eviction / rebind / changed destination / client gone → kill the downlink. The socket
        // and the permit are released right after (field drops).
        ACTIVE_UDP_CONNECTIONS.fetch_sub(1, Ordering::Relaxed);
        self.down.abort();
    }
}

#[inline]
fn bump(last_use: &AtomicU64, tick: &AtomicU64) {
    last_use.store(tick.fetch_add(1, Ordering::Relaxed), Ordering::Relaxed);
}

// ── Optional per-connection idle timeout (see `Config::idle_timeout`) ──

/// Milliseconds since the client's `epoch` (monotonic; immune to wall-clock adjustments).
#[inline]
fn ms_since(epoch: tokio::time::Instant) -> u64 {
    epoch.elapsed().as_millis() as u64
}

/// Records "a datagram passed just now" on the connection's idle clock. Called on uplink and on
/// downlink. `fetch_max`, not `store`: the two directions run in different tasks, and a timestamp
/// read before but written after (a preemption between the two) must not move the clock backwards.
#[inline]
fn stamp_activity(last_activity_ms: &AtomicU64, epoch: tokio::time::Instant) {
    last_activity_ms.fetch_max(ms_since(epoch), Ordering::Relaxed);
}

/// Pure decision: has the connection been silent (both directions) for `idle_ms` or more?
/// `idle_ms == 0` = timeout disabled: never idle. `saturating_sub` covers the benign race where the
/// downlink stamped AFTER `now_ms` was read.
fn is_idle(last_ms: u64, now_ms: u64, idle_ms: u64) -> bool {
    idle_ms > 0 && now_ms.saturating_sub(last_ms) >= idle_ms
}

/// How often (at most) the sweep runs: min(1 s, timeout/4). With a production-sized timeout it is
/// always 1 s, which bounds how late a connection is closed (plus the wait for the client's next
/// message — at most 20 s on a live client).
fn sweep_interval(idle: Duration) -> Duration {
    std::cmp::min(Duration::from_secs(1), idle / 4)
}

/// Time to sweep? Never with the timeout disabled; otherwise once the last sweep is `interval` ago.
fn should_sweep(idle_ms: u64, since_last: Duration, interval: Duration) -> bool {
    idle_ms > 0 && since_last >= interval
}

/// Closes the client's idle connections and returns how many. `retain` DROPS each `UdpConn`: the
/// Drop aborts the downlink task (whose end releases the last `Arc` of the socket, giving the port
/// back to the system) and returns the socket permit.
fn sweep_idle(conns: &mut HashMap<u16, UdpConn>, now_ms: u64, idle_ms: u64) -> usize {
    let before = conns.len();
    conns.retain(|_, c| !is_idle(c.last_activity_ms.load(Ordering::Relaxed), now_ms, idle_ms));
    let closed = before - conns.len();
    UDP_IDLE_CLOSED.fetch_add(closed as u64, Ordering::Relaxed);
    closed
}

/// Serves one client over `stream` (its TCP connection — or anything `AsyncRead + AsyncWrite`).
/// Returns when the client closes, stays silent for 20 s, or the stream fails. `client` is the
/// label used in log lines (normally the peer address).
pub async fn serve<S>(stream: S, client: String, cfg: Arc<Config>)
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let client: Arc<str> = client.into();
    // Idle-timeout clock: milliseconds since this client connected. The sweep runs inside this
    // loop (no extra task), at most every `interval` — the loop wakes on every client message, and a
    // live client sends something at least every 20 s (CLIENT_DISCONNECT).
    let epoch = tokio::time::Instant::now();
    // Saturate instead of truncating: `as u64` on the u128 would wrap an absurd flag value around
    // into a tiny timeout (or 0 = disabled) without any warning.
    let idle_ms = u64::try_from(cfg.idle_timeout.as_millis()).unwrap_or(u64::MAX);
    let interval = sweep_interval(cfg.idle_timeout);
    let mut last_sweep = epoch;

    let (rd, wr) = tokio::io::split(stream);
    let (dl_tx, dl_rx) = mpsc::channel::<Vec<u8>>(DL_CHAN_CAP);
    // Downlink → uplink signal to close a conid whose UDP socket failed (e.g. ICMP port
    // unreachable), = badvpn's connection_dgram_handler_event. Carries (conid, generation).
    let (cleanup_tx, mut cleanup_rx) = mpsc::channel::<(u16, u64)>(64);
    let mut writer = tokio::spawn(writer_task(wr, dl_rx));

    // Buffered reads: `next_msg` does two `read_exact`s per message (prefix, then body). On a raw
    // socket each one would be a `recv()` syscall; with this buffer one `recv()` pulls in as many
    // messages as the kernel has queued, and they are sliced from memory — the same amortisation
    // badvpn gets from its PacketProtoDecoder buffer (also one MTU-sized buffer per client).
    let rd = tokio::io::BufReader::with_capacity(UPLINK_BUF, rd);
    let mut fr = FrameReader::new(rd);
    let mut conns: HashMap<u16, UdpConn> = HashMap::new();
    let tick = Arc::new(AtomicU64::new(0));

    loop {
        // Drain the socket-error cleanups WITHOUT cancelling next_msg (non-blocking try_recv): a
        // select! that cancelled next_msg would corrupt the stream (read_exact is not cancel-safe
        // between the prefix and the body). Only remove the entry if the conid still points at the
        // SAME socket generation (otherwise it was reopened — conid reuse).
        while let Ok((conid, gen_id)) = cleanup_rx.try_recv() {
            if conns.get(&conid).map(|c| c.gen_id) == Some(gen_id) {
                conns.remove(&conid);
            }
        }
        // Any message from the client (data OR keepalive) resets the disconnect timer — here that
        // is simply the timeout wrapping next_msg. 20 s of total silence → disconnect (= badvpn).
        let block = match tokio::time::timeout(CLIENT_DISCONNECT, fr.next_msg()).await {
            Ok(Ok(Some(b))) => b,
            Ok(Ok(None)) => {
                info!("client ({client}): client closed");
                break;
            }
            Ok(Err(e)) => {
                info!("client ({client}): client error: {e}");
                break;
            }
            Err(_) => {
                info!("client ({client}): timed out, disconnecting");
                break;
            }
        };
        // Idle timeout — sweep AFTER reading and BEFORE handling: a packet for a connection silent
        // for `idle + interval` or more finds its socket already closed and REOPENS it (new port)
        // instead of reviving it. Sweeping before `next_msg` would not do: the loop blocks there,
        // and the next packet would be handled with no sweep in between. ANY message triggers it,
        // keepalives included — that is what closes connections the client has abandoned.
        if should_sweep(idle_ms, last_sweep.elapsed(), interval) {
            let closed = sweep_idle(&mut conns, ms_since(epoch), idle_ms);
            if closed > 0 {
                debug!("client ({client}): closed {closed} idle connection(s)");
            }
            last_sweep = tokio::time::Instant::now();
        }
        handle_block(
            &mut conns,
            &dl_tx,
            &cleanup_tx,
            &tick,
            epoch,
            &cfg,
            &client,
            &block,
        )
        .await;
    }

    // Client gone: dropping every UdpConn aborts its downlink and returns its permit; dropping dl_tx
    // lets the writer drain what is queued and shut the write side down. Don't wait on a peer that
    // stopped reading, though — cut the writer after a short grace period.
    conns.clear();
    drop(dl_tx);
    if tokio::time::timeout(WRITER_GRACE, &mut writer)
        .await
        .is_err()
    {
        writer.abort();
    }
}

/// Handles ONE de-framed udpgw block. Mirrors badvpn's `client_recv_if_handler_send` +
/// `connection_send_to_udp`.
#[allow(clippy::too_many_arguments)]
async fn handle_block(
    conns: &mut HashMap<u16, UdpConn>,
    dl_tx: &mpsc::Sender<Vec<u8>>,
    cleanup_tx: &mpsc::Sender<(u16, u64)>,
    tick: &Arc<AtomicU64>,
    epoch: tokio::time::Instant,
    cfg: &Arc<Config>,
    client: &Arc<str>,
    block: &[u8],
) {
    let msg = match parse_msg(block) {
        Ok(m) => m,
        Err(e) => {
            // Malformed block: ignored, the client keeps its connection (= badvpn).
            error!("client ({client}): {e}");
            return;
        }
    };
    if msg.is_keepalive() {
        debug!("client ({client}): received keepalive");
        return; // nothing else to do — the disconnect timer was already reset by the read
    }
    let Some(orig_dest) = msg.dest else { return };

    // "too much data": an uplink payload above udp_mtu is rejected before any socket is opened.
    // Unreachable for a conforming client (a real UDP payload is <= 65507 < 65520).
    if msg.payload.len() > cfg.udp_mtu {
        error!("client ({client}): too much data");
        return;
    }

    // Transparent DNS: the datagram GOES to the system DNS server, but we keep the original
    // destination — that is what the downlink header reports (the client matches the reply to the
    // query it sent). Like badvpn, the rewrite only applies when the connection is CREATED.
    // No DNS server available → PASSTHROUGH to the destination the client asked for (= badvpn,
    // which logs a warning and proceeds with the original address — it does NOT drop the packet).
    let send_dest = if msg.is_dns() {
        match current_resolver(cfg.dns_server) {
            Some(r) => r,
            None => {
                warn!("client ({client}): received DNS packet, but no DNS server available");
                orig_dest
            }
        }
    } else {
        orig_dest
    };

    // (Re)open? conid unknown, FLAG_REBIND, or the original destination changed (badvpn:
    // !BAddr_Compare).
    let reopen = match conns.get(&msg.conid) {
        None => true,
        Some(c) => msg.is_rebind() || c.orig_dest != orig_dest,
    };

    if reopen {
        // Close the old connection of this conid first (drop aborts the downlink + returns the permit).
        if conns.remove(&msg.conid).is_some() {
            debug!(
                "client ({client}): connection {} ({orig_dest}): close old",
                msg.conid
            );
        }

        // LRU eviction: at the cap, close the LEAST recently used one to make room (= badvpn).
        if conns.len() >= cfg.max_connections {
            let victim = conns
                .iter()
                .min_by_key(|(_, c)| c.last_use.load(Ordering::Relaxed))
                .map(|(k, _)| *k);
            if let Some(v) = victim {
                debug!("client ({client}): connection {v}: closing least recently used");
                if conns.remove(&v).is_some() {
                    UDP_LRU_EVICTIONS.fetch_add(1, Ordering::Relaxed);
                }
            }
        }

        // Process-wide socket cap (--max-total-connections). No permit = overload → drop the packet.
        let Ok(permit) = cfg.socket_permits.clone().try_acquire_owned() else {
            debug!(
                "client ({client}): connection {}: total connection limit reached, dropping",
                msg.conid
            );
            return;
        };
        match open_conn(
            msg.conid,
            orig_dest,
            send_dest,
            permit,
            dl_tx.clone(),
            cleanup_tx.clone(),
            tick.clone(),
            epoch,
            cfg.udp_mtu,
            client.clone(),
        )
        .await
        {
            Some(c) => {
                conns.insert(msg.conid, c);
            }
            None => return, // socket bind failed
        }
    }

    // Send the payload (if any) to the real destination and mark use (LRU recency + idle clock).
    if let Some(c) = conns.get(&msg.conid) {
        bump(&c.last_use, tick);
        stamp_activity(&c.last_activity_ms, epoch); // uplink keeps the connection alive
        if !msg.payload.is_empty() {
            match c.sock.send_to(msg.payload, c.send_dest).await {
                Ok(sent) => {
                    UDP_UP_PACKETS.fetch_add(1, Ordering::Relaxed);
                    UDP_UP_BYTES.fetch_add(sent as u64, Ordering::Relaxed);
                }
                Err(e) => {
                    UDP_SEND_ERRORS.fetch_add(1, Ordering::Relaxed);
                    warn!("client ({client}): connection {} ({}): UDP send failed: {e}", msg.conid, c.send_dest);
                }
            }
        }
    }
}

/// Opens a full-cone UDP socket for a conid and spawns its downlink task.
#[allow(clippy::too_many_arguments)]
async fn open_conn(
    conid: u16,
    orig_dest: SocketAddr,
    send_dest: SocketAddr,
    permit: OwnedSemaphorePermit,
    dl_tx: mpsc::Sender<Vec<u8>>,
    cleanup_tx: mpsc::Sender<(u16, u64)>,
    tick: Arc<AtomicU64>,
    epoch: tokio::time::Instant,
    udp_mtu: usize,
    client: Arc<str>,
) -> Option<UdpConn> {
    // Bind to the address family of the REAL send destination. NOT connected → full-cone
    // (recv_from from any origin). Like badvpn without --local-udp-addrs, the OS picks an
    // ephemeral port.
    let bind: SocketAddr = if send_dest.is_ipv4() {
        SocketAddr::from(([0u8, 0, 0, 0], 0))
    } else {
        SocketAddr::from(([0u16; 8], 0))
    };
    let sock = match UdpSocket::bind(bind).await {
        Ok(s) => Arc::new(s),
        Err(e) => {
            UDP_SOCKET_ERRORS.fetch_add(1, Ordering::Relaxed);
            warn!("client ({client}): connection {conid} ({orig_dest}): UDP socket failed: {e}");
            return None;
        }
    };
    let gen_id = tick.fetch_add(1, Ordering::Relaxed); // unique id of this socket instance (cleanup)
    let last_use = Arc::new(AtomicU64::new(gen_id));
    // The idle clock starts at OPEN: a connection opened and never used expires too.
    let last_activity_ms = Arc::new(AtomicU64::new(ms_since(epoch)));
    debug!(
        "client ({client}): connection {conid} ({orig_dest}): initialized, local port {}",
        sock.local_addr().map(|a| a.port()).unwrap_or(0)
    );

    ACTIVE_UDP_CONNECTIONS.fetch_add(1, Ordering::Relaxed);
    let down = tokio::spawn(downlink_task(
        sock.clone(),
        conid,
        gen_id,
        orig_dest,
        dl_tx,
        cleanup_tx,
        last_use.clone(),
        tick,
        last_activity_ms.clone(),
        epoch,
        udp_mtu,
        client,
    ));
    Some(UdpConn {
        sock,
        orig_dest,
        send_dest,
        last_use,
        last_activity_ms,
        gen_id,
        down,
        _permit: permit,
    })
}

/// Downlink of one conid: recv_from (any origin) → frame with the ORIGINAL destination → client.
/// No timeout of its own: runs until the socket fails, the client goes away, or it is aborted
/// (idle timeout / eviction / rebind / client gone). The idle timeout is decided in the client loop
/// (`sweep_idle`), from the `last_activity_ms` this task also stamps.
#[allow(clippy::too_many_arguments)]
async fn downlink_task(
    sock: Arc<UdpSocket>,
    conid: u16,
    gen_id: u64,
    reply_addr: SocketAddr, // = orig_dest; badvpn ALWAYS reports the original destination, never the origin
    dl_tx: mpsc::Sender<Vec<u8>>,
    cleanup_tx: mpsc::Sender<(u16, u64)>,
    last_use: Arc<AtomicU64>,
    tick: Arc<AtomicU64>,
    last_activity_ms: Arc<AtomicU64>,
    epoch: tokio::time::Instant,
    udp_mtu: usize,
    client: Arc<str>,
) {
    let mut buf = vec![0u8; udp_mtu];
    loop {
        let n = match sock.recv_from(&mut buf).await {
            Ok((n, _src)) => n, // full-cone: accept from any origin; the real origin is discarded
            Err(e) => {
                UDP_SOCKET_ERRORS.fetch_add(1, Ordering::Relaxed);
                // Socket error (e.g. ICMP port unreachable) → tell the uplink to close this conid
                // (this generation), = badvpn. If the channel is full/closed the try_send fails and
                // the LRU / client teardown cleans up later — never a hang.
                info!("client ({client}): connection {conid} ({reply_addr}): UDP error: {e}");
                let _ = cleanup_tx.try_send((conid, gen_id));
                break;
            }
        };
        stamp_activity(&last_activity_ms, epoch); // downlink keeps the connection alive
        bump(&last_use, &tick); // receiving counts as use (LRU recency, like badvpn)
        if n > MAX_DL_PAYLOAD {
            // Doesn't fit the u16 packetproto prefix — drop it (only possible with --udp-mtu > 65514).
            warn!(
                "client ({client}): connection {conid} ({reply_addr}): packet is too large, cannot send to client"
            );
            continue;
        }
        let frame = build_downlink_frame(conid, reply_addr, &buf[..n]);
        if dl_tx.send(frame).await.is_err() {
            break; // client gone (writer exited)
        }
        UDP_DOWN_PACKETS.fetch_add(1, Ordering::Relaxed);
        UDP_DOWN_BYTES.fetch_add(n as u64, Ordering::Relaxed);
    }
}

/// The single owner of the client's WRITE side: serializes the frames of every downlink, then
/// shuts the write side down.
async fn writer_task<W: AsyncWrite + Unpin>(mut wr: W, mut dl_rx: mpsc::Receiver<Vec<u8>>) {
    while let Some(frame) = dl_rx.recv().await {
        if wr.write_all(&frame).await.is_err() {
            break;
        }
    }
    let _ = wr.shutdown().await;
}

// ── Transparent DNS: the system DNS server (cached for 2 s, = DNS_UPDATE_TIME) ──
static RESOLVER_CACHE: OnceLock<Mutex<Option<(SocketAddr, Instant)>>> = OnceLock::new();

/// Address of the DNS server for transparent DNS. `override_addr` (the --dns-server flag) wins;
/// otherwise the first `nameserver` of /etc/resolv.conf, cached for 2 s. IPv4 only (= badvpn,
/// which uses `_res.nsaddr_list[0]`).
fn current_resolver(override_addr: Option<SocketAddr>) -> Option<SocketAddr> {
    if override_addr.is_some() {
        return override_addr;
    }
    let cell = RESOLVER_CACHE.get_or_init(|| Mutex::new(None));
    let mut g = cell.lock().unwrap_or_else(|e| e.into_inner());
    if let Some((addr, at)) = *g
        && at.elapsed() < DNS_REFRESH
    {
        return Some(addr);
    }
    match parse_resolv_conf() {
        Some(a) => {
            if g.as_ref().map(|(old, _)| *old) != Some(a) {
                info!("using DNS server {a}");
            }
            *g = Some((a, Instant::now()));
            Some(a)
        }
        // Parse failed → keep the last known good server, if any. (badvpn clears it and starts
        // answering "no DNS server available" until the file is readable again.)
        None => g.as_ref().map(|(a, _)| *a),
    }
}

pub fn dns_status() -> String {
    match current_resolver(None) {
        Some(addr) => format!("resolver={addr}"),
        None => "resolver=unavailable; DNS-flagged packets will use their requested destination".to_string(),
    }
}

fn parse_resolv_conf() -> Option<SocketAddr> {
    // Blocking read of a tiny file (normally in page cache), at most once every 2 s thanks to the
    // cache above → negligible on the async hot path.
    let text = std::fs::read_to_string("/etc/resolv.conf").ok()?;
    parse_resolv_conf_text(&text)
}

fn parse_resolv_conf_text(text: &str) -> Option<SocketAddr> {
    for line in text.lines() {
        let line = line.split(['#', ';']).next()?.trim();
        let mut fields = line.split_whitespace();
        if fields.next() != Some("nameserver") {
            continue;
        }
        if let Some(v4) = fields.next().and_then(|value| value.parse::<Ipv4Addr>().ok()) {
            return Some(SocketAddr::from((v4, 53)));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;
    use std::net::{Ipv6Addr, SocketAddr};

    fn test_config(dns_server: Option<SocketAddr>) -> Arc<Config> {
        Arc::new(Config {
            max_connections: 10,
            udp_mtu: DEFAULT_UDP_MTU,
            dns_server,
            socket_permits: Arc::new(Semaphore::new(64)),
            idle_timeout: Duration::ZERO,
        })
    }

    // ── parse_msg ──────────────────────────────────────────────────────────

    #[test]
    fn parse_data_ipv4() {
        // flags=0, conid=0x1234 (LE → 34 12), dst=1.2.3.4:443 (port 443=0x01BB BE → 01 BB), payload=DE AD BE EF
        let block = [
            0x00, 0x34, 0x12, 1, 2, 3, 4, 0x01, 0xBB, 0xDE, 0xAD, 0xBE, 0xEF,
        ];
        let m = parse_msg(&block).unwrap();
        assert_eq!(m.flags, 0);
        assert_eq!(m.conid, 0x1234);
        assert_eq!(m.dest, Some("1.2.3.4:443".parse().unwrap()));
        assert_eq!(m.payload, &[0xDE, 0xAD, 0xBE, 0xEF]);
        assert!(!m.is_keepalive() && !m.is_dns() && !m.is_rebind());
    }

    #[test]
    fn parse_data_ipv6() {
        let ip = Ipv6Addr::new(0x2001, 0x0db8, 0, 0, 0, 0, 0, 1);
        let mut block = vec![FLAG_IPV6, 0x01, 0x00]; // flags=IPV6, conid=1 (LE)
        block.extend_from_slice(&ip.octets()); // 16 bytes network order
        block.extend_from_slice(&443u16.to_be_bytes()); // port BE
        block.extend_from_slice(&[0x11, 0x22]); // payload
        let m = parse_msg(&block).unwrap();
        assert_eq!(m.conid, 1);
        assert_eq!(m.dest, Some(SocketAddr::new(ip.into(), 443)));
        assert_eq!(m.payload, &[0x11, 0x22]);
    }

    #[test]
    fn parse_keepalive_has_no_addr() {
        // flags=KEEPALIVE, conid=0; no addr / payload.
        let block = [FLAG_KEEPALIVE, 0x00, 0x00];
        let m = parse_msg(&block).unwrap();
        assert!(m.is_keepalive());
        assert_eq!(m.dest, None);
        assert!(m.payload.is_empty());
    }

    #[test]
    fn parse_dns_flag_still_parses_dest() {
        // DNS flagged: the parser still extracts the destination the client asked for (the server
        // rewrites it to the DNS server and keeps this one as the reply address).
        let block = [FLAG_DNS, 0x05, 0x00, 8, 8, 8, 8, 0x00, 0x35]; // dst=8.8.8.8:53
        let m = parse_msg(&block).unwrap();
        assert!(m.is_dns());
        assert_eq!(m.dest, Some("8.8.8.8:53".parse().unwrap()));
        assert!(m.payload.is_empty());
    }

    #[test]
    fn parse_rejects_short_header() {
        assert!(parse_msg(&[0x00, 0x01]).is_err()); // < 3 bytes
    }

    #[test]
    fn parse_rejects_truncated_ipv4_addr() {
        // flags=0 (data, IPv4) but only 2 bytes of addr where 6 are needed.
        assert!(parse_msg(&[0x00, 0x01, 0x00, 1, 2]).is_err());
    }

    #[test]
    fn parse_rejects_truncated_ipv6_addr() {
        let mut block = vec![FLAG_IPV6, 0x01, 0x00];
        block.extend_from_slice(&[0u8; 10]); // only 10 of the 18 addr bytes
        assert!(parse_msg(&block).is_err());
    }

    // ── build_downlink_frame ─────────────────────────────────────────────────

    #[test]
    fn build_downlink_v4_exact_bytes() {
        // Pins the endianness contract in a single vector: len LE, conid LE, port BE.
        // conid=0x1234, report=8.8.4.4:53, payload=[0x01].
        let out = build_downlink_frame(0x1234, "8.8.4.4:53".parse().unwrap(), &[0x01]);
        // body_len = 3(hdr)+6(addr)+1(payload) = 10 = 0x000A → LE [0x0A,0x00]
        let expected = [
            0x0A, 0x00, // len (LE), excludes these 2 bytes
            0x00, // flags
            0x34, 0x12, // conid 0x1234 (LE)
            8, 8, 4, 4, // ip (network order)
            0x00, 0x35, // port 53 (BE)
            0x01, // payload
        ];
        assert_eq!(out, expected);
    }

    #[test]
    fn build_downlink_v6_sets_ipv6_flag() {
        let ip = Ipv6Addr::new(0x2001, 0x0db8, 0, 0, 0, 0, 0, 0x1);
        let out = build_downlink_frame(7, SocketAddr::new(ip.into(), 4000), &[]);
        // len = 3+18+0 = 21 = 0x15 → LE [0x15,0x00]; flags at offset 2 = FLAG_IPV6.
        assert_eq!(&out[0..2], &[0x15, 0x00]);
        assert_eq!(out[2], FLAG_IPV6);
        assert_eq!(out.len(), 2 + 21);
    }

    // ── round-trips (build → strip the prefix → parse) ──────────────────────

    #[test]
    fn roundtrip_v4() {
        let dst: SocketAddr = "203.0.113.9:5000".parse().unwrap();
        let payload = b"game-udp";
        let frame = build_downlink_frame(0xBEEF, dst, payload);
        let m = parse_msg(&frame[2..]).unwrap(); // skip the 2-byte prefix
        assert_eq!(m.conid, 0xBEEF);
        assert_eq!(m.dest, Some(dst));
        assert_eq!(m.payload, payload);
    }

    #[test]
    fn roundtrip_v6() {
        let ip = Ipv6Addr::new(0xfe80, 0, 0, 0, 0, 0, 0, 0xabcd);
        let dst = SocketAddr::new(ip.into(), 9999);
        let payload = b"x";
        let frame = build_downlink_frame(0x0001, dst, payload);
        let m = parse_msg(&frame[2..]).unwrap();
        assert!(m.flags & FLAG_IPV6 != 0);
        assert_eq!(m.dest, Some(dst));
        assert_eq!(m.payload, payload);
    }

    // ── FrameReader (framing over the stream) ──────────────────────────────

    #[tokio::test]
    async fn framereader_yields_two_then_eof() {
        // Two frames back to back in the stream; the reader must yield each block (without prefix).
        let f1 = build_downlink_frame(0x0001, "1.1.1.1:53".parse().unwrap(), &[0xAA]);
        let f2 = build_downlink_frame(0x0002, "2.2.2.2:80".parse().unwrap(), &[0xBB, 0xCC]);
        let mut stream = Vec::new();
        stream.extend_from_slice(&f1);
        stream.extend_from_slice(&f2);

        let mut fr = FrameReader::new(Cursor::new(stream));
        let b1 = fr.next_msg().await.unwrap().expect("frame 1");
        assert_eq!(parse_msg(&b1).unwrap().conid, 0x0001);
        let b2 = fr.next_msg().await.unwrap().expect("frame 2");
        assert_eq!(parse_msg(&b2).unwrap().conid, 0x0002);
        assert!(fr.next_msg().await.unwrap().is_none(), "clean EOF → None");
    }

    #[tokio::test]
    async fn framereader_empty_is_clean_eof() {
        let mut fr = FrameReader::new(Cursor::new(Vec::<u8>::new()));
        assert!(fr.next_msg().await.unwrap().is_none());
    }

    #[tokio::test]
    async fn framereader_partial_prefix_is_error() {
        let mut fr = FrameReader::new(Cursor::new(vec![0x01]));
        assert_eq!(
            fr.next_msg().await.unwrap_err().kind(),
            ErrorKind::UnexpectedEof
        );
    }

    #[tokio::test]
    async fn framereader_partial_body_is_error() {
        let mut fr = FrameReader::new(Cursor::new(vec![0x02, 0x00, 0xAA]));
        assert_eq!(
            fr.next_msg().await.unwrap_err().kind(),
            ErrorKind::UnexpectedEof
        );
    }

    #[test]
    fn resolver_parser_uses_first_ipv4_nameserver_and_skips_ipv6() {
        let text = "# cloud resolver\nnameserver 2001:4860:4860::8888\nnameserver 169.254.169.254 # platform DNS\n";
        assert_eq!(
            parse_resolv_conf_text(text),
            Some("169.254.169.254:53".parse().unwrap())
        );
    }

    #[test]
    fn resolver_parser_returns_none_without_ipv4_nameserver() {
        assert_eq!(
            parse_resolv_conf_text("nameserver 2001:4860:4860::8888\n"),
            None
        );
    }

    #[tokio::test]
    async fn framereader_truncated_body_is_error() {
        // Prefix says 10 bytes of body, only 4 follow → error (truncated message), not None.
        let mut stream = 10u16.to_le_bytes().to_vec();
        stream.extend_from_slice(&[0x00, 0x01, 0x00, 0x02]);
        let mut fr = FrameReader::new(Cursor::new(stream));
        assert!(fr.next_msg().await.is_err());
    }

    #[tokio::test]
    async fn framereader_undersized_block_is_yielded_and_rejected_by_parser() {
        // Prefix = 2 (shorter than the 3-byte header): the reader yields the block as is (the client
        // must NOT be dropped — badvpn logs "missing header" and moves on) and the parser rejects it.
        // The following frame is then read correctly, proving the stream stayed in sync.
        let mut stream = 2u16.to_le_bytes().to_vec();
        stream.extend_from_slice(&[0xAA, 0xBB]);
        stream.extend_from_slice(&build_downlink_frame(
            0x0009,
            "1.1.1.1:53".parse().unwrap(),
            &[0x01],
        ));
        let mut fr = FrameReader::new(Cursor::new(stream));
        let short = fr
            .next_msg()
            .await
            .unwrap()
            .expect("short block is still yielded");
        assert_eq!(short, vec![0xAA, 0xBB]);
        assert!(
            parse_msg(&short).is_err(),
            "missing header → parser rejects"
        );
        let next = fr.next_msg().await.unwrap().expect("next frame");
        assert_eq!(parse_msg(&next).unwrap().conid, 0x0009);
        assert!(fr.next_msg().await.unwrap().is_none());
    }

    // ── e2e: drives the whole serve() against a real UDP echo through a simulated client stream ──
    #[tokio::test]
    async fn e2e_echo_roundtrip_v4() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::UdpSocket;

        // Local UDP echo server: sends back whatever it receives.
        let echo = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let echo_addr = echo.local_addr().unwrap();
        tokio::spawn(async move {
            let mut b = [0u8; 2048];
            while let Ok((n, src)) = echo.recv_from(&mut b).await {
                let _ = echo.send_to(&b[..n], src).await;
            }
        });

        // Simulated client connection (duplex): `srv` goes to serve(); `cli` is the client side.
        let (cli, srv) = tokio::io::duplex(64 * 1024);
        tokio::spawn(serve(srv, "test".to_string(), test_config(None)));
        let (mut cli_rd, mut cli_wr) = tokio::io::split(cli);

        // Uplink: a udpgw data packet for echo_addr (conid=7, payload "ping"), built by hand the way
        // a client does — [len LE][flags=0][conid LE][ip BE][port BE][payload].
        let SocketAddr::V4(v4) = echo_addr else {
            panic!("expected IPv4")
        };
        let mut body = vec![0u8, 0x07, 0x00]; // flags=0, conid=7 (LE)
        body.extend_from_slice(&v4.ip().octets());
        body.extend_from_slice(&v4.port().to_be_bytes());
        body.extend_from_slice(b"ping");
        let mut frame = (body.len() as u16).to_le_bytes().to_vec();
        frame.extend_from_slice(&body);
        cli_wr.write_all(&frame).await.unwrap();

        // Downlink: wait for the reply frame (with a safety timeout).
        let mut lenb = [0u8; 2];
        tokio::time::timeout(Duration::from_secs(5), cli_rd.read_exact(&mut lenb))
            .await
            .expect("timeout waiting for the downlink")
            .unwrap();
        let blen = u16::from_le_bytes(lenb) as usize;
        let mut block = vec![0u8; blen];
        cli_rd.read_exact(&mut block).await.unwrap();

        let m = parse_msg(&block).unwrap();
        assert_eq!(m.conid, 7);
        assert_eq!(m.dest, Some(echo_addr)); // reports the ORIGINAL destination (= badvpn)
        assert_eq!(m.payload, b"ping"); // echoed back through the full-cone socket
    }

    // ── e2e transparent DNS: FLAG_DNS goes to the DNS SERVER, but the downlink reports the ORIGINAL destination ──
    #[tokio::test]
    async fn e2e_dns_rewrite_reports_orig_dest() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::UdpSocket;

        // The "DNS server" is a local echo. If the packet arrives HERE, FLAG_DNS was rewritten to the
        // DNS server — and NOT sent to the original destination (203.0.113.53:53, TEST-NET, unreachable).
        let resolver = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let resolver_addr = resolver.local_addr().unwrap();
        tokio::spawn(async move {
            let mut b = [0u8; 2048];
            while let Ok((n, src)) = resolver.recv_from(&mut b).await {
                let _ = resolver.send_to(&b[..n], src).await;
            }
        });

        let (cli, srv) = tokio::io::duplex(64 * 1024);
        // dns_server = the echo: every FLAG_DNS packet is sent there.
        tokio::spawn(serve(
            srv,
            "test".to_string(),
            test_config(Some(resolver_addr)),
        ));
        let (mut cli_rd, mut cli_wr) = tokio::io::split(cli);

        // Uplink: FLAG_DNS packet, ORIGINAL destination = 203.0.113.53:53 (what the client "asked for").
        let orig: SocketAddr = "203.0.113.53:53".parse().unwrap();
        let SocketAddr::V4(v4) = orig else {
            panic!("expected IPv4")
        };
        let mut body = vec![FLAG_DNS, 0x00, 0x00]; // flags=DNS, conid=0
        body.extend_from_slice(&v4.ip().octets());
        body.extend_from_slice(&v4.port().to_be_bytes());
        body.extend_from_slice(b"query");
        let mut frame = (body.len() as u16).to_le_bytes().to_vec();
        frame.extend_from_slice(&body);
        cli_wr.write_all(&frame).await.unwrap();

        // Downlink: the reply (echoed by the "DNS server") MUST report the ORIGINAL destination.
        // Had the rewrite failed, the packet would go to 203.0.113.53 (no echo) and this would time out.
        let mut lenb = [0u8; 2];
        tokio::time::timeout(Duration::from_secs(5), cli_rd.read_exact(&mut lenb))
            .await
            .expect("timeout — the DNS packet was not rewritten to the DNS server")
            .unwrap();
        let blen = u16::from_le_bytes(lenb) as usize;
        let mut block = vec![0u8; blen];
        cli_rd.read_exact(&mut block).await.unwrap();

        let m = parse_msg(&block).unwrap();
        assert_eq!(m.conid, 0);
        assert_eq!(
            m.dest,
            Some(orig),
            "DNS downlink must report the ORIGINAL destination"
        );
        assert_ne!(
            m.dest,
            Some(resolver_addr),
            "must NOT report the DNS server's address"
        );
        assert_eq!(m.payload, b"query");
    }

    // ── optional per-connection idle timeout ─────────────────────────────────

    // Pure decision at the BOUNDARY (an off-by-one `>=` → `>` only shows up at exactly the timeout).
    #[test]
    fn is_idle_boundary_and_disabled() {
        let idle = 120_000;
        assert!(!is_idle(0, 119_999, idle), "1 ms before the timeout: alive");
        assert!(is_idle(0, 120_000, idle), "exactly at the timeout: idle");
        assert!(
            !is_idle(5_000, 124_999, idle),
            "counted from the LAST datagram, not from open"
        );
        assert!(is_idle(5_000, 125_000, idle));
        // benign race: the downlink stamped AFTER `now` was read → no underflow, alive
        assert!(!is_idle(130_000, 120_000, idle));
        // disabled: never idle, however long
        assert!(!is_idle(0, u64::MAX, 0));
    }

    // Sweep cadence and trigger. The 1 s term is the only one that decides with real timeouts, and
    // no e2e test exercises it (they use a short timeout) — so the production values go here.
    #[test]
    fn sweep_interval_and_trigger() {
        assert_eq!(
            sweep_interval(Duration::from_secs(120)),
            Duration::from_secs(1)
        );
        assert_eq!(
            sweep_interval(Duration::from_secs(4)),
            Duration::from_secs(1)
        );
        assert_eq!(
            sweep_interval(Duration::from_secs(2)),
            Duration::from_millis(500)
        );
        assert_eq!(
            sweep_interval(Duration::from_millis(600)),
            Duration::from_millis(150)
        );
        let one_s = Duration::from_secs(1);
        assert!(!should_sweep(120_000, Duration::from_millis(999), one_s));
        assert!(should_sweep(120_000, one_s, one_s));
        // disabled: never sweeps, even with a zero interval and hours since the last one
        assert!(!should_sweep(0, Duration::from_secs(3600), Duration::ZERO));
    }

    // Sweep over REAL connections (via open_conn): closes only the idle one and RELEASES ITS SOCKET
    // — proven with a `Weak`: without the `abort()` in Drop the downlink task would keep the `Arc`
    // and the port would never come back (the permit would, being a field of UdpConn — on its own it
    // proves nothing).
    #[tokio::test]
    async fn sweep_idle_closes_only_the_idle_one_and_releases_its_socket() {
        let sem = Arc::new(Semaphore::new(10));
        let (dl_tx, _dl_rx) = mpsc::channel::<Vec<u8>>(8);
        let (cl_tx, _cl_rx) = mpsc::channel::<(u16, u64)>(8);
        let tick = Arc::new(AtomicU64::new(0));
        let epoch = tokio::time::Instant::now();
        let dest: SocketAddr = "127.0.0.1:9".parse().unwrap();
        let mut conns: HashMap<u16, UdpConn> = HashMap::new();
        for conid in [1u16, 2] {
            let permit = sem.clone().try_acquire_owned().unwrap();
            let c = open_conn(
                conid,
                dest,
                dest,
                permit,
                dl_tx.clone(),
                cl_tx.clone(),
                tick.clone(),
                epoch,
                DEFAULT_UDP_MTU,
                "test".into(),
            )
            .await
            .expect("bind of the test socket");
            conns.insert(conid, c);
        }
        assert_eq!(sem.available_permits(), 8);
        let sock1 = Arc::downgrade(&conns[&1].sock);
        let sock2 = Arc::downgrade(&conns[&2].sock);
        conns[&1].last_activity_ms.store(0, Ordering::Relaxed); // silent since open
        conns[&2].last_activity_ms.store(10_000, Ordering::Relaxed); // had a datagram at 10 s
        // now = 10,499: conn 2 is 499 ms after its last datagram (1 ms before a 500 ms timeout →
        // alive); conn 1 is 10,499 ms after (→ idle). At 10,500 BOTH would close.
        assert_eq!(sweep_idle(&mut conns, 10_499, 500), 1);
        assert!(
            !conns.contains_key(&1) && conns.contains_key(&2),
            "only the idle one goes"
        );
        assert_eq!(sem.available_permits(), 9, "its permit came back");
        // abort is cooperative: yield so the aborted task finishes and drops its Arc
        for _ in 0..20 {
            tokio::task::yield_now().await;
        }
        assert!(
            sock1.upgrade().is_none(),
            "the closed connection's socket was released"
        );
        assert!(
            sock2.upgrade().is_some(),
            "the live connection's socket is still open"
        );
        assert_eq!(
            sweep_idle(&mut conns, u64::MAX, 0),
            0,
            "timeout 0 = disabled, closes nothing"
        );
        assert!(conns.contains_key(&2));
    }

    // ── e2e of the idle timeout: drives the whole serve() with a short timeout (600 ms, REAL time) ──
    //
    // The test sees whether the connection's socket was REUSED or REOPENED through the source port
    // the destination observes: same socket = same port; reopened = new port (the old socket is still
    // open when the new one binds — abort is asynchronous — so the new port is necessarily different).
    // 600 ms leaves ~500 ms of margin in the "within the timeout" stretches (pauses of ≤100 ms): a
    // loaded machine could only cause a FALSE FAILURE, never a false pass.

    const IDLE_TEST: Duration = Duration::from_millis(600);

    // UDP destination that records the source port of every datagram it receives. `push`:
    // Some(rate) = after the first datagram, keeps sending datagrams back to the source at that rate
    // (downlink-ONLY traffic); None = never answers.
    async fn recording_destination(push: Option<Duration>) -> (SocketAddr, Arc<Mutex<Vec<u16>>>) {
        let s = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
        let addr = s.local_addr().unwrap();
        let ports = Arc::new(Mutex::new(Vec::new()));
        let p = ports.clone();
        tokio::spawn(async move {
            let mut b = [0u8; 2048];
            let mut pushing = false;
            while let Ok((_, src)) = s.recv_from(&mut b).await {
                p.lock().unwrap().push(src.port());
                if let (Some(rate), false) = (push, pushing) {
                    pushing = true;
                    let s2 = s.clone();
                    tokio::spawn(async move {
                        loop {
                            let _ = s2.send_to(b"push", src).await;
                            tokio::time::sleep(rate).await;
                        }
                    });
                }
            }
        });
        (addr, ports)
    }

    fn data_frame(conid: u16, dest: SocketAddr, payload: &[u8]) -> Vec<u8> {
        let SocketAddr::V4(v4) = dest else {
            panic!("the test uses an IPv4 destination")
        };
        let mut body = vec![0u8]; // flags = 0 (data)
        body.extend_from_slice(&conid.to_le_bytes());
        body.extend_from_slice(&v4.ip().octets());
        body.extend_from_slice(&v4.port().to_be_bytes());
        body.extend_from_slice(payload);
        let mut f = (body.len() as u16).to_le_bytes().to_vec();
        f.extend_from_slice(&body);
        f
    }

    // Client keepalive: header only (3 B), no address — it belongs to the CLIENT, refreshes no
    // connection.
    fn keepalive() -> Vec<u8> {
        vec![3, 0, FLAG_KEEPALIVE, 0, 0]
    }

    // Starts serve() with the given timeout and returns the client's write side + the socket
    // semaphore (64 permits, 1 per open socket — how the test sees an untouched connection close).
    // The read side is drained in the background so the downlink never clogs the duplex.
    fn start_client(
        idle_timeout: Duration,
    ) -> (
        tokio::io::WriteHalf<tokio::io::DuplexStream>,
        Arc<Semaphore>,
    ) {
        let (cli, srv) = tokio::io::duplex(64 * 1024);
        let sem = Arc::new(Semaphore::new(64));
        let cfg = Arc::new(Config {
            max_connections: 10,
            udp_mtu: DEFAULT_UDP_MTU,
            dns_server: None,
            socket_permits: sem.clone(),
            idle_timeout,
        });
        tokio::spawn(serve(srv, "test".to_string(), cfg));
        let (mut rd, wr) = tokio::io::split(cli);
        tokio::spawn(async move {
            let mut b = [0u8; 4096];
            while let Ok(n) = rd.read(&mut b).await {
                if n == 0 {
                    break;
                }
            }
        });
        (wr, sem)
    }

    // Keeps the client alive for `dur`, sending the given frame every 100 ms (a keepalive, or data
    // on another connection).
    async fn keep_client_alive(
        wr: &mut tokio::io::WriteHalf<tokio::io::DuplexStream>,
        frame: &[u8],
        dur: Duration,
    ) {
        let end = tokio::time::Instant::now() + dur;
        while tokio::time::Instant::now() < end {
            wr.write_all(frame).await.unwrap();
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    async fn wait_ports(ports: &Arc<Mutex<Vec<u16>>>, n: usize) -> Vec<u16> {
        let t0 = tokio::time::Instant::now();
        loop {
            {
                let v = ports.lock().unwrap();
                if v.len() >= n {
                    return v.clone();
                }
            }
            assert!(
                t0.elapsed() < Duration::from_secs(5),
                "the destination did not receive {n} datagrams"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    async fn wait_permits(sem: &Semaphore, n: usize) {
        let t0 = tokio::time::Instant::now();
        while sem.available_permits() != n {
            assert!(
                t0.elapsed() < Duration::from_secs(3),
                "permits = {}, expected {n} (a connection did not close/open as it should)",
                sem.available_permits()
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    // Silent past the timeout → the next packet REOPENS (new port); within the timeout, same socket.
    // Catches: sweep not called in the loop, or called BEFORE next_msg (the next packet would then
    // revive the idle socket).
    #[tokio::test]
    async fn e2e_idle_connection_reopens_with_new_port() {
        let (dest, ports) = recording_destination(None).await;
        let (mut wr, _sem) = start_client(IDLE_TEST);
        wr.write_all(&data_frame(7, dest, b"a")).await.unwrap();
        wait_ports(&ports, 1).await;
        tokio::time::sleep(Duration::from_millis(50)).await;
        wr.write_all(&data_frame(7, dest, b"b")).await.unwrap(); // within the timeout
        wait_ports(&ports, 2).await;
        tokio::time::sleep(IDLE_TEST * 2).await; // silent past the timeout
        wr.write_all(&data_frame(7, dest, b"c")).await.unwrap();
        let v = wait_ports(&ports, 3).await;
        assert_eq!(v[0], v[1], "within the timeout: same socket");
        assert_ne!(
            v[1], v[2],
            "past the timeout: socket closed, reopened with a new port"
        );
    }

    // Uplink-ONLY traffic (push, no answer) keeps the connection alive: 15 packets every 100 ms
    // (1.5 s, 2.5× the timeout) all leave through the SAME port. Catches: uplink not stamping.
    #[tokio::test]
    async fn e2e_uplink_keeps_connection_alive() {
        let (dest, ports) = recording_destination(None).await;
        let (mut wr, _sem) = start_client(IDLE_TEST);
        for i in 0..15u8 {
            wr.write_all(&data_frame(7, dest, &[i])).await.unwrap();
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        let v = wait_ports(&ports, 15).await;
        assert!(
            v.iter().all(|&p| p == v[0]),
            "continuous uplink must not become idle: {v:?}"
        );
    }

    // Downlink-ONLY traffic keeps the connection alive: the destination pushes every 100 ms; the
    // client sends 1 packet, stays silent on that connection for 3× the timeout, sends another →
    // same port. Catches: downlink not stamping. The client is WARMED UP for 1.5× the timeout before
    // the connection opens, so the open is far from the client's `epoch`: a downlink stamping with
    // another clock (e.g. a local `now()` instead of `epoch`) would then lag enough to close it.
    #[tokio::test]
    async fn e2e_downlink_keeps_connection_alive() {
        let (dest, ports) = recording_destination(Some(Duration::from_millis(100))).await;
        let (mut wr, _sem) = start_client(IDLE_TEST);
        keep_client_alive(&mut wr, &keepalive(), IDLE_TEST * 3 / 2).await;
        wr.write_all(&data_frame(7, dest, b"a")).await.unwrap();
        wait_ports(&ports, 1).await;
        tokio::time::sleep(IDLE_TEST * 3).await;
        wr.write_all(&data_frame(7, dest, b"b")).await.unwrap();
        let v = wait_ports(&ports, 2).await;
        assert_eq!(v[0], v[1], "continuous downlink must not become idle");
    }

    // The real-world case: a connection the client ABANDONS while the client itself stays alive on
    // keepalives only. Its socket must close on its own — seen through the socket permit coming back.
    // Catches: "lazy" expiry (only of the connection being used) and a sweep triggered only by DATA.
    #[tokio::test]
    async fn e2e_abandoned_connection_closes_on_keepalives_only() {
        let (dest, ports) = recording_destination(None).await;
        let (mut wr, sem) = start_client(IDLE_TEST);
        wr.write_all(&data_frame(7, dest, b"a")).await.unwrap();
        wait_ports(&ports, 1).await;
        assert_eq!(sem.available_permits(), 63, "connection 7 open = 1 socket");
        keep_client_alive(&mut wr, &keepalive(), IDLE_TEST * 5 / 2).await;
        wait_permits(&sem, 64).await; // connection 7 closed without ever being touched again
    }

    // Same, with the client kept alive by TRAFFIC on ANOTHER connection: 7 (abandoned) closes, 8
    // (active) stays open.
    #[tokio::test]
    async fn e2e_abandoned_connection_closes_while_another_is_active() {
        let (dest, ports) = recording_destination(None).await;
        let (mut wr, sem) = start_client(IDLE_TEST);
        wr.write_all(&data_frame(7, dest, b"a")).await.unwrap();
        wait_ports(&ports, 1).await;
        keep_client_alive(&mut wr, &data_frame(8, dest, b"x"), IDLE_TEST * 5 / 2).await;
        // 64 − 1 (connection 8, alive): 7 closed. With no more messages there is no sweep, so 8
        // does not close while we wait.
        wait_permits(&sem, 63).await;
    }

    // Timeout disabled (`--udp-idle-timeout 0`, = badvpn): silent for 2× the test timeout, the connection keeps
    // its socket. (That ZERO never sweeps, however long, is proven in `sweep_interval_and_trigger`;
    // this is the end-to-end wiring.)
    #[tokio::test]
    async fn e2e_timeout_zero_never_closes() {
        let (dest, ports) = recording_destination(None).await;
        let (mut wr, _sem) = start_client(Duration::ZERO);
        wr.write_all(&data_frame(7, dest, b"a")).await.unwrap();
        wait_ports(&ports, 1).await;
        tokio::time::sleep(IDLE_TEST * 2).await;
        wr.write_all(&data_frame(7, dest, b"b")).await.unwrap();
        let v = wait_ports(&ports, 2).await;
        assert_eq!(v[0], v[1], "timeout 0 never closes");
    }
}
