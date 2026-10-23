//! 自分の外から見えるアドレスを STUN (RFC 5389 Binding) で 1 回だけ聞く。
//! 結果は server reflexive 候補として offer に入れる (拠点の外の見る人と P2P で繋ぐため)。

use std::net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket};
use std::time::{Duration, Instant};

const MAGIC: u32 = 0x2112_A442;

pub fn request(txid: [u8; 12]) -> [u8; 20] {
    let mut b = [0u8; 20];
    b[0..2].copy_from_slice(&0x0001u16.to_be_bytes()); // Binding Request
    b[4..8].copy_from_slice(&MAGIC.to_be_bytes());
    b[8..20].copy_from_slice(&txid);
    b
}

/// Binding Success の XOR-MAPPED-ADDRESS (無ければ MAPPED-ADDRESS) を読む。IPv4 のみ
pub fn parse_response(b: &[u8], txid: &[u8; 12]) -> Option<SocketAddr> {
    if b.len() < 20 || u16::from_be_bytes([b[0], b[1]]) != 0x0101 || &b[8..20] != txid {
        return None;
    }
    let len = u16::from_be_bytes([b[2], b[3]]) as usize;
    let attrs = b.get(20..20 + len)?;
    let mut i = 0;
    let mut mapped = None;
    while i + 4 <= attrs.len() {
        let typ = u16::from_be_bytes([attrs[i], attrs[i + 1]]);
        let alen = u16::from_be_bytes([attrs[i + 2], attrs[i + 3]]) as usize;
        let v = attrs.get(i + 4..i + 4 + alen)?;
        if v.len() >= 8 && v[1] == 0x01 {
            let port = u16::from_be_bytes([v[2], v[3]]);
            let ip = u32::from_be_bytes([v[4], v[5], v[6], v[7]]);
            match typ {
                0x0020 => {
                    let ip = Ipv4Addr::from(ip ^ MAGIC);
                    return Some(SocketAddr::new(IpAddr::V4(ip), port ^ (MAGIC >> 16) as u16));
                }
                0x0001 => mapped = Some(SocketAddr::new(IpAddr::V4(Ipv4Addr::from(ip)), port)),
                _ => {}
            }
        }
        i += 4 + alen.div_ceil(4) * 4;
    }
    mapped
}

/// `socket` から STUN サーバーへ聞く (再送 3 回、合計 1.5 秒まで)。取れなければ None (LAN 内だけで繋がる)
pub fn query(socket: &UdpSocket, server: SocketAddr, txid: [u8; 12]) -> Option<SocketAddr> {
    let req = request(txid);
    let mut buf = [0u8; 512];
    for _ in 0..3 {
        socket.send_to(&req, server).ok()?;
        let deadline = Instant::now() + Duration::from_millis(500);
        while let Some(left) = deadline.checked_duration_since(Instant::now()) {
            if left.is_zero() || socket.set_read_timeout(Some(left)).is_err() {
                break;
            }
            match socket.recv_from(&mut buf) {
                Ok((n, from)) if from == server => {
                    if let Some(a) = parse_response(&buf[..n], &txid) {
                        return Some(a);
                    }
                }
                Ok(_) => {}
                Err(_) => break,
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_layout() {
        let r = request([7; 12]);
        assert_eq!(&r[0..4], &[0, 1, 0, 0]);
        assert_eq!(&r[4..8], &[0x21, 0x12, 0xa4, 0x42]);
        assert_eq!(&r[8..], &[7; 12]);
    }

    #[test]
    fn parses_xor_mapped_address() {
        let txid = [1; 12];
        // 203.0.113.5:40000
        let port = 40000u16 ^ 0x2112;
        let ip = u32::from(Ipv4Addr::new(203, 0, 113, 5)) ^ MAGIC;
        let mut b = vec![0x01, 0x01, 0, 12, 0x21, 0x12, 0xa4, 0x42];
        b.extend_from_slice(&txid);
        b.extend_from_slice(&[0x00, 0x20, 0, 8, 0, 1]);
        b.extend_from_slice(&port.to_be_bytes());
        b.extend_from_slice(&ip.to_be_bytes());
        assert_eq!(parse_response(&b, &txid), Some("203.0.113.5:40000".parse().unwrap()));
        assert_eq!(parse_response(&b, &[2; 12]), None);
    }
}
