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

pub const DNS_HEADER_LEN: usize = 12;
pub const DNS_TYPE_A: u16 = 1;

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
