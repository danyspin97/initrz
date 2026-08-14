use std::fs;
use std::net::{IpAddr, Ipv4Addr};

use anyhow::{bail, Context, Result};
use log::{info, warn};
use netlink_packet_core::{
    NetlinkHeader, NetlinkMessage, NetlinkPayload, NLM_F_ACK, NLM_F_CREATE, NLM_F_EXCL,
    NLM_F_REQUEST,
};
use netlink_packet_route::{
    address::{AddressAttribute, AddressHeader, AddressMessage, AddressScope},
    link::{LinkFlags, LinkHeader, LinkMessage},
    route::{RouteAddress, RouteAttribute, RouteHeader, RouteMessage, RouteProtocol, RouteScope, RouteType},
    AddressFamily, RouteNetlinkMessage,
};
use netlink_sys::{protocols::NETLINK_ROUTE, Socket, SocketAddr};

/// Linux's well-known main routing table id (RT_TABLE_MAIN).
const RT_TABLE_MAIN: u8 = 254;

struct StaticIpConfig {
    client_ip: Ipv4Addr,
    gateway_ip: Option<Ipv4Addr>,
    prefix_len: u8,
    device: Option<String>,
}

/// Bring up networking from the kernel `ip=` and `nameserver=` cmdline parameters, if present.
///
/// Only static configuration is supported (`ip=<client>:<server>:<gw>:<mask>:<host>:<dev>:off`
/// and friends) - DHCP is intentionally out of scope. If `ip=` requests DHCP/autoconf, or there
/// is no `ip=` parameter at all, that part is a no-op (logged, non-fatal) rather than an error.
/// `nameserver=<ip>` (repeatable) is independent of `ip=` and writes `/etc/resolv.conf`.
pub fn configure_from_cmdline(cmdline: &[String]) -> Result<()> {
    write_resolv_conf(cmdline)?;

    let Some(param) = cmdline.iter().filter(|arg| arg.starts_with("ip=")).last() else {
        return Ok(());
    };
    let value = param.strip_prefix("ip=").unwrap();

    let config = match parse_ip_param(value)? {
        Some(config) => config,
        None => {
            warn!(
                "ip={} requests DHCP/autoconf, which initrz does not support; skipping network bring-up",
                value
            );
            return Ok(());
        }
    };

    let device = match config.device {
        Some(device) => device,
        None => pick_default_device()?,
    };

    info!(
        "configuring {} with {}/{}{}",
        device,
        config.client_ip,
        config.prefix_len,
        config
            .gateway_ip
            .map(|gw| format!(" via {}", gw))
            .unwrap_or_default()
    );

    let ifindex = get_ifindex(&device)?;

    let mut socket =
        Socket::new(NETLINK_ROUTE).with_context(|| "unable to create netlink route socket")?;
    socket
        .bind_auto()
        .with_context(|| "unable to bind netlink route socket")?;
    socket
        .connect(&SocketAddr::new(0, 0))
        .with_context(|| "unable to connect netlink socket to the kernel")?;

    set_link_up(&socket, ifindex)
        .with_context(|| format!("unable to bring up interface {:?}", device))?;
    add_address(&socket, ifindex, config.client_ip, config.prefix_len)
        .with_context(|| format!("unable to configure address on {:?}", device))?;
    if let Some(gateway_ip) = config.gateway_ip {
        add_default_route(&socket, ifindex, gateway_ip)
            .with_context(|| format!("unable to add default route via {}", gateway_ip))?;
    }

    Ok(())
}

fn parse_ip_param(value: &str) -> Result<Option<StaticIpConfig>> {
    if value.is_empty() {
        bail!("empty ip= parameter");
    }

    let fields: Vec<&str> = value.split(':').collect();

    // Whole-value shorthand: ip=dhcp / ip=on / ip=any / ip=both
    if fields.len() == 1 {
        return match fields[0] {
            "dhcp" | "on" | "any" | "both" => Ok(None),
            other => bail!("unrecognized ip= shorthand {:?}", other),
        };
    }

    // Device-qualified shorthand: ip=<device>:dhcp
    if fields.len() == 2 && matches!(fields[1], "dhcp" | "on" | "any" | "both") {
        return Ok(None);
    }

    let get = |i: usize| fields.get(i).copied().unwrap_or("");

    let autoconf = get(6);
    if matches!(autoconf, "dhcp" | "on" | "any" | "both") {
        return Ok(None);
    }

    let client_ip = get(0);
    if client_ip.is_empty() {
        // No static client-ip and not requesting autoconf either - nothing to configure.
        return Ok(None);
    }
    let client_ip: Ipv4Addr = client_ip
        .parse()
        .with_context(|| format!("invalid client-ip {:?} in ip=", client_ip))?;

    let gateway_ip = get(2);
    let gateway_ip = if gateway_ip.is_empty() {
        None
    } else {
        Some(
            gateway_ip
                .parse::<Ipv4Addr>()
                .with_context(|| format!("invalid gateway-ip {:?} in ip=", gateway_ip))?,
        )
    };

    let netmask = get(3);
    let netmask: Ipv4Addr = if netmask.is_empty() {
        Ipv4Addr::new(255, 255, 255, 0)
    } else {
        netmask
            .parse()
            .with_context(|| format!("invalid netmask {:?} in ip=", netmask))?
    };
    let prefix_len = netmask_to_prefix_len(netmask)?;

    let device = get(5);
    let device = if device.is_empty() {
        None
    } else {
        Some(device.to_string())
    };

    Ok(Some(StaticIpConfig {
        client_ip,
        gateway_ip,
        prefix_len,
        device,
    }))
}

