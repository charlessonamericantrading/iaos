//! Bits shared between `fat32.rs` and `fat12.rs`: the 32-byte directory
//! entry format is identical across FAT12/16/32 - only the FAT table
//! encoding and where the root directory lives differ between them.
//!
//! This includes the read side of VFAT long file names: a long name is
//! spread across one or more extra 32-byte entries (attribute `0x0F`)
//! immediately preceding the short entry they describe, which every non-
//! VFAT-aware FAT reader already skips harmlessly (see `parse_dir_sector`
//! below - it always has, even before this kernel could write one
//! itself). `fat12.rs`'s `build_lfn_entries` writes up to 15 chained long
//! entries (195 characters - its own single-sector slot-run limit), and
//! this read side reconstructs a long name of *any* chunk count, so a
//! name written by a real, more complete VFAT implementation (no such
//! cap) still displays correctly here too.

use alloc::string::String;
use alloc::vec::Vec;

pub struct DirEntry {
    pub name: String,
    pub is_dir: bool,
    pub size: u32,
    pub start_cluster: u32,
    /// The on-disk "last write" time/date fields (offsets 22-25 of the raw
    /// 32-byte entry), packed exactly as FAT itself stores them - see
    /// `fat12.rs::to_fat_datetime` for the encoding. `fat12.rs::build_dir_entry`
    /// stamps a real CMOS-RTC-derived value here (and, identically, into
    /// creation/access) at create time; until now nothing ever read it back.
    pub fat_time: u16,
    pub fat_date: u16,
}

/// The standard VFAT short-name checksum (see the Linux kernel's own
/// `Documentation/filesystems/vfat.txt`, verified against that source
/// directly rather than reimplemented from memory - this exact kind of
/// bit-twiddling is easy to get subtly wrong): rotate the running sum
/// right by one bit, then add the next byte, wrapping - applied over all
/// 11 bytes of the padded 8.3 short name. Every long-name entry
/// describing a given short entry stores this same value, letting a
/// reader detect (and safely ignore) a long name left orphaned by a
/// non-VFAT-aware tool that edited the short entry without knowing to
/// update or remove the long entries pointing to it.
pub fn lfn_checksum(short_name: &[u8]) -> u8 {
    short_name
        .iter()
        .fold(0u8, |sum, &b| sum.rotate_right(1).wrapping_add(b))
}

/// Carries an in-progress VFAT long-name reconstruction across
/// successive raw directory entries scanned in on-disk order - a long
/// entry and the short entry it describes are always adjacent, but
/// "adjacent" doesn't mean "in the same sector": a long entry landing in
/// the last slot of one sector is followed by its short entry in the
/// very next one, so this can't be purely per-sector state. Both
/// `parse_dir_sector` (building a listing) and `fat12.rs`'s
/// `find_entry_location_in` (searching for one specific name, by either
/// its long or short form) drive the same instance through every entry
/// of a directory via `push_long_entry`/`take_verified`/`reset`, so the
/// two never risk reconstructing a name differently between "what `ls`
/// shows" and "what a lookup by that name finds".
#[derive(Default)]
pub struct LfnState {
    chunks: Vec<[u16; 13]>,
    checksum: u8,
}

impl LfnState {
    /// Feeds one VFAT long-name entry's raw bytes into the accumulator.
    /// Bit `0x40` on the sequence-number byte marks the entry holding
    /// the END of the name - the FIRST one encountered in on-disk order
    /// (highest sequence number) - so seeing it again means a fresh
    /// sequence is starting, discarding any unfinished previous one
    /// (e.g. left behind by a botched delete).
    pub fn push_long_entry(&mut self, raw: &[u8; 32]) {
        let mut chars = [0u16; 13];
        for i in 0..5 {
            chars[i] = u16::from_le_bytes([raw[1 + i * 2], raw[2 + i * 2]]);
        }
        for i in 0..6 {
            chars[5 + i] = u16::from_le_bytes([raw[14 + i * 2], raw[15 + i * 2]]);
        }
        for i in 0..2 {
            chars[11 + i] = u16::from_le_bytes([raw[28 + i * 2], raw[29 + i * 2]]);
        }
        if raw[0] & 0x40 != 0 {
            self.chunks.clear();
            self.checksum = raw[13];
        }
        self.chunks.push(chars);
    }

    /// Called when a short entry is reached: reconstructs and returns
    /// the pending long name if any is pending AND its checksum
    /// verifies against `short_name_bytes` (that entry's raw 11-byte
    /// short-name field) - guarding against a mismatched/orphaned
    /// sequence, see `lfn_checksum`'s doc. Clears the pending state
    /// either way, matched or not: a short entry always ends whatever
    /// sequence preceded it.
    pub fn take_verified(&mut self, short_name_bytes: &[u8]) -> Option<String> {
        let result = if !self.chunks.is_empty() && self.checksum == lfn_checksum(short_name_bytes) {
            let mut s = String::with_capacity(self.chunks.len() * 13);
            for chunk in self.chunks.iter().rev() {
                decode_lfn_chunk(chunk, &mut s);
            }
            Some(s)
        } else {
            None
        };
        self.chunks.clear();
        result
    }

    /// Discards any pending sequence - call on a deleted entry or volume
    /// label, neither of which a real long-name sequence ever precedes.
    pub fn reset(&mut self) {
        self.chunks.clear();
    }
}

