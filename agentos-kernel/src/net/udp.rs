//! Real UDP header construction/parsing + the RFC 768 pseudo-header
//! checksum - pure byte-buffer logic with no hardware dependency at all,
//! mirroring `net::tcp`'s own "protocol layer first, wire to real device
//! I/O later" order (Fase 88 before Fase 89-90's real TCP round trips).
//! The pseudo-header assembly itself lives in `icmp::pseudo_header_
//! checksum` (Fase 119) - UDP's own pseudo-header is byte-for-byte the
//! same 12-byte shape as TCP's (source IP, dest IP, a zero byte,
//! protocol number, length), so this module reuses it directly, passing
//! `IP_PROTOCOL_UDP` (17) where `net::tcp` passes `IP_PROTOCOL_TCP` (6) -
//! the one genuinely NEW piece of logic in this whole module is the one
//! real RFC 768 wrinkle TCP doesn't have: a checksum of exactly `0x0000`
//! must be transmitted as `0xFFFF` instead, since IPv4 UDP reserves
//! `0x0000` to mean "no checksum was computed" - a real, if rare,
//! ambiguity TCP's own checksum field doesn't share (TCP checksums are
//! mandatory, never optional, so no bit pattern needs to be reserved to
//! mean "absent").
//!
//! Deliberately NOT wired to any connection/session concept (UDP itself
//! has none - it's session-less by design) or device I/O yet - proving
//! these header/checksum primitives are correct in isolation first is
//! the same order `net::tcp`/`net::icmp` themselves already followed.

use crate::net::icmp::pseudo_header_checksum;
use alloc::vec::Vec;

pub const UDP_HEADER_LEN: usize = 8;
pub const IP_PROTOCOL_UDP: u8 = 17;

/// Returns true if `datagram` (header + payload, checksum field as
/// actually received) is internally consistent against the given
/// source/dest IPs. A received checksum field of `0x0000` means "the
/// sender didn't compute one" (RFC 768) - always treated as valid here,
/// the same way a real IP stack must, since there is genuinely nothing
/// to verify in that case. The pseudo-header arithmetic itself lives in
/// `icmp::pseudo_header_checksum` (Fase 119) - shared with `net::tcp`,
/// whose own pseudo-header is byte-for-byte identical bar the protocol
/// number (both RFC 768 and RFC 793 inherited the same pseudo-header
/// idea from the same original IP-checksum design).
pub fn udp_checksum_is_valid(source_ip: [u8; 4], dest_ip: [u8; 4], datagram: &[u8]) -> bool {
    if datagram.len() >= UDP_HEADER_LEN && datagram[6] == 0 && datagram[7] == 0 {
        return true;
    }
    pseudo_header_checksum(source_ip, dest_ip, IP_PROTOCOL_UDP, datagram) == 0
}

/// Builds an 8-byte UDP header (source port, dest port, length, checksum)
/// with a correct RFC 768 pseudo-header checksum already computed and
/// filled in. `source_ip`/`dest_ip` are needed ONLY for that checksum -
/// they are not part of the UDP header itself and don't appear in the
/// returned bytes (the same split `tcp::build_tcp_header` already has).
///
/// If the computed checksum is exactly `0x0000`, it is transmitted as
/// `0xFFFF` instead (RFC 768's own reserved-value rule) - the one
/// genuine wrinkle this function has that `build_tcp_header` doesn't
/// need, since `0x0000` is astronomically unlikely but not impossible
/// for real payload bytes, and silently sending it unmodified would tell
/// the receiver "no checksum was computed" instead of the correct,
/// verifiable one that was.
pub fn build_udp_header(
    source_ip: [u8; 4],
    dest_ip: [u8; 4],
    source_port: u16,
    dest_port: u16,
    payload: &[u8],
) -> [u8; UDP_HEADER_LEN] {
    let mut header = [0u8; UDP_HEADER_LEN];
    header[0..2].copy_from_slice(&source_port.to_be_bytes());
    header[2..4].copy_from_slice(&dest_port.to_be_bytes());
    let udp_len = (UDP_HEADER_LEN + payload.len()) as u16;
    header[4..6].copy_from_slice(&udp_len.to_be_bytes());
    // header[6..8] (checksum) stays zero until computed below.

    let mut datagram = Vec::with_capacity(UDP_HEADER_LEN + payload.len());
    datagram.extend_from_slice(&header);
    datagram.extend_from_slice(payload);
    let checksum = pseudo_header_checksum(source_ip, dest_ip, IP_PROTOCOL_UDP, &datagram);
    let checksum = if checksum == 0 { 0xFFFF } else { checksum };
    header[6..8].copy_from_slice(&checksum.to_be_bytes());
    header
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UdpDatagramInfo {
    pub source_port: u16,
    pub dest_port: u16,
    pub length: u16,
}

/// Parses `datagram`'s header fields, having first verified its
/// pseudo-header checksum against the given source/dest IPs - a caller
/// only ever gets back a result it can trust, covering a too-short
/// buffer and a checksum mismatch as `Err`. `datagram` must be EXACTLY
/// the real UDP portion (header and payload) - the caller is
/// responsible for trimming it to the length the IP header's own Total
/// Length field declares, the same trimming discipline `tcp::parse_tcp_
/// segment`'s own doc already established (Ethernet-level padding up to
/// the 60-byte minimum frame size would otherwise corrupt the checksum
/// sum).
pub fn parse_udp_datagram(
    source_ip: [u8; 4],
    dest_ip: [u8; 4],
    datagram: &[u8],
) -> Result<UdpDatagramInfo, &'static str> {
    if datagram.len() < UDP_HEADER_LEN {
        return Err("UDP datagram shorter than the 8-byte header");
    }
    if !udp_checksum_is_valid(source_ip, dest_ip, datagram) {
        return Err("UDP checksum mismatch - datagram is corrupt or IPs don't match");
    }
    Ok(UdpDatagramInfo {
        source_port: u16::from_be_bytes([datagram[0], datagram[1]]),
        dest_port: u16::from_be_bytes([datagram[2], datagram[3]]),
        length: u16::from_be_bytes([datagram[4], datagram[5]]),
    })
}
