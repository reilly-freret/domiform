//! Built-in mDNS responder for the Matter node, ported near-verbatim from
//! `rs-matter`'s `examples/src/common/mdns.rs` (v0.2.0). It enumerates a suitable
//! LAN interface (via `if-addrs`), binds a dual-stack UDP socket (via `socket2`),
//! joins the Matter mDNS multicast groups, and runs `BuiltinMdns` so controllers
//! can discover the node. Kept isolated so the discovery glue stays out of the
//! transport's main flow.

use std::net::UdpSocket;

use log::{debug, error, info, warn};
use socket2::{Domain, Protocol, Socket, Type};

use rs_matter::crypto::Crypto;
use rs_matter::error::{Error, ErrorCode};
use rs_matter::transport::network::mdns::builtin::{BuiltinMdns, Host};
use rs_matter::transport::network::mdns::{
    MDNS_IPV4_BROADCAST_ADDR, MDNS_IPV6_BROADCAST_ADDR, MDNS_SOCKET_DEFAULT_BIND_ADDR,
};
use rs_matter::transport::network::{Ipv4Addr, Ipv6Addr};
use rs_matter::Matter;

/// Interface name prefixes for point-to-point tunnels (Tailscale, WireGuard,
/// generic VPN `utun`/`tun`). These are `MULTICAST`-flagged on macOS but reject
/// multicast *group joins* (EINVAL), and aren't the LAN a controller is on — so we
/// skip them when auto-selecting the mDNS interface.
const TUNNEL_PREFIXES: &[&str] = &["utun", "tun", "tap", "wg", "tailscale", "ppp", "ipsec"];

fn is_tunnel(name: &str) -> bool {
    TUNNEL_PREFIXES.iter().any(|p| name.starts_with(p))
}

/// Pick a LAN interface and return its `(ipv4, ipv6, if-index)` for mDNS.
/// `override_name`, when set (from `matter_device.interface`), forces that
/// interface and skips the heuristic.
#[inline(never)]
fn initialize_network(override_name: Option<&str>) -> Result<(Ipv4Addr, Ipv6Addr, u32), Error> {
    let all = if_addrs::get_if_addrs().map_err(|_| ErrorCode::StdIoError)?;
    debug!("Available network interfaces: {all:?}");

    // An explicit interface override wins: find its IPv4 (required) and IPv6.
    if let Some(want) = override_name {
        let v4 = all.iter().find_map(|ia| match ia.addr {
            if_addrs::IfAddr::V4(ref v4) if ia.name == want => Some((v4.ip, ia.index.unwrap_or(0))),
            _ => None,
        });
        let v6 = all.iter().find_map(|ia| match ia.addr {
            if_addrs::IfAddr::V6(ref v6) if ia.name == want => Some(v6.ip),
            _ => None,
        });
        return match v4 {
            Some((ip, index)) => {
                let ipv6 = v6.unwrap_or(std::net::Ipv6Addr::UNSPECIFIED);
                info!("Using configured mDNS interface {want} with {ip}/{ipv6}");
                Ok((ip.octets().into(), ipv6.octets().into(), index))
            }
            None => {
                error!("Configured mDNS interface '{want}' has no IPv4 address / not found");
                Err(ErrorCode::StdIoError.into())
            }
        };
    }

    // Prefer an interface that has both an IPv6 address and a non-loopback IPv4
    // address; link-local IPv6 first (most likely the real LAN interface). Tunnel
    // interfaces (Tailscale/VPN `utun*`) are excluded — they can't join multicast.
    let find_ipv6_candidate = |ipv6_filter: fn(std::net::Ipv6Addr) -> bool| {
        all.iter()
            .filter(|ia| !ia.is_loopback() && !is_tunnel(&ia.name))
            .filter_map(|ia| match ia.addr {
                if_addrs::IfAddr::V6(ref v6) if ipv6_filter(v6.ip) => {
                    Some((ia.name.clone(), v6.ip, ia.index.unwrap_or(0)))
                }
                _ => None,
            })
            .find_map(|(iname, ipv6, index)| {
                all.iter()
                    .filter(|ia2| ia2.name == iname)
                    .find_map(|ia2| match ia2.addr {
                        if_addrs::IfAddr::V4(ref v4) => Some((iname.clone(), v4.ip, ipv6, index)),
                        _ => None,
                    })
            })
    };

    // Last-resort fallback: a broadcast-style interface (`eth*`/`eno*`/`en*`) even
    // without IPv6 — covers macOS Wi-Fi (`en0`) and Linux ethernet.
    let find_fallback_candidate = || {
        all.iter()
            .filter(|ia| !ia.is_loopback() && !is_tunnel(&ia.name))
            .filter(|ia| {
                ia.name.starts_with("eth")
                    || ia.name.starts_with("eno")
                    || ia.name.starts_with("en")
            })
            .map(|ia| match ia.addr {
                if_addrs::IfAddr::V4(ref v4) => (
                    ia.name.clone(),
                    v4.ip,
                    std::net::Ipv6Addr::UNSPECIFIED,
                    ia.index.unwrap_or(0),
                ),
                if_addrs::IfAddr::V6(ref v6) => (
                    ia.name.clone(),
                    std::net::Ipv4Addr::UNSPECIFIED,
                    v6.ip,
                    ia.index.unwrap_or(0),
                ),
            })
            .next()
    };

    let candidate = find_ipv6_candidate(|ip| ip.is_unicast_link_local())
        .or_else(|| find_ipv6_candidate(|_| true))
        .or_else(|| {
            warn!("No network interface with a suitable IPv6 address found");
            find_fallback_candidate()
        })
        .ok_or_else(|| {
            error!("Cannot find network interface suitable for mDNS broadcasting");
            ErrorCode::StdIoError
        })?;

    let (iname, ip, ipv6, index) = candidate;
    info!("Will use network interface {iname} with {ip}/{ipv6} for mDNS");
    Ok((ip.octets().into(), ipv6.octets().into(), index))
}