/// Decodes one long-name entry's 13 UTF-16LE-coded characters, stopping
/// at the first `0x0000` terminator (a name chunk shorter than 13
/// characters) or after all 13 (an exact-multiple-of-13-length name,
/// which per spec omits the terminator entirely in its final chunk).
/// ASCII-only: this kernel only ever *writes* long names built from
/// plain ASCII widened 1:1 into UTF-16 (see `fat12.rs`'s
/// `build_lfn_entries`), so narrowing back down with `as u8` is exact and
/// lossless for every long name this kernel can itself produce; a
/// genuinely non-ASCII long name (impossible for this kernel to have
/// written, but a real VFAT tool could produce one) would come back
/// mangled rather than misinterpreted as something else.
fn decode_lfn_chunk(chars: &[u16; 13], out: &mut String) {
    for &c in chars {
        if c == 0x0000 {
            return;
        }
        out.push((c as u8) as char);
    }
}

/// Reconstructs a "NAME.EXT" string from the raw 8-byte name + 3-byte
/// extension fields of a short directory entry (space-padded on disk).
pub fn format_short_name(name: &[u8], ext: &[u8]) -> String {
    let mut s = String::new();
    for &b in name {
        if b == b' ' {
            break;
        }
        s.push(b as char);
    }
    let ext_len = ext.iter().take_while(|&&b| b != b' ').count();
    if ext_len > 0 {
        s.push('.');
        for &b in &ext[..ext_len] {
            s.push(b as char);
        }
    }
    s
}

/// Checks one raw 32-byte short entry against `target_name`, matching
/// either its VFAT long name (if `lfn` has one pending and checksum-
/// verified) OR its own raw short (8.3) name - matching EITHER form is
/// enough, since a file that has both a long and a short name must be
/// look-up-able by either one. Returns the resolved `DirEntry` (using
/// the long name if present, else the short one - the same precedence
/// `parse_dir_sector` already uses when just listing) on a match, `None`
/// otherwise. Caller still handles `0x00`/`0xE5`/volume-label/`0x0F`
/// entries itself (a match check only makes sense once a real short
/// entry is reached) and must drive the same `lfn` instance through
/// every entry in on-disk order, exactly like `parse_dir_sector`.
///
/// Fase 139: extracted from `fat12.rs`'s own `find_entry_location_in`,
/// which already checked both forms this way - `fat32.rs::read_file`
/// did not, and so could never find a file by its short name once it
/// also had a long one (it went through `list_directory`/
/// `parse_dir_sector` instead, which - like any plain listing - only
/// ever keeps ONE resolved name per entry, discarding whichever form
/// lost). The two callers now share this exact matching logic instead
/// of `fat32.rs` needing its own hand-duplicated copy.
pub fn short_entry_if_matches(
    raw: &[u8; 32],
    lfn: &mut LfnState,
    target_name: &str,
) -> Option<DirEntry> {
    let long_name = lfn.take_verified(&raw[0..11]);
    let short_name = format_short_name(&raw[0..8], &raw[8..11]);
    let matches = long_name
        .as_deref()
        .is_some_and(|n| n.eq_ignore_ascii_case(target_name))
        || short_name.eq_ignore_ascii_case(target_name);
    if !matches {
        return None;
    }
    let is_dir = raw[11] & 0x10 != 0;
    let hi = u16::from_le_bytes([raw[20], raw[21]]) as u32;
    let lo = u16::from_le_bytes([raw[26], raw[27]]) as u32;
    let size = u32::from_le_bytes([raw[28], raw[29], raw[30], raw[31]]);
    let fat_time = u16::from_le_bytes([raw[22], raw[23]]);
    let fat_date = u16::from_le_bytes([raw[24], raw[25]]);
    Some(DirEntry {
        name: long_name.unwrap_or(short_name),
        is_dir,
        size,
        start_cluster: (hi << 16) | lo,
        fat_time,
        fat_date,
    })
}

/// Parses the 32-byte directory entries in one already-read sector,
/// appending real ones to `entries`. Returns `true` if the "no more
/// entries anywhere in this directory" marker (a first byte of `0x00`)
/// was seen, telling the caller to stop reading further sectors/clusters.
///
/// `lfn` carries any in-progress VFAT long-name reconstruction across
/// calls - pass the same instance for every sector of the same
/// directory, in order (see `LfnState`'s doc for why).
pub fn parse_dir_sector(
    sector: &[u8; 512],
    entries: &mut Vec<DirEntry>,
    lfn: &mut LfnState,
) -> bool {
    for raw in sector.as_chunks::<32>().0 {
        if raw[0] == 0x00 {
            return true;
        }
        if raw[0] == 0xE5 {
            lfn.reset();
            continue;
        }
        if raw[11] == 0x0F {
            lfn.push_long_entry(raw);
            continue;
        }
        if raw[11] & 0x08 != 0 {
            lfn.reset(); // volume label
            continue;
        }

        let name = lfn
            .take_verified(&raw[0..11])
            .unwrap_or_else(|| format_short_name(&raw[0..8], &raw[8..11]));

        let is_dir = raw[11] & 0x10 != 0;
        let hi = u16::from_le_bytes([raw[20], raw[21]]) as u32;
        let lo = u16::from_le_bytes([raw[26], raw[27]]) as u32;
        let size = u32::from_le_bytes([raw[28], raw[29], raw[30], raw[31]]);
        let fat_time = u16::from_le_bytes([raw[22], raw[23]]);
        let fat_date = u16::from_le_bytes([raw[24], raw[25]]);

        entries.push(DirEntry {
            name,
            is_dir,
            size,
            start_cluster: (hi << 16) | lo,
            fat_time,
            fat_date,
        });
    }
    false
}
