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
//! exists. It still does NOT actually RESOLVE any compression pointer
//! (bare or trailing a label sequence) to find out what name it points
//! at - only ever how many bytes it occupies, real but strictly
//! narrower than general name decompression - nor does it handle
//! multiple QUESTIONS, or decode any record type besides A/AAAA - real,
//! separate, substantially larger DNS-client work if ever needed.
//!
//! Fase 115: `parse_first_a_record` now genuinely searches THROUGH
//! multiple answer records (skipping any non-A ones via their own
//! declared RDLENGTH) instead of only ever inspecting whichever record
//! happens to come first - closing a real gap this kernel's own live CI
//! runs had already exposed without ever being examined: every real DNS
//! reply for "example.com" observed on CI since Fase 106 reports
//! `answer_count=2`, and this function had never actually been proven
//! correct for anything beyond a single answer.
//!
//! Fase 117: an answer record's own NAME field no longer has to be a
//! compression pointer - `name_field_len` also walks a fully spelled-out
//! (uncompressed) sequence of labels, the same wire shape `encode_qname`
//! itself builds. Real servers overwhelmingly compress an answer name
//! that repeats the question, but an uncompressed one is perfectly
//! legal RFC 1035 wire format, previously hard-rejected outright.
//!
//! Fase 120: a label sequence ending in a compression pointer instead
//! of a zero byte ("partial compression", RFC 1035 4.1.4) is now
//! accepted too, the same way a bare pointer already was - counted as a
//! 2-byte terminator, never followed. Since `name_field_len` never
//! resolves what a pointer points at (only how many bytes the NAME
//! field itself occupies), there was no chain-following or
//! loop-prevention work needed to close this gap, unlike a real
//! name-decompression routine would need.
//!
//! Fase 124: `parse_first_aaaa_record` decodes AAAA (IPv6, RFC 3596)
//! records the same way `parse_first_a_record` already decodes A -
//! genuinely additive, not a generalization of what "not a resolver"
//! means, since AAAA's RDATA is a fixed 16-byte address exactly like
//! A's fixed 4-byte one (unlike CNAME, whose RDATA is itself a NAME
//! that could be compressed - decoding THAT would need real
//! chain-following/loop-prevention logic this module still doesn't
//! have, a genuinely different and larger problem). The record-walking
//! loop itself (skip non-matching records via their own RDLENGTH) was
//! shared between both functions via `find_first_record_rdata`, since
//! it was byte-for-byte identical except for which TYPE to look for and
//! how many RDATA bytes the caller copies out - the same real, not
//! speculative, duplication this project's own later Fases (97, 111,
//! 119, 122) have each found and unified elsewhere.
//!
//! Fase 110 adds the encoding-side mirror of the same idea: `build_query`
//! builds a real question section for ANY caller-supplied hostname,
//! closing the gap `dns_query_test`'s own Fase 106 doc left open (it
//! only ever sent one fixed, hand-built "A? example.com" query byte
//! array). Fase 126 further lets the caller choose which QTYPE to ask
//! for (`DNS_TYPE_A` or `DNS_TYPE_AAAA`), rather than always hardcoding
//! A - still only ever a single-QUESTION query, though, the same "not a
//! resolver" scope this module already commits to on the parsing side.

use alloc::vec::Vec;

pub const DNS_HEADER_LEN: usize = 12;
pub const DNS_TYPE_A: u16 = 1;
/// RFC 3596: the AAAA record type (a host's IPv6 address, the exact
/// IPv6 counterpart to `DNS_TYPE_A`'s IPv4 one).
pub const DNS_TYPE_AAAA: u16 = 28;
pub const DNS_QCLASS_IN: u16 = 1;
/// RFC 1035 section 3.1: a label's length byte is stored in 6 bits (the
/// top two bits are reserved for the `11` compression-pointer marker
/// `parse_first_a_record` itself already checks for), so 63 is the
/// largest length a real label can ever carry.
pub const MAX_LABEL_LEN: usize = 63;

