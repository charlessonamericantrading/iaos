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

    /// Finds `name` (case-insensitive, short 8.3 form) in the root
    /// directory and reads its full contents by walking its cluster
    /// chain through the in-memory FAT.
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

    /// Finds the first free cluster (a FAT entry of exactly 0) at or
    /// after cluster 2 - clusters 0/1 aren't real data clusters, they're
    /// reserved by the format itself. Searches the in-memory FAT copy
    /// (loaded once at mount time in `read_bpb`), which is safe here
    /// since nothing else writes to this disk concurrently in a
    /// single-threaded kernel.
    fn find_free_cluster(&self) -> Option<u32> {
        let total_entries = (self.fat_bytes.len() * 2 / 3) as u32;
        for cluster in 2..total_entries {
            let byte_offset = (cluster * 3 / 2) as usize;
            if byte_offset + 1 >= self.fat_bytes.len() {
                break;
            }
            let raw16 =
                u16::from_le_bytes([self.fat_bytes[byte_offset], self.fat_bytes[byte_offset + 1]]);
            let entry = if cluster.is_multiple_of(2) {
                raw16 & 0x0FFF
            } else {
                raw16 >> 4
            };
            if entry == 0 {
                return Some(cluster);
            }
        }
        None
    }

    /// Finds a free slot in the fixed-location root directory - either a
    /// truly never-used entry (first byte `0x00`, meaning everything
    /// from here to the end of the directory is unused too, per the FAT
    /// spec) or a previously-deleted one (`0xE5`, safe to reuse). Returns
    /// the slot's absolute sector LBA and byte offset within that
    /// sector.
    fn find_free_root_entry(&self) -> Result<(u32, usize), &'static str> {
        let mut sector_buf = [0u8; 512];
        for s in 0..self.root_dir_sectors() {
            let lba = self.first_root_dir_sector() + s;
            ata::read_sector(lba, &mut sector_buf)?;
            for (i, raw) in sector_buf.as_chunks::<32>().0.iter().enumerate() {
                if raw[0] == 0x00 || raw[0] == 0xE5 {
                    return Ok((lba, i * 32));
                }
            }
        }
        Err("FAT12: root directory is full, no free entry slot")
    }

    /// Packs `value` (12 bits used) into `cluster`'s entry and writes the
    /// affected sector(s) straight to disk. Only ever updates the
    /// *first* on-disk FAT copy - this matches `next_cluster`'s existing
    /// read path, which never consults a second copy either, so a real
    /// FAT12 volume's usual `num_fats` duplication was already not being
    /// kept meaningfully in sync by this driver before this method
    /// existed.
    ///
    /// Reads the current bytes fresh from disk (not the cached
    /// `fat_bytes` copy) before modifying them, since a 12-bit entry
    /// shares its containing byte pair with its even/odd neighbor -
    /// overwriting blindly would corrupt whatever that neighbor's
    /// cluster is chained to. Handles the one genuine edge case this
    /// packing scheme has: at specific cluster numbers (341, 682, 1023,
    /// ...) the two-byte pair straddles a 512-byte sector boundary, so
    /// both sectors are read/written together when that happens rather
    /// than assumed to never occur.
    fn write_fat_entry_to_disk(&self, cluster: u32, value: u16) -> Result<(), &'static str> {
        let byte_offset = (cluster * 3 / 2) as usize;
        if byte_offset + 1 >= self.fat_bytes.len() {
            return Err("FAT12: cluster number out of range for this FAT");
        }

        let sector0 = self.first_fat_sector() + (byte_offset / 512) as u32;
        let offset0 = byte_offset % 512;
        let straddles = offset0 == 511;

        let mut buf0 = [0u8; 512];
        ata::read_sector(sector0, &mut buf0)?;
        let mut buf1 = [0u8; 512];
        if straddles {
            ata::read_sector(sector0 + 1, &mut buf1)?;
        }

        let byte0 = buf0[offset0];
        let byte1 = if straddles {
            buf1[0]
        } else {
            buf0[offset0 + 1]
        };
        let mut raw16 = u16::from_le_bytes([byte0, byte1]);

        let masked = value & 0x0FFF;
        if cluster.is_multiple_of(2) {
            raw16 = (raw16 & 0xF000) | masked;
        } else {
            raw16 = (raw16 & 0x000F) | (masked << 4);
        }
        let out = raw16.to_le_bytes();

        buf0[offset0] = out[0];
        if straddles {
            buf1[0] = out[1];
            ata::write_sector(sector0, &buf0)?;
            ata::write_sector(sector0 + 1, &buf1)?;
        } else {
            buf0[offset0 + 1] = out[1];
            ata::write_sector(sector0, &buf0)?;
        }
        Ok(())
    }

    /// Creates a new file in the root directory with `name` (short 8.3
    /// form) and `data` as its content - genuinely new, not an overwrite
    /// of something that already exists. Deliberately limited to files
    /// that fit in a *single* cluster (checked up front, clear error
    /// otherwise): a multi-cluster file needs several free-cluster
    /// lookups tracked against each other within one call so the same
    /// free cluster is never handed out twice - real, meaningfully more
    /// bookkeeping, saved for separate future work. Only writes a
    /// short-name directory entry (no VFAT long-name entries) and only
    /// updates the first on-disk FAT copy, same reasoning as
    /// `write_fat_entry_to_disk`.
    pub fn create_file(&self, name: &str, data: &[u8]) -> Result<(), &'static str> {
        let cluster_bytes = self.sectors_per_cluster as usize * 512;
        if data.len() > cluster_bytes {
            return Err("FAT12: create_file only supports files that fit in one cluster");
        }
        if self.read_file(name).is_ok() {
            return Err("FAT12: a file with that name already exists");
        }

        let short_name = to_short_name(name)?;
        let cluster = self
            .find_free_cluster()
            .ok_or("FAT12: no free cluster available")?;
        let (entry_lba, entry_offset) = self.find_free_root_entry()?;

        // Write the file's data into its one cluster - a partially-used
        // final sector is zero-padded, same as write_file already does.
        let cluster_lba = self.cluster_to_lba(cluster);
        for s in 0..self.sectors_per_cluster as u32 {
            let mut sector_buf = [0u8; 512];
            let start = s as usize * 512;
            if start < data.len() {
                let end = (start + 512).min(data.len());
                sector_buf[..end - start].copy_from_slice(&data[start..end]);
            }
            ata::write_sector(cluster_lba + s, &sector_buf)?;
        }

        // Mark this cluster as a complete (single-cluster) chain in its
        // own right - 0x0FFF is comfortably within the `>= 0x0FF8` end-
        // of-chain range `next_cluster` already checks for.
        self.write_fat_entry_to_disk(cluster, 0x0FFF)?;

        // A real timestamp from the CMOS RTC (Fase 20) rather than a
        // zeroed/fake one - nothing in this kernel reads FAT timestamps
        // back yet, so this is honestly more about correctness/
        // completeness than anything currently depended on.
        let time = crate::rtc::read_time();
        let (fat_time, fat_date) = to_fat_datetime(&time);

        let mut entry = [0u8; 32];
        entry[0..11].copy_from_slice(&short_name);
        entry[11] = 0x20; // ATTR_ARCHIVE
        entry[14..16].copy_from_slice(&fat_time.to_le_bytes()); // creation time
        entry[16..18].copy_from_slice(&fat_date.to_le_bytes()); // creation date
        entry[18..20].copy_from_slice(&fat_date.to_le_bytes()); // last access date
        entry[20..22].copy_from_slice(&0u16.to_le_bytes()); // high cluster word - always 0, FAT12 clusters fit in 12 bits
        entry[22..24].copy_from_slice(&fat_time.to_le_bytes()); // write time
        entry[24..26].copy_from_slice(&fat_date.to_le_bytes()); // write date
        entry[26..28].copy_from_slice(&(cluster as u16).to_le_bytes());
        entry[28..32].copy_from_slice(&(data.len() as u32).to_le_bytes());

        let mut sector_buf = [0u8; 512];
        ata::read_sector(entry_lba, &mut sector_buf)?;
        sector_buf[entry_offset..entry_offset + 32].copy_from_slice(&entry);
        ata::write_sector(entry_lba, &sector_buf)?;

        Ok(())
    }
}

