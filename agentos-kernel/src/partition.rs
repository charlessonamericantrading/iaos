//! MBR (Master Boot Record) partition table parsing - the classic,
//! universally-supported way a disk describes its partitions, still
//! present even on modern GPT disks (as a single "protective" entry).
//! Pure data interpretation of a sector already read via
//! `ata::read_sector` - no new hardware I/O of its own.

#[derive(Debug, Clone, Copy)]
pub struct PartitionEntry {
    pub bootable: bool,
    pub partition_type: u8,
    pub start_lba: u32,
    pub sector_count: u32,
}

pub fn partition_type_name(t: u8) -> &'static str {
    match t {
        0x00 => "empty",
        0x01 => "FAT12",
        0x04 | 0x06 | 0x0E => "FAT16",
        0x0B | 0x0C => "FAT32",
        0x07 => "NTFS/exFAT",
        0x82 => "Linux swap",
        0x83 => "Linux",
        0xEE => "GPT protective",
        _ => "unknown",
    }
}

/// Parses the 4 primary partition table entries out of a boot sector
/// previously read via `ata::read_sector(0, ...)`. Doesn't itself check
/// the `0x55AA` boot signature - the caller already does that (a sector
/// that isn't really an MBR would just parse into 4 garbage-looking
/// entries, which the `0x00` "unknown"/`empty` fallback keeps harmless
/// to print, but callers should still check the signature first to know
/// whether these entries mean anything at all).
pub fn parse_mbr(sector: &[u8; 512]) -> [PartitionEntry; 4] {
    let mut entries = [PartitionEntry {
        bootable: false,
        partition_type: 0,
        start_lba: 0,
        sector_count: 0,
    }; 4];

    for (i, entry) in entries.iter_mut().enumerate() {
        let offset = 446 + i * 16;
        let raw = &sector[offset..offset + 16];
        entry.bootable = raw[0] == 0x80;
        entry.partition_type = raw[4];
        entry.start_lba = u32::from_le_bytes([raw[8], raw[9], raw[10], raw[11]]);
        entry.sector_count = u32::from_le_bytes([raw[12], raw[13], raw[14], raw[15]]);
    }

    entries
}
