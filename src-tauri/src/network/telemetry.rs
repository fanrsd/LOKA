//! 60 Hz spatial telemetry: fixed 12-byte little-endian UDP packets, built on the stack.

use serde::Serialize;
use std::{io, net::SocketAddr};
use tokio::net::UdpSocket;

pub const PORT: u16 = 53319;
pub const MAGIC: [u8; 2] = *b"LK";
pub const PACKET_LEN: usize = 12;

#[derive(Serialize, Clone, Copy, Debug, PartialEq)]
#[serde(rename_all = "lowercase")]
#[repr(u8)]
pub enum PacketType {
    Proximity = 1,
    Drop = 2,
    Leave = 3,
}

#[derive(Serialize, Clone, Copy, Debug, PartialEq)]
#[serde(rename_all = "lowercase")]
#[repr(u8)]
pub enum PayloadKind {
    None = 0,
    File = 1,
    Text = 2,
    Url = 3,
}

#[derive(Serialize, Clone, Copy, Debug, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct SpatialPacket {
    pub kind: PacketType,
    pub normalized_y: f32,
    pub proximity: f32,
    pub payload: PayloadKind,
}

impl SpatialPacket {
    pub fn encode(&self) -> [u8; PACKET_LEN] {
        let mut b = [0u8; PACKET_LEN];
        b[0..2].copy_from_slice(&MAGIC);
        b[2] = self.kind as u8;
        b[3..7].copy_from_slice(&self.normalized_y.to_le_bytes());
        b[7..11].copy_from_slice(&self.proximity.to_le_bytes());
        b[11] = self.payload as u8;
        b
    }

    pub fn decode(b: &[u8]) -> Option<Self> {
        if b.len() != PACKET_LEN || b[0..2] != MAGIC {
            return None;
        }
        let kind = match b[2] {
            1 => PacketType::Proximity,
            2 => PacketType::Drop,
            3 => PacketType::Leave,
            _ => return None,
        };
        let payload = match b[11] {
            0 => PayloadKind::None,
            1 => PayloadKind::File,
            2 => PayloadKind::Text,
            3 => PayloadKind::Url,
            _ => return None,
        };
        let f = |r: std::ops::Range<usize>| f32::from_le_bytes(b[r].try_into().unwrap());
        let (normalized_y, proximity) = (f(3..7), f(7..11));
        // clamp at the trust boundary; NaN fails the range check
        if !(0.0..=1.0).contains(&normalized_y) || !(0.0..=1.0).contains(&proximity) {
            return None;
        }
        Some(Self { kind, normalized_y, proximity, payload })
    }
}

pub async fn sender() -> io::Result<UdpSocket> {
    UdpSocket::bind(("0.0.0.0", 0)).await
}

pub async fn send(sock: &UdpSocket, to: SocketAddr, p: &SpatialPacket) {
    let _ = sock.send_to(&p.encode(), to).await; // unreliable by design; next tick supersedes
}

/// Receive packets forever; malformed datagrams are dropped silently.
pub async fn listen(port: u16, mut on_packet: impl FnMut(SpatialPacket) + Send) -> io::Result<()> {
    let sock = UdpSocket::bind(("0.0.0.0", port)).await?;
    let mut buf = [0u8; 64];
    loop {
        let (n, _) = sock.recv_from(&mut buf).await?;
        if let Some(p) = SpatialPacket::decode(&buf[..n]) {
            on_packet(p);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_and_layout() {
        let p = SpatialPacket { kind: PacketType::Drop, normalized_y: 0.25, proximity: 1.0, payload: PayloadKind::Url };
        let b = p.encode();
        assert_eq!(&b[..3], b"LK\x02");
        assert_eq!(b[11], 3);
        assert_eq!(SpatialPacket::decode(&b), Some(p));
    }

    #[test]
    fn rejects_garbage() {
        let mut b = SpatialPacket { kind: PacketType::Leave, normalized_y: 0.0, proximity: 0.0, payload: PayloadKind::None }.encode();
        assert!(SpatialPacket::decode(&b[..11]).is_none());
        b[3..7].copy_from_slice(&f32::NAN.to_le_bytes());
        assert!(SpatialPacket::decode(&b).is_none());
        b[0] = b'X';
        assert!(SpatialPacket::decode(&b).is_none());
    }
}
