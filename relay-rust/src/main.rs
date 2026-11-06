//! badvpn-udpgw — UDP gateway for tun2socks-style clients: a drop-in replacement for the C
//! `badvpn-udpgw` (github.com/ambrop72/badvpn), written in Rust on top of tokio.
//!
//! This file is only the daemon shell: command line, listeners, the client counter and signals.
//! The protocol and the per-client server live in `udpgw.rs`.

use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use clap::{ArgAction, Parser};
use log::{LevelFilter, error, info, warn};
use socket2::SockRef;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Semaphore;

mod udpgw;

/// `MAX_LISTEN_ADDRS` from udpgw.h.
const MAX_LISTEN_ADDRS: usize = 16;

#[derive(Parser, Debug)]
#[command(
    name = "badvpn-udpgw",
    version,
    about = "UDP gateway for tun2socks (badvpn-udpgw protocol), Rust implementation",
    after_help = "Log levels follow badvpn: 0-5 or none/error/warning/notice/info/debug (notice and \
                  info map to the same level). Flags marked [extension] do not exist in the C original."
)]
struct Cli {
    /// Address to listen on, e.g. 127.0.0.1:7300 or [::1]:7300. Repeat for more (up to 16).
    #[arg(long = "listen-addr", value_name = "ADDR", required = true, action = ArgAction::Append)]
    listen_addrs: Vec<SocketAddr>,

    /// Maximum number of simultaneous clients (TCP connections); extra clients are refused.
    #[arg(long, value_name = "NUMBER", default_value_t = 3, value_parser = clap::value_parser!(u64).range(1..))]
    max_clients: u64,

    /// Maximum UDP connections per client; above it the least recently used one is closed.
    #[arg(long, value_name = "NUMBER", default_value_t = 256, value_parser = clap::value_parser!(u64).range(1..))]
    max_connections_for_client: u64,

    /// Largest UDP payload accepted from a client, in bytes.
    #[arg(long, value_name = "BYTES", default_value_t = udpgw::DEFAULT_UDP_MTU as u16, value_parser = clap::value_parser!(u16).range(1..))]
    udp_mtu: u16,

    /// SO_SNDBUF of each client's TCP socket, in bytes (0 = leave the OS default).
    #[arg(long, value_name = "BYTES", default_value_t = 1_048_576)]
    client_socket_sndbuf: usize,

    /// Log verbosity: 0-5 or none/error/warning/notice/info/debug.
    #[arg(long, value_name = "LEVEL", default_value = "info", value_parser = parse_loglevel)]
    loglevel: LevelFilter,

    /// DNS server for packets the client flags as DNS, e.g. 1.1.1.1 or 1.1.1.1:53
    /// (default: the first nameserver in /etc/resolv.conf, re-read every 2 s). [extension]
    #[arg(long, value_name = "ADDR", value_parser = parse_dns_server)]
    dns_server: Option<SocketAddr>,

    /// Process-wide cap on open UDP sockets across all clients (0 = unlimited). [extension]
    #[arg(long, value_name = "NUMBER", default_value_t = 0)]
    max_total_connections: usize,

    /// Close a UDP connection after this many seconds without a datagram in either direction,
    /// freeing its ephemeral port. 0 = never, like the C original. On by default to prevent
    /// ephemeral port exhaustion on busy gateways — see the README. [extension]
    #[arg(long, value_name = "SECONDS", default_value_t = udpgw::DEFAULT_IDLE_TIMEOUT.as_secs())]
    udp_idle_timeout: u64,
}

/// `--udp-idle-timeout` → the effective per-connection timeout handed to the server
/// (0 = `Duration::ZERO` = disabled). Its own function so the conversion is tested.
fn idle_timeout_from_flag(secs: u64) -> Duration {
    Duration::from_secs(secs)
}

/// State shared by every accept loop.
struct Shared {
    cfg: Arc<udpgw::Config>,
    clients: AtomicUsize,
    max_clients: usize,
    sndbuf: usize,
}

/// badvpn's `--loglevel` vocabulary (`<0-5/none/error/warning/notice/info/debug>`) mapped onto the
/// `log` crate. NOTICE and INFO collapse into `Info`; badvpn's channel default is INFO (4).
fn parse_loglevel(s: &str) -> Result<LevelFilter, String> {
    Ok(match s.to_ascii_lowercase().as_str() {
        "0" | "none" => LevelFilter::Off,
        "1" | "error" => LevelFilter::Error,
        "2" | "warning" | "warn" => LevelFilter::Warn,
        "3" | "notice" | "4" | "info" => LevelFilter::Info,
        "5" | "debug" => LevelFilter::Debug,
        _ => {
            return Err(format!(
                "expected 0-5 or none/error/warning/notice/info/debug, got '{s}'"
            ));
        }
    })
}