/// Converts a "NAME" or "NAME.EXT" string into the padded 8.3 short-name
/// format FAT directory entries use on disk (11 bytes: 8 for the name,
/// space-padded, then 3 for the extension, space-padded). Uppercases
/// everything, matching FAT short-name convention. Short-name-only: no
/// VFAT long-name entries are ever written, so anything not fitting 8.3
/// is rejected rather than silently truncated.
fn to_short_name(name: &str) -> Result<[u8; 11], &'static str> {
    let (base, ext) = match name.split_once('.') {
        Some((b, e)) => (b, e),
        None => (name, ""),
    };
    if base.is_empty() || base.len() > 8 || ext.len() > 3 {
        return Err("FAT12: name must fit the 8.3 short-name format");
    }
    let mut out = [b' '; 11];
    for (i, b) in base.bytes().enumerate() {
        out[i] = b.to_ascii_uppercase();
    }
    for (i, b) in ext.bytes().enumerate() {
        out[8 + i] = b.to_ascii_uppercase();
    }
    Ok(out)
}

/// Packs a `RtcTime` into FAT's on-disk time/date u16 formats: time is
/// hours(5 bits):minutes(6 bits):seconds/2(5 bits); date is (year-1980)
/// (7 bits):month(4 bits):day(5 bits).
fn to_fat_datetime(t: &crate::rtc::RtcTime) -> (u16, u16) {
    let fat_time = ((t.hours as u16) << 11) | ((t.minutes as u16) << 5) | (t.seconds as u16 / 2);
    let year_offset = (t.year.saturating_sub(1980)).min(127);
    let fat_date = (year_offset << 9) | ((t.month as u16) << 5) | (t.day as u16);
    (fat_time, fat_date)
}
