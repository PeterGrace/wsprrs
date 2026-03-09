/// Multicast UDP socket factory and receive loop for ka9q-radio streams.
///
/// Uses `socket2` to create a `SOCK_DGRAM` socket, set `SO_REUSEPORT`,
/// bind to the given port, and join the multicast group.  The socket
/// is then wrapped in a Tokio `UdpSocket` for async receives.
///
/// Call [`build_socket`] once for the RTP data port and once for the
/// status port; both use the same multicast group address.
use std::net::{IpAddr, Ipv4Addr, SocketAddr};

use anyhow::{Context, Result};
use socket2::{Domain, Protocol, Socket, Type};
use tokio::net::UdpSocket;
use tokio::sync::{mpsc, Notify};

use crate::config::Config;

/// Maximum UDP datagram size (IP limit).
const MAX_UDP: usize = 65_535;

/// A raw UDP datagram received from the multicast group.
#[derive(Debug)]
pub struct ReceivedPacket {
    /// The datagram bytes.
    pub data: Vec<u8>,
    /// Number of valid bytes in `data`.
    pub len: usize,
}

/// Build and bind a multicast UDP socket on the given `port`.
///
/// The multicast group address and local interface are taken from `cfg`.
/// This function may be called twice — once for the RTP data port
/// (`cfg.multicast_port`) and once for the status port (`cfg.status_port`).
///
/// # Arguments
///
/// * `cfg`  — runtime configuration supplying the multicast group and interface
/// * `port` — UDP port to bind (data port or status port)
///
/// # Errors
///
/// Propagates any socket / bind / multicast errors.
pub fn build_socket(cfg: &Config, port: u16) -> Result<UdpSocket> {
    let domain = if cfg.multicast_addr.is_ipv4() {
        Domain::IPV4
    } else {
        Domain::IPV6
    };

    let sock = Socket::new(domain, Type::DGRAM, Some(Protocol::UDP))
        .context("socket2::Socket::new failed")?;

    // Allow multiple processes to bind to the same port (e.g. multiple
    // instances tuned to different SSRCs).
    sock.set_reuse_port(true).context("SO_REUSEPORT failed")?;
    sock.set_reuse_address(true)
        .context("SO_REUSEADDR failed")?;

    // Bind to INADDR_ANY:port so the kernel delivers multicast datagrams.
    let bind_addr: SocketAddr = SocketAddr::new(
        if cfg.multicast_addr.is_ipv4() {
            IpAddr::V4(Ipv4Addr::UNSPECIFIED)
        } else {
            IpAddr::V6(std::net::Ipv6Addr::UNSPECIFIED)
        },
        port,
    );
    sock.bind(&bind_addr.into()).context("bind failed")?;

    // Join the multicast group.
    match (cfg.multicast_addr, cfg.local_addr) {
        (IpAddr::V4(mcast), IpAddr::V4(iface)) => {
            sock.join_multicast_v4(&mcast, &iface)
                .context("join_multicast_v4 failed")?;
        }
        (IpAddr::V6(mcast), _) => {
            // Interface index 0 = let the OS choose.
            sock.join_multicast_v6(&mcast, 0)
                .context("join_multicast_v6 failed")?;
        }
        _ => anyhow::bail!("multicast_addr and local_addr address families mismatch"),
    }

    // Set non-blocking mode so we can hand the socket to Tokio.
    sock.set_nonblocking(true)
        .context("set_nonblocking failed")?;

    let udp: std::net::UdpSocket = sock.into();
    UdpSocket::from_std(udp).context("UdpSocket::from_std failed")
}

/// Async receive loop: reads datagrams and forwards them on `tx`.
///
/// This function runs until `shutdown` is notified.  Dropped packets
/// (when the channel is full) are counted and logged at warn level.
///
/// # Arguments
///
/// * `socket`   — the bound multicast socket
/// * `tx`       — sender side of the packet channel
/// * `shutdown` — shared `Notify`; when triggered this function returns
pub async fn receive_loop(
    socket: UdpSocket,
    tx: mpsc::Sender<ReceivedPacket>,
    shutdown: std::sync::Arc<Notify>,
) {
    // Reuse a single heap allocation for the receive buffer.
    let mut buf = vec![0u8; MAX_UDP];
    let mut dropped: u64 = 0;

    loop {
        tokio::select! {
            biased;

            _ = shutdown.notified() => {
                tracing::info!("receive_loop: shutdown signal received");
                break;
            }

            result = socket.recv_from(&mut buf) => {
                match result {
                    Ok((len, _src)) => {
                        // try_send: never block the recv loop; drop if full.
                        let pkt = ReceivedPacket {
                            data: buf[..len].to_vec(),
                            len,
                        };
                        if tx.try_send(pkt).is_err() {
                            dropped += 1;
                            if dropped.is_power_of_two() {
                                tracing::warn!(dropped, "packet channel full; datagrams dropped");
                            }
                        }
                    }
                    Err(e) => {
                        tracing::error!(error = %e, "recv_from error");
                    }
                }
            }
        }
    }

    if dropped > 0 {
        tracing::warn!(
            total_dropped = dropped,
            "receive_loop finished; total packets dropped"
        );
    }
}
