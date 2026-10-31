//! LocalSend-style discovery: JSON announce on a 224.0.0.0/24 multicast group on every interface;
//! peers answer an announce by unicast. Manually added hosts get the announce by unicast too,
//! for networks that drop multicast (guest Wi-Fi, client isolation).

use serde::{Deserialize, Serialize};
use socket2::{Domain, Protocol, Socket, Type};
use std::{
    collections::HashSet,
    io,
    net::{IpAddr, Ipv4Addr, SocketAddrV4},
    time::{Duration, Instant},
};
use tokio::{net::UdpSocket, sync::watch};

pub const GROUP: Ipv4Addr = Ipv4Addr::new(224, 0, 0, 168);
pub const PORT: u16 = 53318;

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum DeviceType {
    Desktop,
    Mobile,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct DeviceInfo {
    pub alias: String,
    pub device_type: DeviceType,
    pub os: String,
    /// SHA-256 of the device's certificate (`secure`): checked on every connection
    pub fingerprint: String,
    pub aspect_ratio: f32,
    pub telemetry_port: u16,
    pub transfer_port: u16,
    pub kvm_port: u16,
    /// wire version (`kvm::PROTOCOL`); 0 = before it was announced (v0.5, TCP links)
    #[serde(default)]
    pub proto: u32,
    /// hardware address, so paired devices can wake this one (Wake-on-LAN)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mac: Option<String>,
}

#[derive(Serialize, Deserialize)]
struct Announcement {
    #[serde(flatten)]
    info: DeviceInfo,
    announce: bool,
}

#[derive(Serialize, Clone, Debug, PartialEq)]
pub struct Peer {
    pub info: DeviceInfo,
    pub addr: IpAddr,
    #[serde(skip)]
    pub seen: Instant,
}

/// Peers silent for longer than this are gone (3 missed heartbeats).
pub const STALE: Duration = Duration::from_secs(15);
const HEARTBEAT: Duration = Duration::from_secs(5);

fn bind() -> io::Result<UdpSocket> {
    let s = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
    s.set_reuse_address(true)?; // several LOKA instances (simulator) on one host
    #[cfg(unix)]
    s.set_reuse_port(true)?;
    s.bind(&SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, PORT).into())?;
    s.set_multicast_loop_v4(true)?;
    s.set_nonblocking(true)?;
    UdpSocket::from_std(s.into())
}

/// IPv4 addresses of every interface that is up, loopback excluded.
pub fn local_ipv4() -> Vec<Ipv4Addr> {
    let all = if_addrs::get_if_addrs().unwrap_or_default();
    all.iter()
        .filter(|i| i.is_oper_up() && !i.is_loopback())
        .filter_map(|i| match i.ip() {
            IpAddr::V4(ip) => Some(ip),
            IpAddr::V6(_) => None,
        })
        .collect()
}

/// Announce out of every interface (the OS default one may be a VPN or Hyper-V adapter), joining the group
/// on interfaces that came up since last time, then by unicast to each manual host.
/// Send errors are expected (adapter going down, no route) and must not stop discovery.
async fn broadcast(sock: &UdpSocket, joined: &mut HashSet<Ipv4Addr>, me: &DeviceInfo, announce: bool, hosts: &[IpAddr]) {
    let s = socket2::SockRef::from(sock);
    let ifaces = local_ipv4();
    // a new network (Wi-Fi joined after launch): ask everyone there to answer
    let mut new = false;
    for ip in &ifaces {
        if !joined.contains(ip) && s.join_multicast_v4(&GROUP, ip).is_ok() {
            joined.insert(*ip);
            new = true;
        }
    }
    let msg = encode(me, announce || new);
    for ip in &ifaces {
        if s.set_multicast_if_v4(ip).is_ok() {
            let _ = sock.send_to(&msg, (GROUP, PORT)).await;
        }
    }
    // hosts only hear unicast, so they must be asked to answer every time to stay fresh on our side
    let ask = encode(me, true);
    for &h in hosts {
        let _ = sock.send_to(&ask, (h, PORT)).await;
    }
}

fn encode(me: &DeviceInfo, announce: bool) -> Vec<u8> {
    serde_json::to_vec(&Announcement { info: me.clone(), announce }).expect("plain struct")
}

/// Ask `host` to answer right away (manual add), from a throwaway socket: answers go to our discovery port.
pub async fn poke(host: IpAddr, me: &DeviceInfo) -> io::Result<()> {
    let sock = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).await?;
    sock.send_to(&encode(me, true), (host, PORT)).await.map(drop)
}

/// Announce `me` (again whenever it changes, and every few seconds), answer newcomers,
/// and report every peer heard. `hosts` = manually added addresses. Runs forever.
pub async fn run(
    mut me: watch::Receiver<DeviceInfo>,
    hosts: impl Fn() -> Vec<IpAddr> + Send,
    mut on_peer: impl FnMut(Peer) + Send,
) -> io::Result<()> {
    let sock = bind()?;
    let mut joined = HashSet::new();
    let mut heartbeat = tokio::time::interval(HEARTBEAT); // first tick is immediate: the hello
    let mut buf = [0u8; 2048];
    loop {
        tokio::select! {
            r = sock.recv_from(&mut buf) => {
                // Windows reports an ICMP "port unreachable" from an earlier unicast as a recv error
                let Ok((n, from)) = r else {
                    tokio::time::sleep(Duration::from_millis(50)).await;
                    continue;
                };
                let Ok(a) = serde_json::from_slice::<Announcement>(&buf[..n]) else { continue };
                if a.info.fingerprint == me.borrow().fingerprint {
                    continue;
                }
                if a.announce {
                    let reply = encode(&me.borrow(), false);
                    let _ = sock.send_to(&reply, (from.ip(), PORT)).await;
                    // behind NAT (Android emulator) the sender's discovery port shows up remapped: answer there too
                    if from.port() != PORT {
                        let _ = sock.send_to(&reply, from).await;
                    }
                }
                on_peer(Peer { info: a.info, addr: from.ip(), seen: Instant::now() });
            }
            _ = heartbeat.tick() => {
                let info = me.borrow().clone(); // watch guard must not live across an await
                broadcast(&sock, &mut joined, &info, false, &hosts()).await;
            }
            changed = me.changed() => {
                if changed.is_err() {
                    return Ok(()); // app shutting down
                }
                // role switch: ask everyone to answer so lists refresh immediately
                let info = me.borrow_and_update().clone();
                broadcast(&sock, &mut joined, &info, true, &hosts()).await;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn announcement_wire_format_is_flat_camel_case() {
        let info = DeviceInfo {
            alias: "desk".into(),
            device_type: DeviceType::Desktop,
            os: "windows".into(),
            fingerprint: "f".into(),
            aspect_ratio: 1.5,
            telemetry_port: 1,
            transfer_port: 2,
            kvm_port: 3,
            proto: 2,
            mac: None,
        };
        let v: serde_json::Value = serde_json::to_value(Announcement { info, announce: true }).unwrap();
        assert_eq!(v["deviceType"], "desktop");
        assert_eq!(v["telemetryPort"], 1);
        assert_eq!(v["proto"], 2);
        assert_eq!(v["announce"], true);
        let mut old = v;
        old.as_object_mut().unwrap().remove("proto");
        assert_eq!(serde_json::from_value::<Announcement>(old).unwrap().info.proto, 0, "a v0.5 device announces no version");
    }
}
