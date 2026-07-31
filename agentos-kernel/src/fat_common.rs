//! Bits shared between `fat32.rs` and `fat12.rs`: the 32-byte directory
//! entry format is identical across FAT12/16/32 - only the FAT table
//! encoding and where the root directory lives differ between them.

use alloc::string::String;
use alloc::vec::Vec;

pub struct DirEntry {
    pub name: String,
    pub is_dir: bool,
    pub size: u32,
    pub start_cluster: u32,
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

/// Parses the 32-byte directory entries in one already-read sector,
/// appending real ones to `entries`. Returns `true` if the "no more
/// entries anywhere in this directory" marker (a first byte of `0x00`)
/// was seen, telling the caller to stop reading further sectors/clusters.
pub fn parse_dir_sector(sector: &[u8; 512], entries: &mut Vec<DirEntry>) -> bool {
    for raw in sector.as_chunks::<32>().0 {
        if raw[0] == 0x00 {
            return true;
        }
        if raw[0] == 0xE5 || raw[11] == 0x0F || raw[11] & 0x08 != 0 {
            continue; // deleted / long-filename (VFAT) / volume label
        }

        let name = format_short_name(&raw[0..8], &raw[8..11]);
        let is_dir = raw[11] & 0x10 != 0;
        let hi = u16::from_le_bytes([raw[20], raw[21]]) as u32;
        let lo = u16::from_le_bytes([raw[26], raw[27]]) as u32;
        let size = u32::from_le_bytes([raw[28], raw[29], raw[30], raw[31]]);

        entries.push(DirEntry {
            name,
            is_dir,
            size,
            start_cluster: (hi << 16) | lo,
        });
    }
    false
}
