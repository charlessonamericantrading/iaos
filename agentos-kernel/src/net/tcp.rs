//! Real TCP header construction/parsing + the RFC 793 pseudo-header
//! checksum - pure byte-buffer logic with no hardware dependency at all,
//! mirroring `net::icmp`'s own "protocol layer first, wire to real
//! device I/O later" order (Fase 47 before Fase 48-49's real ARP/ping
//! round trips). Reuses `net::icmp::internet_checksum` directly for the
//! actual summing arithmetic (already proven correct there, verified
//! against RFC 1071 - no reason to re-derive it) - the only genuinely
//! NEW logic here is assembling TCP's own pseudo-header (source IP,
//! dest IP, a zero byte, protocol number 6, and the TCP length) ahead
//! of the real header+payload before handing the combined buffer to
//! that same summing function, per RFC 793 section 3.1.
//!
//! Deliberately NOT wired to any real connection state machine (SYN/
//! SYN-ACK/ACK handshake, sequence tracking, retransmission) or device
//! I/O yet - proving these header/checksum primitives are correct in
//! isolation first is the same order every earlier protocol layer in
//! this kernel has followed (Fase 21's frame allocator, Fase 37/38's
//! VFAT entry-building, Fase 47's own ICMP header/checksum work).

use crate::net::icmp::internet_checksum;
use alloc::vec::Vec;

pub const TCP_HEADER_LEN: usize = 20; // no options
pub const IP_PROTOCOL_TCP: u8 = 6;

pub const TCP_FLAG_FIN: u8 = 0x01;
pub const TCP_FLAG_SYN: u8 = 0x02;
pub const TCP_FLAG_RST: u8 = 0x04;
pub const TCP_FLAG_PSH: u8 = 0x08;
pub const TCP_FLAG_ACK: u8 = 0x10;
pub const TCP_FLAG_URG: u8 = 0x20;

/// RFC 793's pseudo-header checksum: source IP (4) + dest IP (4) + a
/// zero byte + protocol number (6 for TCP) + TCP length (u16), followed
/// by `segment` itself (header, with its own checksum field however the
/// caller left it, plus payload) - all summed via `net::icmp`'s already-
/// proven RFC 1071 algorithm. Used both to COMPUTE a checksum (caller
/// passes a segment with the checksum field still zeroed) and to VERIFY
/// one (caller passes the real, already-checksummed segment - a valid
/// one sums to exactly 0, the same convention `icmp::checksum_is_valid`
/// already established for the analogous ICMP case).
fn tcp_pseudo_checksum(source_ip: [u8; 4], dest_ip: [u8; 4], segment: &[u8]) -> u16 {
    let tcp_len = segment.len() as u16;
    let mut buf = Vec::with_capacity(12 + segment.len());
    buf.extend_from_slice(&source_ip);
    buf.extend_from_slice(&dest_ip);
    buf.push(0);
    buf.push(IP_PROTOCOL_TCP);
    buf.extend_from_slice(&tcp_len.to_be_bytes());
    buf.extend_from_slice(segment);
    internet_checksum(&buf)
}

/// Returns true if `segment` (header + payload, checksum field as
/// actually received) is internally consistent against the given
/// source/dest IPs.
pub fn tcp_checksum_is_valid(source_ip: [u8; 4], dest_ip: [u8; 4], segment: &[u8]) -> bool {
    tcp_pseudo_checksum(source_ip, dest_ip, segment) == 0
}

/// Builds a 20-byte TCP header (no options, `data_offset = 5`) with a
/// correct RFC 793 pseudo-header checksum already computed and filled
/// in. `source_ip`/`dest_ip` are needed ONLY for that checksum - they
/// are not part of the TCP header itself and don't appear in the
/// returned bytes (the same split `icmp::build_ipv4_header` has from
/// `build_icmp_echo`: each protocol layer owns only its own header).
#[allow(clippy::too_many_arguments)]
pub fn build_tcp_header(
    source_ip: [u8; 4],
    dest_ip: [u8; 4],
    source_port: u16,
    dest_port: u16,
    seq_num: u32,
    ack_num: u32,
    flags: u8,
    window: u16,
    payload: &[u8],
) -> [u8; TCP_HEADER_LEN] {
    let mut header = [0u8; TCP_HEADER_LEN];
    header[0..2].copy_from_slice(&source_port.to_be_bytes());
    header[2..4].copy_from_slice(&dest_port.to_be_bytes());
    header[4..8].copy_from_slice(&seq_num.to_be_bytes());
    header[8..12].copy_from_slice(&ack_num.to_be_bytes());
    header[12] = 0x50; // data offset = 5 (20 bytes / 4) in the top 4 bits; reserved bits = 0
    header[13] = flags;
    header[14..16].copy_from_slice(&window.to_be_bytes());
    // header[16..18] (checksum) stays zero until computed below.
    // header[18..20] (urgent pointer) stays zero - URG never set here.

    let mut segment = Vec::with_capacity(TCP_HEADER_LEN + payload.len());
    segment.extend_from_slice(&header);
    segment.extend_from_slice(payload);
    let checksum = tcp_pseudo_checksum(source_ip, dest_ip, &segment);
    header[16..18].copy_from_slice(&checksum.to_be_bytes());
    header
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TcpSegmentInfo {
    pub source_port: u16,
    pub dest_port: u16,
    pub seq_num: u32,
    pub ack_num: u32,
    pub flags: u8,
    pub window: u16,
}

/// Parses `segment`'s header fields, having first verified its
/// pseudo-header checksum against the given source/dest IPs - a caller
/// only ever gets back a result it can trust, covering a too-short
/// buffer, a header with options (unsupported - `data_offset != 5`), and
/// a checksum mismatch all as `Err`. `segment` is the TCP header
/// followed by its payload (if any) - unlike ICMP, the checksum here
/// depends on IPs the segment bytes alone don't carry, so they're
/// required parameters, not optional context.
pub fn parse_tcp_segment(
    source_ip: [u8; 4],
    dest_ip: [u8; 4],
    segment: &[u8],
) -> Result<TcpSegmentInfo, &'static str> {
    if segment.len() < TCP_HEADER_LEN {
        return Err("TCP segment shorter than the 20-byte header");
    }
    let data_offset_words = segment[12] >> 4;
    if data_offset_words != 5 {
        return Err("TCP header has options (data offset != 5) - unsupported");
    }
    if !tcp_checksum_is_valid(source_ip, dest_ip, segment) {
        return Err("TCP checksum mismatch - segment is corrupt or IPs don't match");
    }
    Ok(TcpSegmentInfo {
        source_port: u16::from_be_bytes([segment[0], segment[1]]),
        dest_port: u16::from_be_bytes([segment[2], segment[3]]),
        seq_num: u32::from_be_bytes([segment[4], segment[5], segment[6], segment[7]]),
        ack_num: u32::from_be_bytes([segment[8], segment[9], segment[10], segment[11]]),
        flags: segment[13],
        window: u16::from_be_bytes([segment[14], segment[15]]),
    })
}