fn netmask_to_prefix_len(mask: Ipv4Addr) -> Result<u8> {
    let bits = u32::from(mask);
    let prefix = bits.leading_ones();
    // A valid netmask is a contiguous run of 1 bits followed by 0 bits; shifting left by
    // `prefix` should clear everything if that holds.
    if bits.checked_shl(prefix).unwrap_or(0) != 0 {
        bail!("netmask {} is not a contiguous prefix mask", mask);
    }
    Ok(prefix as u8)
}

/// Writes `/etc/resolv.conf` from one or more `nameserver=<ip>` cmdline parameters, so hostname
/// lookups (e.g. an NFS/CIFS root given by name) work during the initramfs stage. A no-op if no
/// `nameserver=` parameter is present.
fn write_resolv_conf(cmdline: &[String]) -> Result<()> {
    let nameservers: Vec<&str> = cmdline
        .iter()
        .filter_map(|arg| arg.strip_prefix("nameserver="))
        .collect();
    if nameservers.is_empty() {
        return Ok(());
    }

    info!("writing /etc/resolv.conf with nameserver(s) {:?}", nameservers);
    fs::write("/etc/resolv.conf", format_resolv_conf(&nameservers))
        .with_context(|| "unable to write /etc/resolv.conf")
}

fn format_resolv_conf(nameservers: &[&str]) -> String {
    nameservers.iter().map(|ns| format!("nameserver {}\n", ns)).collect()
}

fn pick_default_device() -> Result<String> {
    let mut candidates: Vec<String> = fs::read_dir("/sys/class/net")
        .with_context(|| "unable to read /sys/class/net")?
        .filter_map(|entry| entry.ok())
        .filter_map(|entry| entry.file_name().into_string().ok())
        .filter(|name| name != "lo")
        .collect();
    candidates.sort();
    candidates
        .into_iter()
        .next()
        .with_context(|| "no network interface found to configure")
}

fn get_ifindex(device: &str) -> Result<u32> {
    let path = format!("/sys/class/net/{}/ifindex", device);
    let content = fs::read_to_string(&path).with_context(|| format!("unable to read {:?}", path))?;
    content
        .trim()
        .parse()
        .with_context(|| format!("invalid ifindex in {:?}", path))
}

fn set_link_up(socket: &Socket, ifindex: u32) -> Result<()> {
    // LinkMessage/AddressMessage/RouteMessage are #[non_exhaustive], so they can't be built with
    // struct-literal syntax from outside the crate - go through Default + field assignment.
    let mut link_message = LinkMessage::default();
    link_message.header = LinkHeader {
        index: ifindex,
        flags: LinkFlags::Up,
        change_mask: LinkFlags::Up,
        ..Default::default()
    };
    let message = RouteNetlinkMessage::NewLink(link_message);
    send_and_ack(socket, message, NLM_F_REQUEST | NLM_F_ACK)
}

fn add_address(socket: &Socket, ifindex: u32, address: Ipv4Addr, prefix_len: u8) -> Result<()> {
    let mut address_message = AddressMessage::default();
    address_message.header = AddressHeader {
        family: AddressFamily::Inet,
        prefix_len,
        scope: AddressScope::Universe,
        index: ifindex,
        ..Default::default()
    };
    address_message.attributes = vec![
        AddressAttribute::Local(IpAddr::V4(address)),
        AddressAttribute::Address(IpAddr::V4(address)),
    ];
    let message = RouteNetlinkMessage::NewAddress(address_message);
    send_and_ack(
        socket,
        message,
        NLM_F_REQUEST | NLM_F_ACK | NLM_F_CREATE | NLM_F_EXCL,
    )
}