/// Searches THROUGH a DNS response message's own answer records (Fase
/// 115: up to `ancount` of them, not just the first) and returns the
/// first one's IPv4 address that is a plain A record - real DNS replies
/// can legitimately place a CNAME (or other type) before the actual A
/// record a caller wants, and this kernel's own real CI runs have
/// observed `answer_count=2` for "example.com" since Fase 106, so
/// genuinely walking past a non-A record rather than just assuming the
/// first one already is one is real, necessary correctness, not
/// speculative generality. `message` is the whole DNS message (12-byte
/// header, followed by the question section, followed by the answer
/// section) - unlike `tcp`/`udp`'s own parse functions, there's no
/// checksum to verify here (DNS itself carries none; UDP's own
/// checksum, already verified by `udp::parse_udp_datagram` before this
/// is ever called, is what actually protects this payload in transit).
///
/// `question_len` is the exact byte length of the question section
/// (QNAME + QTYPE + QCLASS) - the caller already knows this, since it's
/// the same bytes as the query it originally sent (a real server
/// virtually always echoes the question section back completely
/// unchanged), so the answer section is known to start immediately
/// after it without this function needing to walk the question's own
/// QNAME labels itself.
///
/// **Handles every real-world shape of a record's own NAME field, not
/// general DNS**: a 2-byte compression pointer (RFC 1035 section
/// 4.1.4's `11` top-bit-pair marker) - what most real DNS servers send
/// for an answer that repeats the question's own name - a fully
/// spelled-out sequence of length-prefixed labels terminated by a zero
/// byte (Fase 117, the same wire shape `encode_qname` itself builds),
/// and a label sequence that ends in a compression pointer instead of a
/// zero byte (Fase 120, RFC 1035 4.1.4's "partial compression" - one or
/// more real labels, then a pointer to where the name continues). None
/// of these NAME shapes is ever actually resolved to find out what the
/// name says - this function only ever needs to know how many bytes
/// each one occupies, to find where the record's own fixed fields
/// start, so a compression pointer (bare or trailing a label sequence)
/// is simply counted as 2 bytes and never followed. See
/// `name_field_len` for exactly how each shape's length is computed. A
/// non-A record is skipped using its OWN declared RDLENGTH to find the
/// next record - real, necessary work now that more than one record is
/// actually being looked at, not previously needed when only the first
/// one was ever inspected.
pub fn parse_first_a_record(message: &[u8], question_len: usize) -> Result<[u8; 4], &'static str> {
    let rdata = find_first_record_rdata(message, question_len, DNS_TYPE_A)
        .map_err(|_| "no A record found among this reply's own answer records")?;
    if rdata.len() != 4 {
        return Err("A record's own RDLENGTH is not 4");
    }
    let mut ip = [0u8; 4];
    ip.copy_from_slice(rdata);
    Ok(ip)
}

/// Searches THROUGH a DNS response's own answer records for the first
/// AAAA (RFC 3596, IPv6) record, the exact same way `parse_first_a_record`
/// searches for the first A one - see that function's own doc for the
/// NAME-field/RDLENGTH-skip details shared by both (Fase 124: both call
/// [`find_first_record_rdata`] underneath).
pub fn parse_first_aaaa_record(
    message: &[u8],
    question_len: usize,
) -> Result<[u8; 16], &'static str> {
    let rdata = find_first_record_rdata(message, question_len, DNS_TYPE_AAAA)
        .map_err(|_| "no AAAA record found among this reply's own answer records")?;
    if rdata.len() != 16 {
        return Err("AAAA record's own RDLENGTH is not 16");
    }
    let mut ip = [0u8; 16];
    ip.copy_from_slice(rdata);
    Ok(ip)
}

/// The record-walking loop shared by `parse_first_a_record` and
/// `parse_first_aaaa_record` (Fase 124): searches THROUGH a DNS
/// response's own answer records (up to `ancount` of them) and returns
/// the RDATA slice of the first one whose TYPE matches `record_type`,
/// skipping any non-matching record using its own declared RDLENGTH -
/// real, necessary work now that more than one record (and more than
/// one TYPE) is actually being looked for, not previously needed when
/// only a single hardcoded TYPE was ever inspected. Does NOT validate
/// the matching record's own RDLENGTH against what its TYPE actually
/// requires - that's each caller's own job (a 4-byte check for A, a
/// 16-byte one for AAAA), since this function has no opinion on what a
/// "correct" length is for a TYPE it doesn't itself know the meaning of.
fn find_first_record_rdata(
    message: &[u8],
    question_len: usize,
    record_type: u16,
) -> Result<&[u8], &'static str> {
    if message.len() < DNS_HEADER_LEN {
        return Err("DNS message shorter than the 12-byte header");
    }
    let ancount = u16::from_be_bytes([message[6], message[7]]);
    if ancount == 0 {
        return Err("DNS response has zero answer records");
    }

    let mut offset = DNS_HEADER_LEN + question_len;
    for _ in 0..ancount {
        let name_len = name_field_len(message, offset)?;

        // Past the NAME field: TYPE(2) + CLASS(2) + TTL(4) + RDLENGTH(2)
        // = 10 fixed bytes, then RDATA.
        let fixed_start = offset + name_len;
        if message.len() < fixed_start + 10 {
            return Err("DNS message too short to contain the answer record's fixed fields");
        }
        let this_type = u16::from_be_bytes([message[fixed_start], message[fixed_start + 1]]);
        let rdlength =
            u16::from_be_bytes([message[fixed_start + 8], message[fixed_start + 9]]) as usize;
        let rdata_start = fixed_start + 10;
        if message.len() < rdata_start + rdlength {
            return Err("DNS message too short to contain the answer record's own RDATA");
        }

        if this_type == record_type {
            return Ok(&message[rdata_start..rdata_start + rdlength]);
        }

        // Not the requested type - skip past it (using its own declared
        // RDLENGTH) and check the next answer.
        offset = rdata_start + rdlength;
    }
    Err("no record of the requested type found among this reply's own answer records")
}

