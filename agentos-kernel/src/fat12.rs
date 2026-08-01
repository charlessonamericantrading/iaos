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

/// Where a directory's entries physically live - the root has a
/// special, fixed-location layout (see the module doc); any
/// subdirectory is just an ordinary cluster chain, the same shape
/// `create_directory`'s own content already assumes. Lets
/// `create_file`/`read_file`/`write_file`/`delete_file` (and their
/// internal helpers) target either without duplicating their logic -
/// see the `_in` variants of each, added once multi-cluster/resize
/// support (Fase 29/30) made a subdirectory's cluster-chain-based
/// storage no harder to write into than the root's fixed sectors.
#[derive(Clone, Copy)]
enum DirLocation {
    Root,
    Cluster(u32),
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

    /// Returns the sector LBAs holding a directory's entries, in order -
    /// either the root's fixed range, or a subdirectory's cluster chain
    /// walked to completion. Unifies the root-vs-cluster-chain
    /// distinction into one flat list every entry-scanning helper below
    /// can iterate the same way, since a directory here is never more
    /// than a handful of sectors (FAT12 volumes are small; today's
    /// single-cluster subdirectories doubly so).
    fn directory_sectors(&self, dir: DirLocation) -> Result<Vec<u32>, &'static str> {
        match dir {
            DirLocation::Root => Ok((0..self.root_dir_sectors())
                .map(|s| self.first_root_dir_sector() + s)
                .collect()),
            DirLocation::Cluster(start_cluster) => {
                let mut lbas = Vec::new();
                let mut cluster = Some(start_cluster);
                while let Some(c) = cluster {
                    let cluster_lba = self.cluster_to_lba(c);
                    lbas.extend((0..self.sectors_per_cluster as u32).map(|s| cluster_lba + s));
                    cluster = self.next_cluster(c)?;
                }
                Ok(lbas)
            }
        }
    }

    /// Lists a directory's entries regardless of where it lives -
    /// dispatches to the two existing, independently-verified listing
    /// functions rather than reimplementing either.
    fn list_entries_in(&self, dir: DirLocation) -> Result<Vec<DirEntry>, &'static str> {
        match dir {
            DirLocation::Root => self.list_root_directory(),
            DirLocation::Cluster(c) => self.list_directory(c),
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

    /// Overwrites an existing file's content with `data`, growing or
    /// shrinking its cluster chain as needed - originally same-size-only
    /// ("resizing needs FAT chain growth/shrink... substantially more
    /// work, deliberately not attempted"); `create_file`'s multi-cluster
    /// support (`find_free_clusters`) turned out to be exactly the
    /// missing piece for growing too, and shrinking is the same problem
    /// from the other direction - freeing the excess trailing clusters
    /// instead of allocating extra ones.
    pub fn write_file(&mut self, name: &str, data: &[u8]) -> Result<(), &'static str> {
        self.write_file_impl(DirLocation::Root, name, data)
    }

    /// Same as `write_file`, but for a file inside the subdirectory
    /// whose own cluster is `dir_cluster` (found via a prior `ls`/
    /// `list_directory` lookup) instead of the root.
    pub fn write_file_in(
        &mut self,
        dir_cluster: u32,
        name: &str,
        data: &[u8],
    ) -> Result<(), &'static str> {
        self.write_file_impl(DirLocation::Cluster(dir_cluster), name, data)
    }

    fn write_file_impl(
        &mut self,
        dir: DirLocation,
        name: &str,
        data: &[u8],
    ) -> Result<(), &'static str> {
        let (entry_lba, entry_offset, entry) = self.find_entry_location_in(dir, name)?;
        if entry.is_dir {
            return Err("FAT12: write_file does not support directories");
        }

        // Walk the file's existing chain - needed either way, to know
        // exactly which clusters it already owns before deciding
        // whether any need to be added or freed.
        let mut clusters = alloc::vec::Vec::new();
        let mut cluster = Some(entry.start_cluster);
        while let Some(c) = cluster {
            clusters.push(c);
            cluster = self.next_cluster(c)?;
        }

        let cluster_bytes = self.sectors_per_cluster as usize * 512;
        let new_clusters_needed = data.len().div_ceil(cluster_bytes).max(1);

        if new_clusters_needed > clusters.len() {
            let extra = self.find_free_clusters(new_clusters_needed - clusters.len())?;
            clusters.extend(extra);
        } else if new_clusters_needed < clusters.len() {
            // Free every cluster past the new end of the chain - each
            // one's entry goes back to 0x000 (free), same as
            // delete_file already does for a whole chain.
            for &c in &clusters[new_clusters_needed..] {
                self.write_fat_entry_to_disk(c, 0x0000)?;
            }
            clusters.truncate(new_clusters_needed);
        }

        // Write data across the (possibly resized) chain, chaining each
        // cluster to the next - identical in shape to create_file's own
        // write+chain loop, since both are "lay data across exactly
        // these clusters, in this order" once the chain length is
        // decided.
        let mut written = 0usize;
        for (i, &c) in clusters.iter().enumerate() {
            let cluster_lba = self.cluster_to_lba(c);
            for s in 0..self.sectors_per_cluster as u32 {
                let mut sector_buf = [0u8; 512];
                if written < data.len() {
                    let end = (written + 512).min(data.len());
                    sector_buf[..end - written].copy_from_slice(&data[written..end]);
                    written = end;
                }
                // A cluster's last sector may only be partially covered
                // by real file data (size isn't always a multiple of
                // 512) - the trailing bytes beyond that are left
                // zeroed, same reasoning as before: nothing about a FAT
                // file's bytes past its logical size is meaningful.
                ata::write_sector(cluster_lba + s, &sector_buf)?;
            }
            let next_entry = match clusters.get(i + 1) {
                Some(&next_cluster) => next_cluster as u16,
                None => 0x0FFF,
            };
            self.write_fat_entry_to_disk(c, next_entry)?;
        }

        // The only directory-entry field a resize can change is size -
        // start_cluster never does, since clusters[0] is always the
        // same first cluster the file already had.
        let mut sector_buf = [0u8; 512];
        ata::read_sector(entry_lba, &mut sector_buf)?;
        sector_buf[entry_offset + 28..entry_offset + 32]
            .copy_from_slice(&(data.len() as u32).to_le_bytes());
        ata::write_sector(entry_lba, &sector_buf)?;

        Ok(())
    }

    /// Finds `name` (case-insensitive, short 8.3 form) in the root
    /// directory and reads its full contents by walking its cluster
    /// chain through the in-memory FAT.
    pub fn read_file(&self, name: &str) -> Result<Vec<u8>, &'static str> {
        self.read_file_impl(DirLocation::Root, name)
    }

    /// Same as `read_file`, but for a file inside the subdirectory whose
    /// own cluster is `dir_cluster` instead of the root.
    pub fn read_file_in(&self, dir_cluster: u32, name: &str) -> Result<Vec<u8>, &'static str> {
        self.read_file_impl(DirLocation::Cluster(dir_cluster), name)
    }

    fn read_file_impl(&self, dir: DirLocation, name: &str) -> Result<Vec<u8>, &'static str> {
        let not_found = match dir {
            DirLocation::Root => "FAT12: file not found in root directory",
            DirLocation::Cluster(_) => "FAT12: file not found in directory",
        };
        let entries = self.list_entries_in(dir)?;
        let entry = entries
            .iter()
            .find(|e| !e.is_dir && e.name.eq_ignore_ascii_case(name))
            .ok_or(not_found)?;

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

    /// Finds `count` distinct free clusters (FAT entry == 0) at or after
    /// cluster 2 - clusters 0/1 aren't real data clusters, they're
    /// reserved by the format itself. Searches the in-memory FAT copy
    /// (loaded once at mount time in `read_bpb`) in a single pass,
    /// collecting entries as it goes - unlike calling a "find one free
    /// cluster" function `count` times in a row, which would return the
    /// *same* cluster every time, since nothing marks a cluster "taken"
    /// until its FAT entry is actually written. Returns them in
    /// ascending order; safe to search the cached copy rather than disk
    /// directly since nothing else writes to this disk concurrently in
    /// a single-threaded kernel.
    fn find_free_clusters(&self, count: usize) -> Result<alloc::vec::Vec<u32>, &'static str> {
        let mut found = alloc::vec::Vec::with_capacity(count);
        let total_entries = (self.fat_bytes.len() * 2 / 3) as u32;
        for cluster in 2..total_entries {
            if found.len() == count {
                break;
            }
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
                found.push(cluster);
            }
        }
        if found.len() < count {
            return Err("FAT12: not enough free clusters available for this file");
        }
        Ok(found)
    }

    /// Finds a free slot in the fixed-location root directory - either a
    /// truly never-used entry (first byte `0x00`, meaning everything
    /// from here to the end of the directory is unused too, per the FAT
    /// spec) or a previously-deleted one (`0xE5`, safe to reuse). Returns
    /// the slot's absolute sector LBA and byte offset within that
    /// sector.
    fn find_free_root_entry(&self) -> Result<(u32, usize), &'static str> {
        self.find_free_entry_in(DirLocation::Root)
    }

    /// Same as `find_free_root_entry`, generalized to a subdirectory's
    /// cluster chain too. Deliberately does NOT grow the chain with a
    /// fresh cluster if every existing slot is taken - same single-
    /// cluster-only limitation `create_directory` already has, honestly
    /// surfaced as an error rather than silently attempted.
    fn find_free_entry_in(&self, dir: DirLocation) -> Result<(u32, usize), &'static str> {
        let mut sector_buf = [0u8; 512];
        for lba in self.directory_sectors(dir)? {
            ata::read_sector(lba, &mut sector_buf)?;
            for (i, raw) in sector_buf.as_chunks::<32>().0.iter().enumerate() {
                if raw[0] == 0x00 || raw[0] == 0xE5 {
                    return Ok((lba, i * 32));
                }
            }
        }
        Err("FAT12: directory is full, no free entry slot")
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
    ///
    /// Also writes the same two bytes into `self.fat_bytes` afterward,
    /// keeping the in-memory cache coherent with disk. Found the hard
    /// way: without this, `delete_file` on a file created earlier in the
    /// same mount would call `next_cluster` and see the cluster's *old*
    /// (stale, still-free) entry instead of the one just written here,
    /// failing with "hit a free (0) entry unexpectedly". `read_file`
    /// never hit this because its loop returns as soon as it has enough
    /// bytes, so a single-cluster file never reaches its own
    /// `next_cluster` call - `delete_file` is the first caller that
    /// walks the chain unconditionally, regardless of file size.
    fn write_fat_entry_to_disk(&mut self, cluster: u32, value: u16) -> Result<(), &'static str> {
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

        self.fat_bytes[byte_offset] = out[0];
        self.fat_bytes[byte_offset + 1] = out[1];

        Ok(())
    }

    /// Creates a new file in the root directory with `name` (short 8.3
    /// form) and `data` as its content - genuinely new, not an overwrite
    /// of something that already exists. Handles files of any size,
    /// chaining as many clusters together as needed (originally limited
    /// to a single cluster - see the multi-cluster note on
    /// `find_free_clusters` for why that needed its own design pass:
    /// allocating several free clusters safely means finding them all
    /// in one pass, not calling a "find one free cluster" function
    /// several times, which would hand out the same cluster repeatedly).
    /// Only writes a short-name directory entry (no VFAT long-name
    /// entries) and only updates the first on-disk FAT copy, same
    /// reasoning as `write_fat_entry_to_disk`.
    pub fn create_file(&mut self, name: &str, data: &[u8]) -> Result<(), &'static str> {
        self.create_file_impl(DirLocation::Root, name, data)
    }

    /// Same as `create_file`, but for a new file inside the subdirectory
    /// whose own cluster is `dir_cluster` instead of the root.
    pub fn create_file_in(
        &mut self,
        dir_cluster: u32,
        name: &str,
        data: &[u8],
    ) -> Result<(), &'static str> {
        self.create_file_impl(DirLocation::Cluster(dir_cluster), name, data)
    }

    fn create_file_impl(
        &mut self,
        dir: DirLocation,
        name: &str,
        data: &[u8],
    ) -> Result<(), &'static str> {
        if self.read_file_impl(dir, name).is_ok() {
            return Err("FAT12: a file with that name already exists");
        }

        let short_name = to_short_name(name)?;
        let cluster_bytes = self.sectors_per_cluster as usize * 512;
        // At least one cluster even for an empty (0-byte) file, matching
        // this method's existing convention from before multi-cluster
        // support: every created file gets a real cluster to anchor its
        // directory entry's start_cluster field to.
        let clusters_needed = data.len().div_ceil(cluster_bytes).max(1);
        let clusters = self.find_free_clusters(clusters_needed)?;
        let (entry_lba, entry_offset) = self.find_free_entry_in(dir)?;

        // Write the file's data across its clusters in order, chaining
        // each to the next as we go - a partially-used final sector is
        // zero-padded, same as write_file already does; with sizing
        // rounded up via div_ceil above, only the last cluster's tail
        // sectors can ever be partially used, never a whole cluster.
        let mut written = 0usize;
        for (i, &cluster) in clusters.iter().enumerate() {
            let cluster_lba = self.cluster_to_lba(cluster);
            for s in 0..self.sectors_per_cluster as u32 {
                let mut sector_buf = [0u8; 512];
                if written < data.len() {
                    let end = (written + 512).min(data.len());
                    sector_buf[..end - written].copy_from_slice(&data[written..end]);
                    written = end;
                }
                ata::write_sector(cluster_lba + s, &sector_buf)?;
            }
            let next_entry = match clusters.get(i + 1) {
                Some(&next_cluster) => next_cluster as u16,
                // 0x0FFF is comfortably within the `>= 0x0FF8` end-of-
                // chain range `next_cluster` already checks for.
                None => 0x0FFF,
            };
            self.write_fat_entry_to_disk(cluster, next_entry)?;
        }

        // A real timestamp from the CMOS RTC (Fase 20) rather than a
        // zeroed/fake one - nothing in this kernel reads FAT timestamps
        // back yet, so this is honestly more about correctness/
        // completeness than anything currently depended on.
        let time = crate::rtc::read_time();
        let (fat_time, fat_date) = to_fat_datetime(&time);
        let entry = build_dir_entry(
            &short_name,
            0x20, // ATTR_ARCHIVE
            clusters[0],
            data.len() as u32,
            fat_time,
            fat_date,
        );

        let mut sector_buf = [0u8; 512];
        ata::read_sector(entry_lba, &mut sector_buf)?;
        sector_buf[entry_offset..entry_offset + 32].copy_from_slice(&entry);
        ata::write_sector(entry_lba, &sector_buf)?;

        Ok(())
    }

    /// Creates a new, empty subdirectory in the root directory. Unlike
    /// the root itself (fixed-location, no cluster of its own), a
    /// subdirectory is just an ordinary single-cluster chain holding two
    /// real entries - "." (points to itself) and ".." (points to its
    /// parent; cluster `0` is the real FAT convention for "the parent is
    /// the root directory", since the root has no cluster number of its
    /// own to point back to) - with everything else in the cluster
    /// zeroed.
    ///
    /// That zeroing is deliberate, not just tidiness: a "free" cluster
    /// (FAT entry `0x000`) says nothing about what's still sitting in
    /// its actual data bytes on disk - this kernel's own self-tests
    /// create-then-delete files every boot, so a freshly-allocated
    /// cluster can easily still hold a previous file's leftover bytes.
    /// For a plain file that's harmless (only bytes up to its logical
    /// size are ever read back), but a directory's bytes are always
    /// interpreted as structured entries - stale data past "."/".."
    /// could be misread as bogus additional entries if left in place.
    pub fn create_directory(&mut self, name: &str) -> Result<(), &'static str> {
        let entries = self.list_root_directory()?;
        if entries.iter().any(|e| e.name.eq_ignore_ascii_case(name)) {
            return Err("FAT12: a file or directory with that name already exists");
        }

        let short_name = to_short_name(name)?;
        let cluster = self.find_free_clusters(1)?[0];
        let (entry_lba, entry_offset) = self.find_free_root_entry()?;

        let time = crate::rtc::read_time();
        let (fat_time, fat_date) = to_fat_datetime(&time);

        let mut dot_name = [b' '; 11];
        dot_name[0] = b'.';
        let mut dotdot_name = [b' '; 11];
        dotdot_name[0] = b'.';
        dotdot_name[1] = b'.';
        let dot_entry = build_dir_entry(&dot_name, 0x10, cluster, 0, fat_time, fat_date);
        let dotdot_entry = build_dir_entry(&dotdot_name, 0x10, 0, 0, fat_time, fat_date);

        let cluster_lba = self.cluster_to_lba(cluster);
        let mut first_sector = [0u8; 512];
        first_sector[0..32].copy_from_slice(&dot_entry);
        first_sector[32..64].copy_from_slice(&dotdot_entry);
        ata::write_sector(cluster_lba, &first_sector)?;
        for s in 1..self.sectors_per_cluster as u32 {
            ata::write_sector(cluster_lba + s, &[0u8; 512])?;
        }

        self.write_fat_entry_to_disk(cluster, 0x0FFF)?;

        let dir_entry = build_dir_entry(&short_name, 0x10, cluster, 0, fat_time, fat_date);
        let mut root_sector = [0u8; 512];
        ata::read_sector(entry_lba, &mut root_sector)?;
        root_sector[entry_offset..entry_offset + 32].copy_from_slice(&dir_entry);
        ata::write_sector(entry_lba, &root_sector)?;

        Ok(())
    }

    /// Lists the entries inside a SUBdirectory by walking its cluster
    /// chain - unlike `list_root_directory`, which reads the root's
    /// fixed-location sectors directly, a subdirectory's content is an
    /// ordinary cluster chain of 32-byte entries (starting with the real
    /// "." and ".." entries `create_directory` writes - not skipped or
    /// specially handled here, so they show up in the result like any
    /// other entry, matching real FAT behavior).
    pub fn list_directory(&self, start_cluster: u32) -> Result<Vec<DirEntry>, &'static str> {
        let mut entries = Vec::new();
        let mut cluster = Some(start_cluster);
        let mut sector_buf = [0u8; 512];

        while let Some(c) = cluster {
            let cluster_lba = self.cluster_to_lba(c);
            for s in 0..self.sectors_per_cluster as u32 {
                ata::read_sector(cluster_lba + s, &mut sector_buf)?;
                if fat_common::parse_dir_sector(&sector_buf, &mut entries) {
                    return Ok(entries);
                }
            }
            cluster = self.next_cluster(c)?;
        }

        Ok(entries)
    }

    /// Finds `name`'s directory entry AND its exact on-disk location
    /// (sector LBA plus byte offset within that sector) - unlike
    /// `list_root_directory`, which only returns the parsed `DirEntry`
    /// fields, this is needed to actually modify the entry in place
    /// (`delete_file` marking it deleted). Mirrors `fat_common`'s own
    /// entry-parsing logic directly rather than reusing
    /// `parse_dir_sector`, since that only returns a `Vec<DirEntry>` with
    /// no indication of which raw 32-byte slot each one came from.
    fn find_entry_location(&self, name: &str) -> Result<(u32, usize, DirEntry), &'static str> {
        self.find_entry_location_in(DirLocation::Root, name)
    }

    /// Same as `find_entry_location`, generalized to a subdirectory's
    /// cluster chain too - the `0x00` "no more entries" check and the
    /// deleted/VFAT/volume-label skip logic apply identically either
    /// way, since both directory shapes share the exact same 32-byte
    /// entry format.
    fn find_entry_location_in(
        &self,
        dir: DirLocation,
        name: &str,
    ) -> Result<(u32, usize, DirEntry), &'static str> {
        let not_found = match dir {
            DirLocation::Root => "FAT12: file not found in root directory",
            DirLocation::Cluster(_) => "FAT12: file not found in directory",
        };
        let mut sector_buf = [0u8; 512];
        for lba in self.directory_sectors(dir)? {
            ata::read_sector(lba, &mut sector_buf)?;
            for (i, raw) in sector_buf.as_chunks::<32>().0.iter().enumerate() {
                if raw[0] == 0x00 {
                    return Err(not_found);
                }
                if raw[0] == 0xE5 || raw[11] == 0x0F || raw[11] & 0x08 != 0 {
                    continue; // deleted / long-filename (VFAT) / volume label
                }
                let entry_name = fat_common::format_short_name(&raw[0..8], &raw[8..11]);
                if entry_name.eq_ignore_ascii_case(name) {
                    let is_dir = raw[11] & 0x10 != 0;
                    let hi = u16::from_le_bytes([raw[20], raw[21]]) as u32;
                    let lo = u16::from_le_bytes([raw[26], raw[27]]) as u32;
                    let size = u32::from_le_bytes([raw[28], raw[29], raw[30], raw[31]]);
                    return Ok((
                        lba,
                        i * 32,
                        DirEntry {
                            name: entry_name,
                            is_dir,
                            size,
                            start_cluster: (hi << 16) | lo,
                        },
                    ));
                }
            }
        }
        Err(not_found)
    }

    /// Deletes `name`: frees every cluster in its chain (each FAT entry
    /// set back to `0x000`) and marks its directory entry deleted (first
    /// byte `0xE5`, the standard FAT convention) - the rest of the
    /// entry's bytes are left as-is, matching how real FAT filesystems
    /// handle deletion; only that one byte actually needs to change for
    /// the slot to be treated as free/reusable by `find_free_root_entry`
    /// (or any other FAT-aware reader).
    pub fn delete_file(&mut self, name: &str) -> Result<(), &'static str> {
        self.delete_file_impl(DirLocation::Root, name)
    }

    /// Same as `delete_file`, but for a file inside the subdirectory
    /// whose own cluster is `dir_cluster` instead of the root.
    pub fn delete_file_in(&mut self, dir_cluster: u32, name: &str) -> Result<(), &'static str> {
        self.delete_file_impl(DirLocation::Cluster(dir_cluster), name)
    }

    fn delete_file_impl(&mut self, dir: DirLocation, name: &str) -> Result<(), &'static str> {
        let (entry_lba, entry_offset, entry) = self.find_entry_location_in(dir, name)?;
        if entry.is_dir {
            return Err("FAT12: delete_file does not support directories");
        }

        if entry.size > 0 {
            let mut cluster = Some(entry.start_cluster);
            while let Some(c) = cluster {
                // Capture the next link before zeroing this entry - once
                // it's zeroed, the chain onward from here is lost.
                let next = self.next_cluster(c)?;
                self.write_fat_entry_to_disk(c, 0x0000)?;
                cluster = next;
            }
        }

        let mut sector_buf = [0u8; 512];
        ata::read_sector(entry_lba, &mut sector_buf)?;
        sector_buf[entry_offset] = 0xE5;
        ata::write_sector(entry_lba, &sector_buf)?;

        Ok(())
    }

    /// Deletes an EMPTY subdirectory: frees its cluster chain and marks
    /// its root-directory entry deleted (`0xE5`) - same mechanics as
    /// `delete_file`, since freeing a chain and marking an entry deleted
    /// doesn't care whether the chain held file bytes or directory
    /// entries. Refuses to delete a non-empty directory (anything beyond
    /// the `.`/`..` entries every directory `create_directory` writes) -
    /// the standard, safe `rmdir` semantic. This can't currently be
    /// exercised against a genuinely non-empty directory in practice,
    /// since nothing yet puts a real file *inside* a subdirectory
    /// (`create_file`/`read_file`/`write_file` are still root-only) - but
    /// the check is correct and forward-looking regardless, not
    /// speculative dead code: it's exactly what would matter the moment
    /// subdirectory-scoped file I/O is added.
    pub fn delete_directory(&mut self, name: &str) -> Result<(), &'static str> {
        let (entry_lba, entry_offset, entry) = self.find_entry_location(name)?;
        if !entry.is_dir {
            return Err("FAT12: delete_directory does not support files - use rm");
        }

        let entries = self.list_directory(entry.start_cluster)?;
        if entries.len() > 2 {
            return Err("FAT12: directory is not empty");
        }

        let mut cluster = Some(entry.start_cluster);
        while let Some(c) = cluster {
            let next = self.next_cluster(c)?;
            self.write_fat_entry_to_disk(c, 0x0000)?;
            cluster = next;
        }

        let mut sector_buf = [0u8; 512];
        ata::read_sector(entry_lba, &mut sector_buf)?;
        sector_buf[entry_offset] = 0xE5;
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

