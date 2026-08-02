//! Real DNS message parsing - pure byte-buffer logic with no hardware
//! dependency at all, mirroring `net::icmp`/`net::tcp`/`net::udp`'s own
//! "protocol layer first, wire to real device I/O later" order. Sits ONE
//! layer above `net::udp` (DNS is an application-layer protocol carried
//! BY UDP, not part of UDP itself) - the same relationship `net::tcp`
//! has to `net::icmp`'s own lower-level IPv4 header work.
//!
//! **Deliberately NOT a DNS resolver.** `net::e1000::dns_query_test`
//! (Fase 106) already builds and sends a fixed, hand-built query and
//! confirms a genuine reply arrives (transaction ID match, the QR bit) -
//! this module closes the one piece that test's own doc explicitly left
//! open: actually extracting a real, useful value (an IPv4 address) from
//! the reply's answer section, rather than only confirming a reply
//! exists. It still does NOT implement general name compression
//! (multiple pointers, pointer chains), multiple questions/answers,
//! or any record type besides A - real, separate, substantially larger
//! DNS-client work if ever needed.
//!
//! Fase 110 adds the encoding-side mirror of the same idea: `build_query`
//! builds a real question section for ANY caller-supplied hostname,
//! closing the gap `dns_query_test`'s own Fase 106 doc left open (it
//! only ever sent one fixed, hand-built "A? example.com" query byte
//! array). Still only ever a single-question, single-record-type (A)
//! query - the same "not a resolver" scope this module already commits
//! to on the parsing side.

use alloc::vec::Vec;

pub const DNS_HEADER_LEN: usize = 12;
pub const DNS_TYPE_A: u16 = 1;
pub const DNS_QCLASS_IN: u16 = 1;
/// RFC 1035 section 3.1: a label's length byte is stored in 6 bits (the
/// top two bits are reserved for the `11` compression-pointer marker
/// `parse_first_a_record` itself already checks for), so 63 is the
/// largest length a real label can ever carry.
pub const MAX_LABEL_LEN: usize = 63;

/// Parses the FIRST answer record out of a DNS response message and
/// returns its IPv4 address, if (and only if) that record is a plain A
/// record. `message` is the whole DNS message (12-byte header, followed
/// by the question section, followed by the answer section) - unlike
/// `tcp`/`udp`'s own parse functions, there's no checksum to verify here
/// (DNS itself carries none; UDP's own checksum, already verified by
/// `udp::parse_udp_datagram` before this is ever called, is what
/// actually protects this payload in transit).
///
/// `question_len` is the exact byte length of the question section
/// (QNAME + QTYPE + QCLASS) - the caller already knows this, since it's
/// the same bytes as the query it originally sent (a real server
/// virtually always echoes the question section back completely
/// unchanged), so the answer section is known to start immediately
/// after it without this function needing to walk the question's own
/// QNAME labels itself.
///
/// **Handles exactly one real-world shape, not general DNS**: the
/// answer record's own NAME field is assumed to be a 2-byte compression
/// pointer (RFC 1035 section 4.1.4's `11` top-bit-pair marker) rather
/// than a literal repeated name - what every real DNS server actually
/// sends for an answer that repeats the question's own name, since
/// spelling it out again in full would be pure waste. An uncompressed
/// literal name is reported as an error rather than parsed - genuinely
/// rare in practice for an answer record, and out of this Fase's own
/// deliberately narrow scope (prove ONE real record can be extracted,
/// not build a general-purpose DNS message parser).
pub fn parse_first_a_record(message: &[u8], question_len: usize) -> Result<[u8; 4], &'static str> {
    if message.len() < DNS_HEADER_LEN {
        return Err("DNS message shorter than the 12-byte header");
    }
    let ancount = u16::from_be_bytes([message[6], message[7]]);
    if ancount == 0 {
        return Err("DNS response has zero answer records");
    }

    let answer_start = DNS_HEADER_LEN + question_len;
    if message.len() < answer_start + 2 {
        return Err("DNS message too short to contain an answer record NAME");
    }
    if message[answer_start] & 0xC0 != 0xC0 {
        return Err(
            "answer record NAME is not a compression pointer - uncompressed names aren't parsed",
        );
    }

    // Past the 2-byte NAME pointer: TYPE(2) + CLASS(2) + TTL(4) +
    // RDLENGTH(2) = 10 fixed bytes, then RDATA.
    let fixed_start = answer_start + 2;
    if message.len() < fixed_start + 10 {
        return Err("DNS message too short to contain the answer record's fixed fields");
    }
    let record_type = u16::from_be_bytes([message[fixed_start], message[fixed_start + 1]]);
    let rdlength =
        u16::from_be_bytes([message[fixed_start + 8], message[fixed_start + 9]]) as usize;
    if record_type != DNS_TYPE_A {
        return Err("first answer record is not an A record");
    }
    if rdlength != 4 {
        return Err("A record's own RDLENGTH is not 4");
    }

    let rdata_start = fixed_start + 10;
    if message.len() < rdata_start + 4 {
        return Err("DNS message too short to contain the A record's RDATA");
    }
    let mut ip = [0u8; 4];
    ip.copy_from_slice(&message[rdata_start..rdata_start + 4]);
    Ok(ip)
}