/// Returns the byte length of the NAME field starting at `offset` -
/// either a 2-byte compression pointer (the common case, unchanged
/// since Fase 107), or (Fase 117) a fully spelled-out sequence of
/// length-prefixed labels terminated by a zero byte, the mirror image
/// of what `encode_qname` itself builds. Does NOT follow a compression
/// pointer to verify what it points to - `parse_first_a_record` never
/// needed that (the caller already knows the question section it sent,
/// and a real server virtually always points back at exactly that), so
/// this only needs to know how many bytes the NAME field itself
/// occupies, not what it names.
///
/// A label sequence that ends in a compression pointer instead of a
/// zero byte ("partial compression", RFC 1035 4.1.4 - one or more real
/// labels, then a pointer to where the name continues) is accepted the
/// same way a bare pointer is: as a 2-byte terminator, counted and
/// returned, never FOLLOWED (Fase 120 - this function has never needed
/// to resolve what a NAME says, only how many bytes it occupies, so
/// there is no pointer chain to walk and no loop-prevention invariant
/// needed here, unlike a real name-decompression routine would need).
/// Also rejects a label over 63 bytes (`MAX_LABEL_LEN`, the same limit
/// `encode_qname` enforces on the way in) and a label sequence that
/// runs past the end of the message - both real malformed-input cases,
/// not something to guess around.
fn name_field_len(message: &[u8], offset: usize) -> Result<usize, &'static str> {
    if offset >= message.len() {
        return Err("answer record NAME starts past the end of the message");
    }
    if message[offset] & 0xC0 == 0xC0 {
        if offset + 1 >= message.len() {
            return Err("answer record NAME's own compression pointer is missing its second byte");
        }
        return Ok(2);
    }

    let mut pos = offset;
    loop {
        if pos >= message.len() {
            return Err("answer record NAME runs past the end of the message");
        }
        let label_len = message[pos] as usize;
        if label_len & 0xC0 == 0xC0 {
            if pos + 1 >= message.len() {
                return Err(
                    "answer record NAME's own compression pointer is missing its second byte",
                );
            }
            pos += 2;
            break;
        }
        if label_len == 0 {
            pos += 1;
            break;
        }
        if label_len > MAX_LABEL_LEN {
            return Err("answer record NAME has a label over 63 bytes");
        }
        pos += 1 + label_len;
    }
    Ok(pos - offset)
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
/// section) asking for `hostname`'s `qtype` record - the general form of
/// `dns_query_test`'s own Fase 106 fixed `DNS_QUERY` byte array, which
/// this exact function now replaces there. Flags are always `0x0100`
/// (standard query, recursion desired) and QDCOUNT is always 1 - still
/// not a general multi-question query builder, matching this module's
/// own standing "not a resolver" scope on the parsing side.
///
/// Fase 126: `qtype` is now caller-chosen (`DNS_TYPE_A` or
/// `DNS_TYPE_AAAA`) rather than always `DNS_TYPE_A` - otherwise Fase
/// 124's own `parse_first_aaaa_record` could never actually be reached
/// by a real query this kernel sends, only by a synthetic self-test
/// fixture. Live wiring of an AAAA-requesting `dns_query_test`/`ping`
/// path remains real, separate follow-on work if ever wanted - this
/// Fase only makes the QTYPE choice possible, matching the same
/// encode-then-generalize order Fase 107 (decode A) then Fase 110
/// (generalize the encoder) already used.
///
/// `transaction_id` is caller-chosen. Real DNS clients randomize it per
/// query to resist cache-poisoning/spoofing; this kernel does not - the
/// same undefended trust model `arp_resolve` already has toward its own
/// replies, and genuinely out of scope for what this Fase closes (a real
/// hostname parameter, not a hardened resolver).
pub fn build_query(
    transaction_id: u16,
    hostname: &str,
    qtype: u16,
) -> Result<Vec<u8>, &'static str> {
    let mut query = Vec::with_capacity(DNS_HEADER_LEN + hostname.len() + 6);
    query.extend_from_slice(&transaction_id.to_be_bytes());
    query.extend_from_slice(&[0x01, 0x00]); // flags: standard query, recursion desired
    query.extend_from_slice(&[0x00, 0x01]); // QDCOUNT = 1
    query.extend_from_slice(&[0x00, 0x00]); // ANCOUNT = 0
    query.extend_from_slice(&[0x00, 0x00]); // NSCOUNT = 0
    query.extend_from_slice(&[0x00, 0x00]); // ARCOUNT = 0
    encode_qname(hostname, &mut query)?;
    query.extend_from_slice(&qtype.to_be_bytes());
    query.extend_from_slice(&DNS_QCLASS_IN.to_be_bytes());
    Ok(query)
}