/// Packs one 32-byte FAT directory entry: short name, attribute byte, a
/// single timestamp used for creation/write/access alike (this driver
/// doesn't track them separately), starting cluster, and size. Shared
/// between `create_file` and `create_directory` - both are ultimately
/// "write one root-directory slot", differing only in the attribute
/// byte and what `size` means (a real byte count for a file, always 0
/// for a directory, whose real extent is implied by walking its chain
/// instead).
fn build_dir_entry(
    short_name: &[u8; 11],
    attr: u8,
    cluster: u32,
    size: u32,
    fat_time: u16,
    fat_date: u16,
) -> [u8; 32] {
    let mut entry = [0u8; 32];
    entry[0..11].copy_from_slice(short_name);
    entry[11] = attr;
    entry[14..16].copy_from_slice(&fat_time.to_le_bytes()); // creation time
    entry[16..18].copy_from_slice(&fat_date.to_le_bytes()); // creation date
    entry[18..20].copy_from_slice(&fat_date.to_le_bytes()); // last access date
    entry[20..22].copy_from_slice(&((cluster >> 16) as u16).to_le_bytes()); // high cluster word - always 0, FAT12 clusters fit in 12 bits
    entry[22..24].copy_from_slice(&fat_time.to_le_bytes()); // write time
    entry[24..26].copy_from_slice(&fat_date.to_le_bytes()); // write date
    entry[26..28].copy_from_slice(&(cluster as u16).to_le_bytes());
    entry[28..32].copy_from_slice(&size.to_le_bytes());
    entry
}