/// Run the built-in mDNS responder for `matter` until a fatal error.
/// `interface_override` forces a specific network interface (from config).
pub async fn run_mdns<C: Crypto>(
    matter: &Matter<'_>,
    crypto: C,
    interface_override: Option<&str>,
) -> Result<(), Error> {
    let (ipv4_addr, ipv6_addr, interface) = initialize_network(interface_override)?;

    // Socket family choice matters on macOS: a dual-stack IPv6 socket cannot do
    // IPv4 multicast (join *or* send) there, so on a v4-only interface (the common
    // home-LAN case) we must use a native IPv4 socket. When the interface has a
    // usable IPv6 address we prefer the dual-stack socket (both families over one
    // socket, as on Linux).
    let have_ipv6 = ipv6_addr != Ipv6Addr::UNSPECIFIED;
    let socket = if have_ipv6 {
        build_dual_stack_socket(ipv4_addr, interface)?
    } else {
        build_ipv4_socket(ipv4_addr)?
    };
    let socket = async_io::Async::<UdpSocket>::new_nonblocking(socket.into())?;

    BuiltinMdns::new()
        .run(
            &socket,
            &socket,
            &Host {
                hostname: "001122334455",
                ip: ipv4_addr,
                ipv6: ipv6_addr,
            },
            Some(ipv4_addr),
            // Only advertise over IPv6 when the interface actually has one.
            have_ipv6.then_some(interface),
            matter,
            crypto,
        )
        .await
}

/// A native IPv4 mDNS socket bound to port 5353, joined to the Matter IPv4 mDNS
/// group on `ipv4_addr`'s interface. Used on v4-only interfaces (and always on
/// macOS, where v4 multicast needs an `AF_INET` socket).
fn build_ipv4_socket(ipv4_addr: Ipv4Addr) -> Result<Socket, Error> {
    let socket = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
    socket.set_reuse_address(true)?;
    #[cfg(unix)]
    socket.set_reuse_port(true)?;
    let _ = socket.set_multicast_loop_v4(true);
    // Bind to 0.0.0.0:5353 (the mDNS port) so we both send and receive multicast.
    let bind: std::net::SocketAddr = (std::net::Ipv4Addr::UNSPECIFIED, 5353).into();
    socket.bind(&bind.into())?;
    socket
        .join_multicast_v4(&MDNS_IPV4_BROADCAST_ADDR, &ipv4_addr)
        .inspect_err(|e| error!("mDNS: IPv4 multicast join failed: {e}"))?;
    let _ = socket.set_multicast_if_v4(&ipv4_addr);
    Ok(socket)
}

/// A dual-stack IPv6 socket joined to both Matter mDNS groups. Used when the
/// interface has a usable IPv6 address (works on Linux; on macOS the v4-only path
/// above is taken instead).
fn build_dual_stack_socket(ipv4_addr: Ipv4Addr, interface: u32) -> Result<Socket, Error> {
    let socket = Socket::new(Domain::IPV6, Type::DGRAM, Some(Protocol::UDP))?;
    socket.set_reuse_address(true)?;
    #[cfg(unix)]
    socket.set_reuse_port(true)?;
    socket.set_only_v6(false)?;
    let _ = socket.set_multicast_loop_v4(true);
    let _ = socket.set_multicast_loop_v6(true);
    socket.bind(&MDNS_SOCKET_DEFAULT_BIND_ADDR.into())?;
    let joined_v6 = socket
        .join_multicast_v6(&MDNS_IPV6_BROADCAST_ADDR, interface)
        .inspect_err(|e| debug!("mDNS: IPv6 multicast join skipped: {e}"))
        .is_ok();
    let joined_v4 = socket
        .join_multicast_v4(&MDNS_IPV4_BROADCAST_ADDR, &ipv4_addr)
        .inspect_err(|e| debug!("mDNS: IPv4 multicast join skipped: {e}"))
        .is_ok();
    if !joined_v4 && !joined_v6 {
        error!("mDNS: could not join any multicast group; device won't be discoverable");
        return Err(ErrorCode::StdIoError.into());
    }
    Ok(socket)
}
