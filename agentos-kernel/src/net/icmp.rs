//! Real IPv4 + ICMP echo (ping) protocol construction and parsing - pure
//! byte-buffer logic with no hardware dependency at all. `net::tcpip`/
//! `net::virtio_net` remain the pre-existing *simulated* IPv4 path (fake
//! MAC, no real checksum, a `transmit_packet` that just counts - see their
//! own doc comments); this module is the real one, verified against RFC
//! 791 (IPv4), RFC 792 (ICMP), and RFC 1071 (checksum) rather than relied
//! on from memory alone - the same discipline `net::e1000` used for its
//! own register-level details, given how easily a wrong checksum silently
//! drops a packet with no visible error at all (the receiving stack just
//! never replies, an even harder symptom to root-cause than a crash).
//!
//! Deliberately NOT wired to `net::e1000`'s TX/RX yet - that needs a real
//! destination MAC address (ARP resolution, which doesn't exist yet
//! either) and touches the already fully-proven, CI-asserted TX/RX code
//! paths from Fase 44-46. Proving these header/checksum primitives are
//! correct in isolation first, before anything depends on them, is the
//! same order Fase 37/38's VFAT entry-building or Fase 21's frame
//! allocator both followed.

use alloc::vec::Vec;

pub const ETHERNET_HEADER_LEN: usize = 14;
pub const IPV4_HEADER_LEN: usize = 20;
pub const ICMP_ECHO_HEADER_LEN: usize = 8;

pub const ETHERTYPE_IPV4: u16 = 0x0800;
pub const IP_PROTOCOL_ICMP: u8 = 1;

pub const ICMP_TYPE_ECHO_REPLY: u8 = 0;
pub const ICMP_TYPE_ECHO_REQUEST: u8 = 8;

/// RFC 1071's Internet checksum: the 16-bit one's complement of the one's
/// complement sum of `data` treated as big-endian 16-bit words. A trailing
/// odd byte is padded with an implicit zero *low* byte - i.e. treated as a
/// word's high byte alone (`byte << 8`) - matching RFC 792's "padded with
/// one octet of zeros" for an odd-length ICMP message. Used identically
/// for both the IPv4 header checksum and the ICMP message checksum below -
/// same algorithm, just over different byte ranges.
///
/// This returns the value meant to be *embedded* in a checksum field
/// (`!sum`) - to verify an already-checksummed buffer instead, use
/// [`checksum_is_valid`], not a direct comparison against this function's
/// result (see that function's doc for why `0xFFFF` is the wrong constant
/// to compare against here, despite how it reads in RFC 1071 itself).
pub fn internet_checksum(data: &[u8]) -> u16 {
    let mut sum: u32 = 0;
    let (chunks, remainder) = data.as_chunks::<2>();
    for &word in chunks {
        sum += u16::from_be_bytes(word) as u32;
    }
    if let [last] = *remainder {
        sum += (last as u32) << 8;
    }
    while sum >> 16 != 0 {
        sum = (sum & 0xFFFF) + (sum >> 16);
    }
    !(sum as u16)
}

/// Returns true if `data` - a buffer that already includes its own
/// checksum field - is internally consistent (correctly checksummed, not
/// corrupted since).
///
/// This is deliberately its own function rather than a comparison callers
/// write themselves: RFC 1071 phrases its verification check as "the
/// one's complement **sum** is all-1-bits (`0xFFFF`)" - but that's the
/// *pre-complement* sum. [`internet_checksum`] returns `!sum` (the value
/// meant to be embedded in a checksum field), so re-running it over a
/// validly-checksummed buffer returns `!0xFFFF = 0x0000`, not `0xFFFF` -
/// confirmed the hard way (a real, first-run bug in this Fase's own
/// self-test, caught by an independent hand-computed example rather than
/// assumed correct from a round-trip test that would have agreed with
/// itself either way). Comparing against `0xFFFF` here would silently
/// reject every genuinely valid buffer.
pub fn checksum_is_valid(data: &[u8]) -> bool {
    internet_checksum(data) == 0
}