fn add_default_route(socket: &Socket, ifindex: u32, gateway: Ipv4Addr) -> Result<()> {
    let mut route_message = RouteMessage::default();
    route_message.header = RouteHeader {
        address_family: AddressFamily::Inet,
        table: RT_TABLE_MAIN,
        protocol: RouteProtocol::Boot,
        scope: RouteScope::Universe,
        kind: RouteType::Unicast,
        ..Default::default()
    };
    route_message.attributes = vec![
        RouteAttribute::Gateway(RouteAddress::Inet(gateway)),
        RouteAttribute::Oif(ifindex),
    ];
    let message = RouteNetlinkMessage::NewRoute(route_message);
    send_and_ack(
        socket,
        message,
        NLM_F_REQUEST | NLM_F_ACK | NLM_F_CREATE | NLM_F_EXCL,
    )
}

fn send_and_ack(socket: &Socket, payload: RouteNetlinkMessage, flags: u16) -> Result<()> {
    let mut header = NetlinkHeader::default();
    header.flags = flags;
    let mut message = NetlinkMessage::new(header, NetlinkPayload::InnerMessage(payload));
    message.finalize();

    let mut buf = vec![0u8; message.buffer_len()];
    message.serialize(&mut buf);
    socket
        .send(&buf, 0)
        .with_context(|| "failed to send netlink request")?;

    let mut recv_buf = vec![0u8; 4096];
    let n = socket
        .recv(&mut recv_buf, 0)
        .with_context(|| "failed to receive netlink reply")?;
    let reply = NetlinkMessage::<RouteNetlinkMessage>::deserialize(&recv_buf[..n])
        .with_context(|| "failed to parse netlink reply")?;

    match reply.payload {
        NetlinkPayload::Error(err) if err.code.is_none() => Ok(()),
        NetlinkPayload::Error(err) => {
            bail!("netlink request failed with error code {}", err.raw_code())
        }
        other => bail!("unexpected netlink reply: {:?}", other),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_static_ip() {
        let config = parse_ip_param("192.168.1.100::192.168.1.1:255.255.255.0::eth0:off")
            .unwrap()
            .unwrap();
        assert_eq!(config.client_ip, Ipv4Addr::new(192, 168, 1, 100));
        assert_eq!(config.gateway_ip, Some(Ipv4Addr::new(192, 168, 1, 1)));
        assert_eq!(config.prefix_len, 24);
        assert_eq!(config.device.as_deref(), Some("eth0"));
    }

    #[test]
    fn test_parse_no_gateway_no_device() {
        let config = parse_ip_param("10.0.0.5:::255.255.0.0")
            .unwrap()
            .unwrap();
        assert_eq!(config.client_ip, Ipv4Addr::new(10, 0, 0, 5));
        assert_eq!(config.gateway_ip, None);
        assert_eq!(config.prefix_len, 16);
        assert_eq!(config.device, None);
    }

    #[test]
    fn test_parse_bare_value_is_error() {
        // Only one field and it's not a known keyword - reported as an error rather than
        // guessed at, since the standard syntax always keeps the colons even for empty fields.
        assert!(parse_ip_param("10.0.0.5").is_err());
    }

    #[test]
    fn test_parse_dhcp_shorthand() {
        assert!(parse_ip_param("dhcp").unwrap().is_none());
        assert!(parse_ip_param("eth0:dhcp").unwrap().is_none());
    }

    #[test]
    fn test_parse_dhcp_autoconf_field() {
        assert!(parse_ip_param("192.168.1.100:::255.255.255.0::eth0:dhcp")
            .unwrap()
            .is_none());
    }

    #[test]
    fn test_netmask_to_prefix_len() {
        assert_eq!(
            netmask_to_prefix_len(Ipv4Addr::new(255, 255, 255, 0)).unwrap(),
            24
        );
        assert_eq!(
            netmask_to_prefix_len(Ipv4Addr::new(255, 255, 255, 255)).unwrap(),
            32
        );
        assert_eq!(netmask_to_prefix_len(Ipv4Addr::new(0, 0, 0, 0)).unwrap(), 0);
        assert!(netmask_to_prefix_len(Ipv4Addr::new(255, 0, 255, 0)).is_err());
    }

    #[test]
    fn test_format_resolv_conf_single() {
        assert_eq!(format_resolv_conf(&["8.8.8.8"]), "nameserver 8.8.8.8\n");
    }

    #[test]
    fn test_format_resolv_conf_multiple() {
        assert_eq!(
            format_resolv_conf(&["8.8.8.8", "1.1.1.1"]),
            "nameserver 8.8.8.8\nnameserver 1.1.1.1\n"
        );
    }

    #[test]
    fn test_format_resolv_conf_empty() {
        assert_eq!(format_resolv_conf(&[]), "");
    }
}
