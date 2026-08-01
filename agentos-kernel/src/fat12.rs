//! Minimal read-only FAT12 support - the older, simpler sibling of FAT32,
//! used for small volumes. Our own disk's `0x0C`-typed ("FAT32 LBA")
//! partition turned out to actually be formatted this way (see
//! `fat32.rs`'s `read_bpb` doc comment for how that was discovered) - the
//! MBR type byte apparently doesn't always match reality for a volume
//! this small (4096 sectors = 2 MiB, squarely FAT12 territory).
//!
//! Two real structural differences from FAT32, both handled here:
//! - The root directory lives at a *fixed* sector range (computed from
//!   `root_entry_count`), not a cluster chain like FAT32's.
//! - FAT entries are packed 12 bits each, two entries per 3 bytes - the
//!   classic "easy to get subtly wrong" part of this format. The whole
//!   FAT is read into memory once (FAT12 volumes are always small, so
//!   this is at most a few KiB) specifically to avoid ever having to
//!   handle a 12-bit entry straddling a sector boundary during lookups.

use crate::ata;
use crate::fat_common::{self, DirEntry};
use crate::partition::PartitionEntry;
use alloc::vec;
use alloc::vec::Vec;

pub struct Fat12Info {
    sectors_per_cluster: u8,
    num_fats: u8,
    fat_size_16: u16,
    root_entry_count: u16,
    reserved_sector_count: u16,
    partition_start_lba: u32,
    fat_bytes: Vec<u8>,
}

pub fn read_bpb(partition: &PartitionEntry) -> Result<Fat12Info, &'static str> {
    let mut sector = [0u8; 512];
    ata::read_sector(partition.start_lba, &mut sector)?;

    if sector[510] != 0x55 || sector[511] != 0xAA {
        return Err("FAT12: boot sector missing 0x55AA signature");
    }
    let bytes_per_sector = u16::from_le_bytes([sector[11], sector[12]]);
    if bytes_per_sector != 512 {
        return Err("FAT12: only 512-byte sectors are supported");
    }

    let sectors_per_cluster = sector[13];
    let reserved_sector_count = u16::from_le_bytes([sector[14], sector[15]]);
    let num_fats = sector[16];
    let root_entry_count = u16::from_le_bytes([sector[17], sector[18]]);
    let fat_size_16 = u16::from_le_bytes([sector[22], sector[23]]);

    // Mirrors fat32::read_bpb's check in the opposite direction: a real
    // FAT32 volume always zeros this field, so seeing it nonzero is what
    // tells us we're in the right place; FAT32 images should fail here
    // and be handled by fat32.rs instead.
    if root_entry_count == 0 {
        return Err("not FAT12/16 (root_entry_count == 0 means this is FAT32)");
    }
    if fat_size_16 == 0 {
        return Err("FAT12: fat_size_16 is 0 - not a valid FAT12/16 BPB");
    }

    let first_fat_sector = partition.start_lba + reserved_sector_count as u32;
    let fat_sectors = fat_size_16 as u32;
    let mut fat_bytes = vec![0u8; (fat_sectors * 512) as usize];
    for s in 0..fat_sectors {
        let mut buf = [0u8; 512];
        ata::read_sector(first_fat_sector + s, &mut buf)?;
        let start = (s * 512) as usize;
        fat_bytes[start..start + 512].copy_from_slice(&buf);
    }

    Ok(Fat12Info {
        sectors_per_cluster,
        num_fats,
        fat_size_16,
        root_entry_count,
        reserved_sector_count,
        partition_start_lba: partition.start_lba,
        fat_bytes,
    })
}

impl Fat12Info {
    fn first_fat_sector(&self) -> u32 {
        self.partition_start_lba + self.reserved_sector_count as u32
    }

    fn root_dir_sectors(&self) -> u32 {
        (self.root_entry_count as u32 * 32).div_ceil(512)
    }

    fn first_root_dir_sector(&self) -> u32 {
        self.first_fat_sector() + self.num_fats as u32 * self.fat_size_16 as u32
    }

    fn first_data_sector(&self) -> u32 {
        self.first_root_dir_sector() + self.root_dir_sectors()
    }

    fn cluster_to_lba(&self, cluster: u32) -> u32 {
        self.first_data_sector() + (cluster - 2) * self.sectors_per_cluster as u32
    }