/// Builds a 14-byte Ethernet II header.
pub fn build_ethernet_header(
    dest_mac: [u8; 6],
    src_mac: [u8; 6],
    ethertype: u16,
) -> [u8; ETHERNET_HEADER_LEN] {
    let mut header = [0u8; ETHERNET_HEADER_LEN];
    header[0..6].copy_from_slice(&dest_mac);
    header[6..12].copy_from_slice(&src_mac);
    header[12..14].copy_from_slice(&ethertype.to_be_bytes());
    header
}

/// Builds a 20-byte IPv4 header (no options, `version_ihl = 0x45`) with a
/// correct header checksum already computed and filled in. `payload_len`
/// is only used to fill in the Total Length field - this returns just the
/// header itself, not the header concatenated with the payload, so a
/// caller can build/reuse each piece (Ethernet/IPv4/ICMP) independently.
pub fn build_ipv4_header(
    identification: u16,
    ttl: u8,
    protocol: u8,
    source: [u8; 4],
    destination: [u8; 4],
    payload_len: usize,
) -> [u8; IPV4_HEADER_LEN] {
    let mut header = [0u8; IPV4_HEADER_LEN];
    header[0] = 0x45; // version 4, IHL 5 (20 bytes, no options)
    header[1] = 0x00; // DSCP/ECN: none
    let total_len = (IPV4_HEADER_LEN + payload_len) as u16;
    header[2..4].copy_from_slice(&total_len.to_be_bytes());
    header[4..6].copy_from_slice(&identification.to_be_bytes());
    header[6..8].copy_from_slice(&0u16.to_be_bytes()); // flags/fragment offset: none
    header[8] = ttl;
    header[9] = protocol;
    // header[10..12] (checksum) stays zero for the sum below, per RFC 1071.
    header[12..16].copy_from_slice(&source);
    header[16..20].copy_from_slice(&destination);
    let checksum = internet_checksum(&header);
    header[10..12].copy_from_slice(&checksum.to_be_bytes());
    header
}

/// Builds a full ICMP echo request (`is_reply = false`) or reply
/// (`is_reply = true`) message - the 8-byte echo header followed by
/// `data`, with the whole-message checksum (RFC 792: header *and* data
/// together) already computed and filled in.
pub fn build_icmp_echo(is_reply: bool, identifier: u16, sequence: u16, data: &[u8]) -> Vec<u8> {
    let mut message = Vec::with_capacity(ICMP_ECHO_HEADER_LEN + data.len());
    message.push(if is_reply {
        ICMP_TYPE_ECHO_REPLY
    } else {
        ICMP_TYPE_ECHO_REQUEST
    });
    message.push(0); // code: always 0 for echo request/reply
    message.extend_from_slice(&0u16.to_be_bytes()); // checksum placeholder
    message.extend_from_slice(&identifier.to_be_bytes());
    message.extend_from_slice(&sequence.to_be_bytes());
    message.extend_from_slice(data);
    let checksum = internet_checksum(&message);
    message[2..4].copy_from_slice(&checksum.to_be_bytes());
    message
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IcmpEchoInfo {
    pub is_reply: bool,
    pub identifier: u16,
    pub sequence: u16,
}

/// Parses `message`'s echo-specific fields, having first verified its
/// checksum - a caller only ever gets back a result it can trust,
/// covering a too-short buffer, a checksum mismatch (corruption, or not
/// really an ICMP message at all), and a non-echo type all as `Err`. The
/// payload itself isn't copied out here: a caller already holds `message`
/// and can slice `&message[ICMP_ECHO_HEADER_LEN..]` for it directly.
pub fn parse_icmp_echo(message: &[u8]) -> Result<IcmpEchoInfo, &'static str> {
    if message.len() < ICMP_ECHO_HEADER_LEN {
        return Err("ICMP message shorter than the 8-byte echo header");
    }
    if !checksum_is_valid(message) {
        return Err("ICMP checksum mismatch - message is corrupt or not a valid echo message");
    }
    let is_reply = match message[0] {
        ICMP_TYPE_ECHO_REPLY => true,
        ICMP_TYPE_ECHO_REQUEST => false,
        _ => return Err("not an ICMP echo request/reply (unexpected type)"),
    };
    Ok(IcmpEchoInfo {
        is_reply,
        identifier: u16::from_be_bytes([message[4], message[5]]),
        sequence: u16::from_be_bytes([message[6], message[7]]),
    })
}
