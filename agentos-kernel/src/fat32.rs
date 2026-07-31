//! Minimal read-only FAT32 support: parse the BIOS Parameter Block (BPB),
//! walk the FAT cluster chain, and list a directory's entries.
//!
//! Deliberately stops at *listing* what's there - reading a file's actual
//! contents by name is the next real step once this is proven, not
//! attempted here. Short (8.3) names only; VFAT long-filename entries are
//! recognized and skipped, not reconstructed.

use crate::ata;
use crate::partition::PartitionEntry;
use alloc::string::String;
use alloc::vec::Vec;

pub struct Fat32Info {
    pub sectors_per_cluster: u8,
    pub reserved_sector_count: u16,
    pub num_fats: u8,
    pub fat_size_32: u32,
    pub root_cluster: u32,
    pub partition_start_lba: u32,
}

pub struct DirEntry {
    pub name: String,
    pub is_dir: bool,
    pub size: u32,
    pub start_cluster: u32,
}

/// Reads and parses the FAT32 BPB from the first sector of `partition`.
pub fn read_bpb(partition: &PartitionEntry) -> Result<Fat32Info, &'static str> {
    let mut sector = [0u8; 512];
    ata::read_sector(partition.start_lba, &mut sector)?;

    if sector[510] != 0x55 || sector[511] != 0xAA {
        return Err("FAT32: boot sector missing 0x55AA signature");
    }

    let bytes_per_sector = u16::from_le_bytes([sector[11], sector[12]]);
    if bytes_per_sector != 512 {
        return Err("FAT32: only 512-byte sectors are supported");
    }

    // The authoritative FAT32-vs-FAT12/16 discriminator per the FAT spec:
    // FAT32 always sets this to 0 (its root directory is a cluster chain,
    // not a fixed-size table); FAT12/16 never do. Skipping this check is
    // what let a real FAT12 partition (found on our own disk image - its
    // MBR type byte says 0x0C "FAT32 LBA", but it was actually formatted
    // FAT12, confirmed by the on-disk "FAT12   " filesystem-type string)
    // get misread as FAT32: FAT12/16's BPB has *different* fields at the
    // offsets FAT32 uses for `fat_size_32`/`root_cluster`, which produced
    // garbage large enough to compute an LBA in the billions.
    let root_entry_count = u16::from_le_bytes([sector[17], sector[18]]);
    if root_entry_count != 0 {
        return Err("not FAT32 (root_entry_count != 0 means this is FAT12/16, not supported yet)");
    }

    let sectors_per_cluster = sector[13];
    let reserved_sector_count = u16::from_le_bytes([sector[14], sector[15]]);
    let num_fats = sector[16];
    let fat_size_32 = u32::from_le_bytes([sector[36], sector[37], sector[38], sector[39]]);
    let root_cluster = u32::from_le_bytes([sector[44], sector[45], sector[46], sector[47]]);

    if fat_size_32 == 0 {
        return Err("FAT32: fat_size_32 is 0 - not a valid FAT32 BPB");
    }

    Ok(Fat32Info {
        sectors_per_cluster,
        reserved_sector_count,
        num_fats,
        fat_size_32,
        root_cluster,
        partition_start_lba: partition.start_lba,
    })
}

impl Fat32Info {
    fn first_fat_sector(&self) -> u32 {
        self.partition_start_lba + self.reserved_sector_count as u32
    }

    fn first_data_sector(&self) -> u32 {
        self.first_fat_sector() + self.num_fats as u32 * self.fat_size_32
    }

    /// FAT32 clusters are numbered from 2 - clusters 0 and 1 don't exist
    /// as real data (0 means "free" in the FAT, 1 is reserved), so the
    /// first real data cluster (2) maps to the very first data sector.
    fn cluster_to_lba(&self, cluster: u32) -> u32 {
        self.first_data_sector() + (cluster - 2) * self.sectors_per_cluster as u32
    }

    /// Looks up what comes after `cluster` in its chain. FAT32 entries are
    /// 4 bytes each (128 per 512-byte sector); only the low 28 bits are
    /// meaningful, the top 4 are reserved. `>= 0x0FFFFFF8` means end of
    /// chain; `0` would mean "free", which should never show up while
    /// following a chain that started at an in-use cluster.
    fn next_cluster(&self, cluster: u32) -> Result<Option<u32>, &'static str> {
        let fat_byte_offset = cluster * 4;
        let fat_sector = self.first_fat_sector() + fat_byte_offset / 512;
        let offset_in_sector = (fat_byte_offset % 512) as usize;

        let mut sector = [0u8; 512];
        ata::read_sector(fat_sector, &mut sector)?;

        let raw = u32::from_le_bytes([
            sector[offset_in_sector],
            sector[offset_in_sector + 1],
            sector[offset_in_sector + 2],
            sector[offset_in_sector + 3],
        ]) & 0x0FFF_FFFF;

        match raw {
            0 => Err("FAT32: cluster chain hit a free (0) entry unexpectedly"),
            n if n >= 0x0FFF_FFF8 => Ok(None),
            n => Ok(Some(n)),
        }
    }

    /// Lists directory entries in the cluster chain starting at
    /// `start_cluster` (pass `root_cluster` for the root directory).
    pub fn list_directory(&self, start_cluster: u32) -> Result<Vec<DirEntry>, &'static str> {
        let mut entries = Vec::new();
        let mut cluster = start_cluster;
        let mut sector_buf = [0u8; 512];

        'outer: loop {
            let cluster_lba = self.cluster_to_lba(cluster);
            for s in 0..self.sectors_per_cluster as u32 {
                ata::read_sector(cluster_lba + s, &mut sector_buf)?;
                for raw in sector_buf.as_chunks::<32>().0 {
                    if raw[0] == 0x00 {
                        break 'outer; // no more entries anywhere in this directory
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
            }

            match self.next_cluster(cluster)? {
                Some(next) => cluster = next,
                None => break,
            }
        }

        Ok(entries)
    }
}

/// Reconstructs a "NAME.EXT" string from the raw 8-byte name + 3-byte
/// extension fields of a short directory entry (space-padded on disk).
fn format_short_name(name: &[u8], ext: &[u8]) -> String {
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