/// `--dns-server`: `ip:port`, or a bare IP (port 53).
fn parse_dns_server(s: &str) -> Result<SocketAddr, String> {
    if let Ok(sa) = s.parse::<SocketAddr>() {
        return Ok(sa);
    }
    match s.parse::<IpAddr>() {
        Ok(ip) => Ok(SocketAddr::new(ip, 53)),
        Err(_) => Err(format!("expected an IP address or ip:port, got '{s}'")),
    }
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    if cli.listen_addrs.len() > MAX_LISTEN_ADDRS {
        eprintln!("--listen-addr: too many listen addresses (maximum {MAX_LISTEN_ADDRS})");
        std::process::exit(1);
    }

    env_logger::Builder::new()
        .filter_level(cli.loglevel)
        .format(|buf, record| {
            use std::io::Write;
            writeln!(
                buf,
                "{} {:<7} {}",
                buf.timestamp_seconds(),
                record.level(),
                record.args()
            )
        })
        .init();

    info!(
        "initializing {} {}",
        env!("CARGO_PKG_NAME"),
        env!("CARGO_PKG_VERSION")
    );

    let permits = if cli.max_total_connections == 0 {
        Semaphore::MAX_PERMITS
    } else {
        cli.max_total_connections
    };
    let cfg = Arc::new(udpgw::Config {
        max_connections: cli.max_connections_for_client as usize,
        udp_mtu: cli.udp_mtu as usize,
        dns_server: cli.dns_server,
        socket_permits: Arc::new(Semaphore::new(permits)),
        idle_timeout: idle_timeout_from_flag(cli.udp_idle_timeout),
    });
    let shared = Arc::new(Shared {
        cfg,
        clients: AtomicUsize::new(0),
        max_clients: cli.max_clients as usize,
        sndbuf: cli.client_socket_sndbuf,
    });

    // Bind everything first: a bad address must fail the start, not run half-configured.
    let mut listeners = Vec::with_capacity(cli.listen_addrs.len());
    for addr in &cli.listen_addrs {
        match TcpListener::bind(addr).await {
            Ok(l) => {
                info!("listening on {addr}");
                listeners.push(l);
            }
            Err(e) => {
                error!("listen addr {addr}: bind failed: {e}");
                std::process::exit(1);
            }
        }
    }
    for l in listeners {
        tokio::spawn(accept_loop(l, shared.clone()));
    }

    info!("DNS status: {}", udpgw::dns_status());

    let stats_shared = shared.clone();
    let stats_task = tokio::spawn(async move {
        let mut ticker = tokio::time::interval(Duration::from_secs(10));
        ticker.tick().await;
        loop {
            ticker.tick().await;
            info!(
                "UDPGW_STATS {}",
                udpgw::stats_line(stats_shared.clients.load(Ordering::Relaxed))
            );
        }
    });

    let idle = shared.cfg.idle_timeout;
    info!(
        "entering event loop (max clients {}, max connections per client {}, udp mtu {}, udp idle timeout {})",
        shared.max_clients,
        shared.cfg.max_connections,
        shared.cfg.udp_mtu,
        if idle.is_zero() {
            "off".to_string()
        } else {
            format!("{}s", idle.as_secs())
        }
    );
    wait_for_shutdown().await;
    stats_task.abort();
    let _ = stats_task.await;
    info!("termination requested");
    info!("exiting");
}

/// Accepts clients on one listener. Enforces `--max-clients` (refuse, like badvpn — never queue)
/// and applies `--client-socket-sndbuf`.
async fn accept_loop(listener: TcpListener, shared: Arc<Shared>) {
    loop {
        let (stream, peer) = match listener.accept().await {
            Ok(x) => x,
            Err(e) => {
                // Typically EMFILE/ENFILE: back off briefly instead of spinning.
                error!("accept failed: {e}");
                tokio::time::sleep(Duration::from_millis(100)).await;
                continue;
            }
        };
        if shared.clients.fetch_add(1, Ordering::SeqCst) >= shared.max_clients {
            shared.clients.fetch_sub(1, Ordering::SeqCst);
            error!("maximum number of clients reached");
            continue; // dropping `stream` closes it
        }
        configure_client_socket(&stream, peer, shared.sndbuf);
        info!("client ({peer}): connected");
        let shared2 = shared.clone();
        tokio::spawn(async move {
            udpgw::serve(stream, peer.to_string(), shared2.cfg.clone()).await;
            shared2.clients.fetch_sub(1, Ordering::SeqCst);
        });
    }
}

/// = badvpn's `BConnection_SetSendBuffer` on the client socket (SO_SNDBUF); 0 leaves the OS default.
fn configure_client_socket(stream: &TcpStream, peer: SocketAddr, sndbuf: usize) {
    if sndbuf == 0 {
        return;
    }
    if let Err(e) = SockRef::from(stream).set_send_buffer_size(sndbuf) {
        warn!("client ({peer}): set send buffer failed: {e}");
    }
}

/// SIGINT / SIGTERM (Ctrl-C only on non-Unix).
async fn wait_for_shutdown() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let mut term = match signal(SignalKind::terminate()) {
            Ok(s) => s,
            Err(e) => {
                warn!("SIGTERM handler unavailable ({e}); only Ctrl-C will stop the process");
                let _ = tokio::signal::ctrl_c().await;
                return;
            }
        };
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = term.recv() => {}
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

