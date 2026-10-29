//! LocalSend-style discovery: JSON announce on a 224.0.0.0/24 multicast group, peers reply on the group.

use serde::{Deserialize, Serialize};
use socket2::{Domain, Protocol, Socket, Type};
use std::{
    io,
    net::{IpAddr, Ipv4Addr, SocketAddr, SocketAddrV4},
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

/// Main shares its mouse & keyboard; Client is controlled. `None` until the user picks.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    Main,
    Client,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct DeviceInfo {
    pub alias: String,
    pub device_type: DeviceType,
    pub os: String,
    pub fingerprint: String,
    pub aspect_ratio: f32,
    pub telemetry_port: u16,
    pub transfer_port: u16,
    pub kvm_port: u16,
    pub role: Option<Role>,
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

// shortcut: joins the group on the default interface only; pick per-interface if VPN/Hyper-V adapters steal it.
fn bind() -> io::Result<UdpSocket> {
    let s = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
    s.set_reuse_address(true)?; // several LOKA instances (simulator) on one host
    #[cfg(unix)]
    s.set_reuse_port(true)?;
    s.bind(&SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, PORT).into())?;
    s.join_multicast_v4(&GROUP, &Ipv4Addr::UNSPECIFIED)?;
    s.set_multicast_loop_v4(true)?;
    s.set_nonblocking(true)?;
    UdpSocket::from_std(s.into())
}

/// Announce `me` (again whenever it changes, and every few seconds), answer newcomers,
/// and report every peer heard. Runs forever.
pub async fn run(mut me: watch::Receiver<DeviceInfo>, mut on_peer: impl FnMut(Peer) + Send) -> io::Result<()> {
    let sock = bind()?;
    let group = SocketAddr::from((GROUP, PORT));
    let encode = |me: &DeviceInfo, announce| serde_json::to_vec(&Announcement { info: me.clone(), announce }).expect("plain struct");
    let hello = encode(&me.borrow(), true); // watch guard must not live across an await
    sock.send_to(&hello, group).await?;
    let mut heartbeat = tokio::time::interval(HEARTBEAT);
    let mut buf = [0u8; 2048];
    loop {
        tokio::select! {
            r = sock.recv_from(&mut buf) => {
                let (n, from) = r?;
                let Ok(a) = serde_json::from_slice::<Announcement>(&buf[..n]) else { continue };
                if a.info.fingerprint == me.borrow().fingerprint {
                    continue;
                }
                if a.announce {
                    let reply = encode(&me.borrow(), false);
                    sock.send_to(&reply, group).await?;
                }
                on_peer(Peer { info: a.info, addr: from.ip(), seen: Instant::now() });
            }
            _ = heartbeat.tick() => {
                let beat = encode(&me.borrow(), false);
                sock.send_to(&beat, group).await?;
            }
            changed = me.changed() => {
                if changed.is_err() {
                    return Ok(()); // app shutting down
                }
                // role switch: ask everyone to answer so lists refresh immediately
                let again = encode(&me.borrow_and_update(), true);
                sock.send_to(&again, group).await?;
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
            role: Some(Role::Client),
        };
        let v: serde_json::Value = serde_json::to_value(Announcement { info, announce: true }).unwrap();
        assert_eq!(v["deviceType"], "desktop");
        assert_eq!(v["telemetryPort"], 1);
        assert_eq!(v["announce"], true);
        assert_eq!(v["role"], "client");
    }
}