    /// Looks up the 12-bit FAT entry for `cluster` from the in-memory
    /// copy of the FAT. Two entries are packed per 3 bytes: for an even
    /// cluster number the entry is the low 12 bits of the u16 at the
    /// packed byte offset; for odd, the high 12 bits.
    fn next_cluster(&self, cluster: u32) -> Result<Option<u32>, &'static str> {
        let byte_offset = (cluster * 3 / 2) as usize;
        if byte_offset + 1 >= self.fat_bytes.len() {
            return Err("FAT12: cluster number out of range for this FAT");
        }
        let raw16 =
            u16::from_le_bytes([self.fat_bytes[byte_offset], self.fat_bytes[byte_offset + 1]]);
        let entry = if cluster.is_multiple_of(2) {
            raw16 & 0x0FFF
        } else {
            raw16 >> 4
        };

        match entry {
            0 => Err("FAT12: cluster chain hit a free (0) entry unexpectedly"),
            n if n >= 0x0FF8 => Ok(None),
            n => Ok(Some(n as u32)),
        }
    }

    /// Lists the fixed-location root directory - FAT12/16's root isn't a
    /// cluster chain like FAT32's, it's a plain run of sectors right
    /// after the FAT(s).
    pub fn list_root_directory(&self) -> Result<Vec<DirEntry>, &'static str> {
        let mut entries = Vec::new();
        let mut sector_buf = [0u8; 512];
        for s in 0..self.root_dir_sectors() {
            ata::read_sector(self.first_root_dir_sector() + s, &mut sector_buf)?;
            if fat_common::parse_dir_sector(&sector_buf, &mut entries) {
                break;
            }
        }
        Ok(entries)
    }

    /// Finds `name` (case-insensitive, short 8.3 form) in the root
    /// directory and reads its full contents by walking its cluster
    /// chain through the in-memory FAT.
    /// Overwrites an existing file's content with `data`, which must be
    /// EXACTLY the same length as the file's current size - this first
    /// version reuses the file's existing cluster chain as-is and
    /// touches neither the FAT nor the directory entry (size field
    /// included). Creating a new file (needs a free directory-entry
    /// search + cluster allocation) or resizing an existing one (needs
    /// FAT chain growth/shrink) are both substantially more work,
    /// deliberately not attempted here.
    pub fn write_file(&self, name: &str, data: &[u8]) -> Result<(), &'static str> {
        let entries = self.list_root_directory()?;
        let entry = entries
            .iter()
            .find(|e| !e.is_dir && e.name.eq_ignore_ascii_case(name))
            .ok_or("FAT12: file not found in root directory")?;

        if data.len() != entry.size as usize {
            return Err("FAT12: write_file only supports same-size overwrites for now");
        }
        if entry.size == 0 {
            return Ok(());
        }

        let mut cluster = entry.start_cluster;
        let mut written = 0usize;

        loop {
            let cluster_lba = self.cluster_to_lba(cluster);
            for s in 0..self.sectors_per_cluster as u32 {
                let remaining = data.len() - written;
                if remaining == 0 {
                    return Ok(());
                }
                let take = remaining.min(512);
                let mut sector_buf = [0u8; 512];
                sector_buf[..take].copy_from_slice(&data[written..written + take]);
                // A cluster's last sector may only be partially covered
                // by real file data (size isn't always a multiple of
                // 512) - the trailing bytes beyond `take` are left
                // zeroed. That's fine: nothing about a FAT file's bytes
                // past its logical `size` is meaningful, and read_file
                // only ever reads back exactly `entry.size` bytes.
                ata::write_sector(cluster_lba + s, &sector_buf)?;
                written += take;
            }
            if written >= data.len() {
                return Ok(());
            }
            match self.next_cluster(cluster)? {
                Some(next) => cluster = next,
                None => return Err("FAT12: cluster chain ended before all data was written"),
            }
        }
    }

    pub fn read_file(&self, name: &str) -> Result<Vec<u8>, &'static str> {
        let entries = self.list_root_directory()?;
        let entry = entries
            .iter()
            .find(|e| !e.is_dir && e.name.eq_ignore_ascii_case(name))
            .ok_or("FAT12: file not found in root directory")?;

        if entry.size == 0 {
            return Ok(Vec::new());
        }

        let mut data = Vec::with_capacity(entry.size as usize);
        let mut cluster = entry.start_cluster;
        let mut sector_buf = [0u8; 512];

        loop {
            let cluster_lba = self.cluster_to_lba(cluster);
            for s in 0..self.sectors_per_cluster as u32 {
                ata::read_sector(cluster_lba + s, &mut sector_buf)?;
                let remaining = entry.size as usize - data.len();
                let take = remaining.min(512);
                data.extend_from_slice(&sector_buf[..take]);
                if data.len() >= entry.size as usize {
                    return Ok(data);
                }
            }

            match self.next_cluster(cluster)? {
                Some(next) => cluster = next,
                None => return Ok(data),
            }
        }
    }
}