#[cfg(test)]
mod cli_tests {
    use super::*;

    #[test]
    fn loglevel_accepts_badvpn_numbers_and_names() {
        assert_eq!(parse_loglevel("0").unwrap(), LevelFilter::Off);
        assert_eq!(parse_loglevel("none").unwrap(), LevelFilter::Off);
        assert_eq!(parse_loglevel("1").unwrap(), LevelFilter::Error);
        assert_eq!(parse_loglevel("warning").unwrap(), LevelFilter::Warn);
        assert_eq!(parse_loglevel("notice").unwrap(), LevelFilter::Info);
        assert_eq!(parse_loglevel("4").unwrap(), LevelFilter::Info);
        assert_eq!(parse_loglevel("DEBUG").unwrap(), LevelFilter::Debug);
        assert!(parse_loglevel("6").is_err());
        assert!(parse_loglevel("verbose").is_err());
    }

    #[test]
    fn dns_server_accepts_bare_ip_and_ip_port() {
        assert_eq!(
            parse_dns_server("1.1.1.1").unwrap(),
            "1.1.1.1:53".parse::<SocketAddr>().unwrap()
        );
        assert_eq!(
            parse_dns_server("9.9.9.9:5353").unwrap(),
            "9.9.9.9:5353".parse::<SocketAddr>().unwrap()
        );
        assert_eq!(
            parse_dns_server("[::1]:53").unwrap(),
            "[::1]:53".parse::<SocketAddr>().unwrap()
        );
        assert!(parse_dns_server("dns.example").is_err());
    }

    #[test]
    fn cli_defaults_match_badvpn() {
        let cli = Cli::try_parse_from(["badvpn-udpgw", "--listen-addr", "127.0.0.1:7300"]).unwrap();
        assert_eq!(cli.max_clients, 3); // DEFAULT_MAX_CLIENTS
        assert_eq!(cli.max_connections_for_client, 256); // DEFAULT_MAX_CONNECTIONS_FOR_CLIENT
        assert_eq!(cli.udp_mtu as usize, 65_520); // DEFAULT_UDP_MTU
        assert_eq!(cli.client_socket_sndbuf, 1_048_576); // CLIENT_DEFAULT_SOCKET_SEND_BUFFER
        assert_eq!(cli.loglevel, LevelFilter::Info); // udpgw channel default = 4
        assert_eq!(cli.max_total_connections, 0);
        assert!(cli.dns_server.is_none());
        // The ONE deliberate default difference from the C original (which has no idle timeout):
        // 120 s, to prevent ephemeral port exhaustion — see `udpgw::DEFAULT_IDLE_TIMEOUT`.
        assert_eq!(cli.udp_idle_timeout, 120, "idle timeout ON by default");
        assert_eq!(
            idle_timeout_from_flag(cli.udp_idle_timeout),
            udpgw::DEFAULT_IDLE_TIMEOUT
        );
    }

    #[test]
    fn udp_idle_timeout_flag_parses_and_converts() {
        // The opt-out: `0` restores the C original's behaviour (no idle timeout).
        let off = Cli::try_parse_from([
            "badvpn-udpgw",
            "--listen-addr",
            "127.0.0.1:7300",
            "--udp-idle-timeout",
            "0",
        ])
        .unwrap();
        assert_eq!(idle_timeout_from_flag(off.udp_idle_timeout), Duration::ZERO);
        let cli = Cli::try_parse_from([
            "badvpn-udpgw",
            "--listen-addr",
            "127.0.0.1:7300",
            "--udp-idle-timeout",
            "120",
        ])
        .unwrap();
        assert_eq!(
            idle_timeout_from_flag(cli.udp_idle_timeout),
            Duration::from_secs(120)
        );
        assert_eq!(
            idle_timeout_from_flag(1),
            Duration::from_secs(1),
            "seconds, not milliseconds"
        );
        assert_eq!(
            idle_timeout_from_flag(0),
            Duration::ZERO,
            "0 must disable, not become 1 s"
        );
    }

    #[test]
    fn cli_requires_listen_addr_and_rejects_zero_limits() {
        assert!(Cli::try_parse_from(["badvpn-udpgw"]).is_err());
        assert!(
            Cli::try_parse_from([
                "badvpn-udpgw",
                "--listen-addr",
                "127.0.0.1:7300",
                "--max-clients",
                "0"
            ])
            .is_err()
        );
        assert!(
            Cli::try_parse_from([
                "badvpn-udpgw",
                "--listen-addr",
                "127.0.0.1:7300",
                "--udp-mtu",
                "0"
            ])
            .is_err()
        );
        let two = Cli::try_parse_from([
            "badvpn-udpgw",
            "--listen-addr",
            "127.0.0.1:7300",
            "--listen-addr",
            "[::1]:7300",
        ])
        .unwrap();
        assert_eq!(two.listen_addrs.len(), 2);
    }
}