/// Encodes `hostname` (a plain dotted name, e.g. `"example.com"`) into
/// wire-format DNS labels - length-prefixed segments terminated by a
/// zero byte (RFC 1035 section 3.1) - appending directly onto `out`.
/// This is the exact QNAME shape `dns_query_test`'s own Fase 106 bytes
/// were hand-built to (`0x07 "example" 0x03 "com" 0x00`), generalized to
/// accept any hostname rather than that one fixed string.
///
/// Rejects a hostname that would encode to an invalid or misleading
/// label: empty (`""`), a label over 63 bytes (RFC 1035's own hard
/// limit), or an empty label from a leading/trailing/doubled `.` - each
/// would either desync a real resolver's own parsing or silently query a
/// different name than the caller intended, so all three are treated as
/// caller-input-boundary errors rather than something to guess around.
/// ASCII hostnames only - real internationalized domain names need
/// punycode encoding first, genuinely separate, out-of-scope work.
pub fn encode_qname(hostname: &str, out: &mut Vec<u8>) -> Result<(), &'static str> {
    if hostname.is_empty() {
        return Err("hostname is empty");
    }
    for label in hostname.split('.') {
        if label.is_empty() {
            return Err("hostname has an empty label (leading, trailing, or doubled '.')");
        }
        if label.len() > MAX_LABEL_LEN {
            return Err("hostname label exceeds 63 bytes");
        }
        out.push(label.len() as u8);
        out.extend_from_slice(label.as_bytes());
    }
    out.push(0);
    Ok(())
}

/// Builds a complete DNS query message (12-byte header + question
/// section) asking for `hostname`'s A record - the general form of
/// `dns_query_test`'s own Fase 106 fixed `DNS_QUERY` byte array, which
/// this exact function now replaces there. Flags are always `0x0100`
/// (standard query, recursion desired) and QDCOUNT is always 1 - still
/// not a general multi-question query builder, matching this module's
/// own standing "not a resolver" scope on the parsing side.
///
/// `transaction_id` is caller-chosen. Real DNS clients randomize it per
/// query to resist cache-poisoning/spoofing; this kernel does not - the
/// same undefended trust model `arp_resolve` already has toward its own
/// replies, and genuinely out of scope for what this Fase closes (a real
/// hostname parameter, not a hardened resolver).
pub fn build_query(transaction_id: u16, hostname: &str) -> Result<Vec<u8>, &'static str> {
    let mut query = Vec::with_capacity(DNS_HEADER_LEN + hostname.len() + 6);
    query.extend_from_slice(&transaction_id.to_be_bytes());
    query.extend_from_slice(&[0x01, 0x00]); // flags: standard query, recursion desired
    query.extend_from_slice(&[0x00, 0x01]); // QDCOUNT = 1
    query.extend_from_slice(&[0x00, 0x00]); // ANCOUNT = 0
    query.extend_from_slice(&[0x00, 0x00]); // NSCOUNT = 0
    query.extend_from_slice(&[0x00, 0x00]); // ARCOUNT = 0
    encode_qname(hostname, &mut query)?;
    query.extend_from_slice(&DNS_TYPE_A.to_be_bytes());
    query.extend_from_slice(&DNS_QCLASS_IN.to_be_bytes());
    Ok(query)
}
