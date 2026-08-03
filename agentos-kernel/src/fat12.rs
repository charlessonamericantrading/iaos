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

    /// Fase 169: exposes whether `cluster`'s own FAT entry currently reads
    /// back as free (0), via `next_cluster`'s own error branch for that
    /// exact case. Added purely so a self-test can directly prove a
    /// specific cluster was really freed by `delete_file`/`delete_directory`.
    /// A still-allocated end-of-chain cluster (entry `>= 0x0FF8`) reads
    /// `false` here, not `true`, so this genuinely distinguishes "freed"
    /// from "still holds a real, if empty, file's last cluster".
    pub fn cluster_is_free(&self, cluster: u32) -> bool {
        self.next_cluster(cluster).is_err()
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
        let mut lfn = fat_common::LfnState::default();
        for s in 0..self.root_dir_sectors() {
            ata::read_sector(self.first_root_dir_sector() + s, &mut sector_buf)?;
            if fat_common::parse_dir_sector(&sector_buf, &mut entries, &mut lfn) {
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
        // find_entry_location_in (not list_entries_in) so a VFAT entry
        // matches by *either* its long name or its generated short
        // alias, same as delete_file/write_file already do - using
        // list_entries_in here instead would only ever see whichever one
        // name parse_dir_sector chose to reconstruct, silently making
        // the alias unreadable. An entry matching by name that turns out
        // to be a directory is treated as not found, same as before this
        // used find_entry_location_in - it never distinguished "wrong
        // type" from "no such name" either.
        let (_, _, entry) = self.find_entry_location_in(dir, name)?;
        if entry.is_dir {
            let not_found = match dir {
                DirLocation::Root => "FAT12: file not found in root directory",
                DirLocation::Cluster(_) => "FAT12: file not found in directory",
            };
            return Err(not_found);
        }

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

    /// Finds one free slot in a directory - a thin wrapper over
    /// `find_free_entry_run_in` for the common single-slot case (every
    /// name that fits the plain 8.3 short-name format only ever needs
    /// one). See that function's doc for the real logic.
    fn find_free_entry_in(&mut self, dir: DirLocation) -> Result<(u32, usize), &'static str> {
        Ok(self.find_free_entry_run_in(dir, 1)?[0])
    }

    /// Finds `count` *consecutive* free slots in a directory, all within
    /// one sector - either truly never-used entries (first byte `0x00`,
    /// meaning everything from here to the end of the directory is
    /// unused too, per the FAT spec) or previously-deleted ones (`0xE5`,
    /// safe to reuse). Returns each slot's absolute sector LBA and byte
    /// offset within that sector, in order. `count` is 1 (a plain
    /// short-name entry) or `N + 1` for an `N`-chunk VFAT name (`N` long
    /// entries plus the short entry they describe - see
    /// `build_name_entries`).
    ///
    /// Restricting a run to a single sector (never straddling into the
    /// next one) is a deliberate simplification: a sector holds
    /// `ENTRIES_PER_SECTOR` (16) entries, so a run bumping into a
    /// sector's last slot simply continues the search into the next
    /// sector instead of splitting across both - avoiding the extra
    /// complexity of a multi-sector write. `count` can therefore never
    /// exceed 16 here; `build_name_entries` caps a VFAT name at 15
    /// chunks (195 characters) specifically so `count` (chunks + 1)
    /// never does either - checked explicitly below rather than left as
    /// an implicit assumption, since silently exceeding it would compute
    /// a byte offset past a single 512-byte `sector_buf` in the
    /// `grow_directory` fallback below (a real, found-by-reasoning-
    /// through-this-function's-own-limits gap, not a hypothetical one).
    ///
    /// The root can never grow - it's a fixed-size range dictated by
    /// the BPB itself, not a cluster chain - so a full root is a real,
    /// permanent error. A subdirectory's chain, though, is exactly as
    /// extensible as a file's: `grow_directory` below appends a fresh
    /// cluster the same way `create_file`/`write_file` already do,
    /// rather than surfacing an avoidable "directory is full" error the
    /// moment its one starting cluster's entries run out - and a freshly
    /// zeroed cluster trivially contains a run of any `count` up to a
    /// whole sector's worth, starting right at its first slot.
    fn find_free_entry_run_in(
        &mut self,
        dir: DirLocation,
        count: usize,
    ) -> Result<Vec<(u32, usize)>, &'static str> {
        const ENTRIES_PER_SECTOR: usize = 512 / 32;
        if count > ENTRIES_PER_SECTOR {
            return Err("FAT12: cannot reserve that many consecutive directory-entry slots within one sector");
        }
        let mut sector_buf = [0u8; 512];
        for lba in self.directory_sectors(dir)? {
            ata::read_sector(lba, &mut sector_buf)?;
            let mut run_start: Option<usize> = None;
            for (i, raw) in sector_buf.as_chunks::<32>().0.iter().enumerate() {
                if raw[0] == 0x00 || raw[0] == 0xE5 {
                    let start = *run_start.get_or_insert(i);
                    if i - start + 1 == count {
                        return Ok((start..=i).map(|slot| (lba, slot * 32)).collect());
                    }
                } else {
                    run_start = None;
                }
            }
        }
        match dir {
            DirLocation::Root => Err("FAT12: root directory is full, no free entry slot"),
            DirLocation::Cluster(start_cluster) => {
                let (lba, offset) = self.grow_directory(start_cluster)?;
                Ok((0..count).map(|slot| (lba, offset + slot * 32)).collect())
            }
        }
    }

    /// Appends one freshly-allocated, zeroed cluster to the end of a
    /// directory's existing chain and returns a free slot in it - the
    /// first entry of a brand-new, all-zero cluster is always free by
    /// construction (byte `0x00`, "no more entries used from here on",
    /// same convention `create_directory`'s own zeroing already relies
    /// on). Reuses the exact same allocate-then-chain primitives
    /// `create_file`/`write_file` already use for a *file's* chain -
    /// growing a directory's chain isn't a fundamentally different
    /// operation, just applied to entries instead of file bytes.
    fn grow_directory(&mut self, start_cluster: u32) -> Result<(u32, usize), &'static str> {
        let mut tail = start_cluster;
        while let Some(next) = self.next_cluster(tail)? {
            tail = next;
        }

        let new_cluster = self.find_free_clusters(1)?[0];
        let new_lba = self.cluster_to_lba(new_cluster);
        for s in 0..self.sectors_per_cluster as u32 {
            ata::write_sector(new_lba + s, &[0u8; 512])?;
        }

        self.write_fat_entry_to_disk(tail, new_cluster as u16)?;
        self.write_fat_entry_to_disk(new_cluster, 0x0FFF)?;

        Ok((new_lba, 0))
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

    /// Creates a new file in the root directory with `name` and `data`
    /// as its content - genuinely new, not an overwrite of something
    /// that already exists. Handles files of any size, chaining as many
    /// clusters together as needed (originally limited to a single
    /// cluster - see the multi-cluster note on `find_free_clusters` for
    /// why that needed its own design pass: allocating several free
    /// clusters safely means finding them all in one pass, not calling a
    /// "find one free cluster" function several times, which would hand
    /// out the same cluster repeatedly). `name` gets a plain short-name
    /// entry if it fits 8.3, or a generated short alias plus one VFAT
    /// long-name entry if it doesn't (see `build_name_entries`) - either
    /// way only the first on-disk FAT copy is updated, same reasoning as
    /// `write_fat_entry_to_disk`.
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

        let (short_name, long_entries) = self.build_name_entries(dir, name)?;
        let cluster_bytes = self.sectors_per_cluster as usize * 512;
        // find_free_entry_run_in MUST run before find_free_clusters
        // below, not after - found the hard way via the directory-growth
        // self-test. find_free_entry_run_in can grow the directory
        // itself (allocating and immediately committing a fresh cluster
        // to the FAT, via grow_directory -> write_fat_entry_to_disk). If
        // the file's own data cluster(s) were chosen first instead, that
        // choice would only be a candidate in the in-memory FAT cache -
        // find_free_clusters never commits anything itself, only
        // write_fat_entry_to_disk does - so grow_directory's own
        // find_free_clusters call could still see that "candidate"
        // cluster as free and hand out the exact same one, corrupting
        // whichever write happened second.
        let slots = self.find_free_entry_run_in(dir, long_entries.len() + 1)?;

        // At least one cluster even for an empty (0-byte) file, matching
        // this method's existing convention from before multi-cluster
        // support: every created file gets a real cluster to anchor its
        // directory entry's start_cluster field to.
        let clusters_needed = data.len().div_ceil(cluster_bytes).max(1);
        let clusters = self.find_free_clusters(clusters_needed)?;

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
        // zeroed/fake one - genuinely read back too, via
        // `fat_common::DirEntry`'s own `fat_time`/`fat_date` fields
        // (Fase 148), not just written and left unverified.
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
        self.write_name_entries(&slots, &long_entries, entry)?;

        Ok(())
    }

    /// Writes a short entry (and, if any, the VFAT long entries
    /// describing it) into the slots `find_free_entry_run_in` reserved -
    /// `long_entries` (already in correct on-disk order - highest
    /// sequence number, holding the end of the name, first) fill
    /// `slots[0..long_entries.len()]` in order, the short entry always
    /// the last slot (`slots.len() == long_entries.len() + 1` always, by
    /// construction from every caller). Reads and rewrites each slot's
    /// own sector independently rather than assuming they all share one
    /// (true today, since `find_free_entry_run_in` never returns a run
    /// spanning two sectors - but this way stays correct even if that
    /// restriction is ever lifted).
    fn write_name_entries(
        &mut self,
        slots: &[(u32, usize)],
        long_entries: &[[u8; 32]],
        short_entry: [u8; 32],
    ) -> Result<(), &'static str> {
        let mut writes = Vec::new();
        for (&(lba, offset), &entry) in slots.iter().zip(long_entries) {
            writes.push((lba, offset, entry));
        }
        let &(short_lba, short_offset) = slots.last().unwrap();
        writes.push((short_lba, short_offset, short_entry));

        for (lba, offset, bytes) in writes {
            let mut sector_buf = [0u8; 512];
            ata::read_sector(lba, &mut sector_buf)?;
            sector_buf[offset..offset + 32].copy_from_slice(&bytes);
            ata::write_sector(lba, &sector_buf)?;
        }
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
        self.create_directory_impl(DirLocation::Root, name)
    }

    /// Same as `create_directory`, but nested inside the subdirectory
    /// whose own cluster is `parent_cluster` instead of the root -
    /// directories more than one level deep.
    pub fn create_directory_in(
        &mut self,
        parent_cluster: u32,
        name: &str,
    ) -> Result<(), &'static str> {
        self.create_directory_impl(DirLocation::Cluster(parent_cluster), name)
    }

    fn create_directory_impl(
        &mut self,
        parent: DirLocation,
        name: &str,
    ) -> Result<(), &'static str> {
        let entries = self.list_entries_in(parent)?;
        if entries.iter().any(|e| e.name.eq_ignore_ascii_case(name)) {
            return Err("FAT12: a file or directory with that name already exists");
        }

        let (short_name, long_entries) = self.build_name_entries(parent, name)?;
        // find_free_entry_run_in before find_free_clusters - see the
        // matching comment in create_file_impl for why the order
        // matters (found via the same directory-growth self-test):
        // find_free_entry_run_in can grow `parent` itself, immediately
        // committing a fresh cluster to the FAT - if this new
        // directory's own cluster were chosen first instead, that
        // choice wouldn't be committed yet, and find_free_entry_run_in's
        // own growth could still see it as free and collide with it.
        let slots = self.find_free_entry_run_in(parent, long_entries.len() + 1)?;
        let cluster = self.find_free_clusters(1)?[0];

        let time = crate::rtc::read_time();
        let (fat_time, fat_date) = to_fat_datetime(&time);

        let mut dot_name = [b' '; 11];
        dot_name[0] = b'.';
        let mut dotdot_name = [b' '; 11];
        dotdot_name[0] = b'.';
        dotdot_name[1] = b'.';
        // ".." points to the parent's own cluster - 0 is the real FAT
        // convention specifically for "the parent is the root
        // directory" (root has no cluster number of its own); a nested
        // directory's parent is a genuine cluster instead.
        let parent_cluster = match parent {
            DirLocation::Root => 0,
            DirLocation::Cluster(c) => c,
        };
        let dot_entry = build_dir_entry(&dot_name, 0x10, cluster, 0, fat_time, fat_date);
        let dotdot_entry =
            build_dir_entry(&dotdot_name, 0x10, parent_cluster, 0, fat_time, fat_date);

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
        self.write_name_entries(&slots, &long_entries, dir_entry)?;

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
        let mut lfn = fat_common::LfnState::default();

        while let Some(c) = cluster {
            let cluster_lba = self.cluster_to_lba(c);
            for s in 0..self.sectors_per_cluster as u32 {
                ata::read_sector(cluster_lba + s, &mut sector_buf)?;
                if fat_common::parse_dir_sector(&sector_buf, &mut entries, &mut lfn) {
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
    /// no indication of which raw 32-byte slot each one came from - but
    /// shares its actual VFAT reconstruction (`LfnState`), so `name` is
    /// matched against a VFAT entry's real long name exactly the same
    /// way `ls` displays it, not just its generated short alias.
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
        let mut lfn = fat_common::LfnState::default();
        for lba in self.directory_sectors(dir)? {
            ata::read_sector(lba, &mut sector_buf)?;
            for (i, raw) in sector_buf.as_chunks::<32>().0.iter().enumerate() {
                if raw[0] == 0x00 {
                    return Err(not_found);
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

                // Fase 139: matching logic (checking both the long AND
                // short name) moved into the shared
                // `fat_common::short_entry_if_matches` - `fat32.rs`'s own
                // `find_entry` now uses the exact same helper instead of
                // hand-duplicating this again.
                if let Some(entry) = fat_common::short_entry_if_matches(raw, &mut lfn, name) {
                    return Ok((lba, i * 32, entry));
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

        // Fase 169: no `entry.size > 0` guard here - create_file_impl always
        // allocates at least one real cluster to anchor start_cluster, even
        // for a 0-byte file (see its own "at least one cluster even for an
        // empty file" comment), so a 0-byte file's chain still needs freeing.
        // Matches delete_directory_impl's own unconditional free below,
        // which never had this gap since every directory always has a real
        // cluster for its `.`/`..` entries.
        let mut cluster = Some(entry.start_cluster);
        while let Some(c) = cluster {
            // Capture the next link before zeroing this entry - once
            // it's zeroed, the chain onward from here is lost.
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

    /// Deletes an EMPTY subdirectory: frees its cluster chain and marks
    /// its root-directory entry deleted (`0xE5`) - same mechanics as
    /// `delete_file`, since freeing a chain and marking an entry deleted
    /// doesn't care whether the chain held file bytes or directory
    /// entries. Refuses to delete a non-empty directory (anything beyond
    /// the `.`/`..` entries every directory `create_directory` writes) -
    /// the standard, safe `rmdir` semantic.
    pub fn delete_directory(&mut self, name: &str) -> Result<(), &'static str> {
        self.delete_directory_impl(DirLocation::Root, name)
    }

    /// Same as `delete_directory`, but for a directory nested inside the
    /// subdirectory whose own cluster is `parent_cluster` instead of the
    /// root.
    pub fn delete_directory_in(
        &mut self,
        parent_cluster: u32,
        name: &str,
    ) -> Result<(), &'static str> {
        self.delete_directory_impl(DirLocation::Cluster(parent_cluster), name)
    }

    fn delete_directory_impl(
        &mut self,
        parent: DirLocation,
        name: &str,
    ) -> Result<(), &'static str> {
        let (entry_lba, entry_offset, entry) = self.find_entry_location_in(parent, name)?;
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

    /// Decides how to represent `name` on disk. If it already fits the
    /// plain 8.3 short-name format, no VFAT entries are needed at all -
    /// the same simpler path every name used before VFAT support existed,
    /// returned here as an empty `Vec` for the long-entries half -
    /// *except* that path also has to guard against a real collision
    /// `to_short_name` alone can't see: a plain 8.3 name typed directly
    /// might be byte-for-byte identical to some *other* entry's
    /// already-generated VFAT alias (`generate_short_alias` only ever
    /// checks collisions when *it* picks one, not when a caller bypasses
    /// it entirely with a name that already fits). Found by reasoning
    /// through what an earlier Fase's self-test would do with an alias
    /// as a plain input, not by a failing test - left unfixed, two
    /// directory entries could end up sharing one short name, corrupting
    /// whichever lookup finds the wrong one first.
    ///
    /// Otherwise, if `name` is short enough (see the cap below) and
    /// every character is valid, generates a short alias plus as many
    /// long entries as the name needs (already collision-checked
    /// internally, see `generate_short_alias`). Capped at 195 characters
    /// (15 long entries of 13 characters each) - not an arbitrary
    /// number, but the largest that still keeps a new entry's required
    /// slots (long entries plus the one short entry, see
    /// `find_free_entry_run_in`) within its own single-sector limit (16
    /// entries). A name over that cap gets an honest "doesn't fit"
    /// error rather than a silently truncated one; lifting it would mean
    /// lifting the one-sector restriction too, letting a slot run span
    /// sectors - real, additional complexity not attempted yet.
    fn build_name_entries(
        &self,
        dir: DirLocation,
        name: &str,
    ) -> Result<([u8; 11], alloc::vec::Vec<[u8; 32]>), &'static str> {
        if let Ok(short_name) = to_short_name(name) {
            if self.existing_short_names_in(dir)?.contains(&short_name) {
                return Err("FAT12: that short name is already in use");
            }
            return Ok((short_name, alloc::vec::Vec::new()));
        }
        const MAX_LFN_CHARS: usize = 15 * 13;
        if name.is_empty() || name.len() > MAX_LFN_CHARS || !name.bytes().all(is_valid_lfn_char) {
            return Err(
                "FAT12: name must fit the 8.3 short-name format, or be a valid name of 195 characters or fewer",
            );
        }
        let short_name = self.generate_short_alias(dir, name)?;
        let long_entries = build_lfn_entries(name, &short_name);
        Ok((short_name, long_entries))
    }

    /// Generates a short 8.3 "alias" for a long name that doesn't fit
    /// 8.3 itself, following the same `BASE~N.EXT` convention real VFAT
    /// uses: up to 5 valid characters of the base name, uppercased, then
    /// `~1` (incrementing on collision against every other short name
    /// already in `dir`), then up to 3 characters of the extension.
    /// Capped at `~99`: pairing a fixed 5-character prefix with up to 2
    /// digits always fits within 8 bytes without the variable-width
    /// prefix-trimming real Windows does to support much larger N - a
    /// directory in this kernel colliding 99 times over on the same
    /// 5-character prefix is not a real scenario worth that extra
    /// complexity.
    fn generate_short_alias(&self, dir: DirLocation, name: &str) -> Result<[u8; 11], &'static str> {
        let (base, ext) = match name.split_once('.') {
            Some((b, e)) => (b, e),
            None => (name, ""),
        };
        let mut clean_base: Vec<u8> = base
            .bytes()
            .map(|b| b.to_ascii_uppercase())
            .filter(|&b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
            .collect();
        if clean_base.is_empty() {
            clean_base.push(b'_');
        }
        clean_base.truncate(5);

        let mut clean_ext: Vec<u8> = ext
            .bytes()
            .map(|b| b.to_ascii_uppercase())
            .filter(|&b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
            .collect();
        clean_ext.truncate(3);

        let existing = self.existing_short_names_in(dir)?;
        for n in 1..=99u32 {
            let mut out = [b' '; 11];
            let suffix = alloc::format!("~{}", n);
            out[..clean_base.len()].copy_from_slice(&clean_base);
            out[clean_base.len()..clean_base.len() + suffix.len()]
                .copy_from_slice(suffix.as_bytes());
            out[8..8 + clean_ext.len()].copy_from_slice(&clean_ext);

            if !existing.contains(&out) {
                return Ok(out);
            }
        }
        Err("FAT12: too many colliding short-name aliases in this directory")
    }

    /// Collects the raw 11-byte short-name field of every real
    /// (non-deleted, non-VFAT, non-volume-label) entry in `dir` - used
    /// only to check a freshly-generated short alias for collisions.
    /// Deliberately not `list_entries_in`/`DirEntry`: once
    /// `parse_dir_sector` can reconstruct one, a VFAT entry's long name
    /// is no longer the same string as what's actually stored in its
    /// short-name field, which is specifically what a new alias must
    /// avoid colliding with.
    fn existing_short_names_in(&self, dir: DirLocation) -> Result<Vec<[u8; 11]>, &'static str> {
        let mut names = Vec::new();
        let mut sector_buf = [0u8; 512];
        for lba in self.directory_sectors(dir)? {
            ata::read_sector(lba, &mut sector_buf)?;
            for raw in sector_buf.as_chunks::<32>().0 {
                if raw[0] == 0x00 {
                    return Ok(names);
                }
                if raw[0] == 0xE5 || raw[11] == 0x0F || raw[11] & 0x08 != 0 {
                    continue;
                }
                let mut short = [0u8; 11];
                short.copy_from_slice(&raw[0..11]);
                names.push(short);
            }
        }
        Ok(names)
    }
}

/// Converts a "NAME" or "NAME.EXT" string into the padded 8.3 short-name
/// format FAT directory entries use on disk (11 bytes: 8 for the name,
/// space-padded, then 3 for the extension, space-padded). Uppercases
/// everything, matching FAT short-name convention. Anything not fitting
/// 8.3 is rejected here rather than silently truncated - `build_name_
/// entries` (the actual entry point every create path uses) is what
/// falls back to a generated alias plus a VFAT long entry when this
/// fails, not this function.
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

/// Whether a byte is safe to use in a VFAT long name: printable ASCII,
/// excluding the characters reserved across every real filesystem's
/// long-name rules (path separators and other shell/OS-meaningful
/// punctuation). Non-ASCII is out of scope here, not because the format
/// forbids it (real VFAT is UTF-16 and supports it fully) but because
/// this kernel's own keyboard driver can't produce it - `build_lfn_entries`
/// widens each accepted byte 1:1 into a UTF-16 code unit, which is exact
/// for everything this check lets through.
fn is_valid_lfn_char(b: u8) -> bool {
    matches!(b, 0x20..=0x7E)
        && !matches!(
            b,
            b'"' | b'*' | b'/' | b':' | b'<' | b'>' | b'?' | b'\\' | b'|'
        )
}

/// Builds the sequence of 32-byte VFAT long-name entries for `name` (up
/// to `build_name_entries`'s 195-character cap - 15 chunks of 13 UTF-16
/// code units each), in on-disk order: the entry holding the *end* of
/// the name first (sequence number ORed with `0x40`), counting down to
/// sequence `1` last, immediately preceding the short entry all of them
/// describe. A `name` of 13 characters or fewer reduces to exactly one
/// entry with sequence `0x41` - the same single-entry case this function
/// originally handled alone, verified unchanged by construction rather
/// than by special-casing it. Only the *last* chunk in string order
/// (first on disk) can be shorter than 13 characters - every other chunk
/// is always a full 13, since only the final piece of a fixed-13-wide
/// split can be partial - padded per spec: a `0x0000` terminator right
/// after its real characters, then `0xFFFF` for every slot after that.
fn build_lfn_entries(name: &str, short_name: &[u8; 11]) -> alloc::vec::Vec<[u8; 32]> {
    let bytes: alloc::vec::Vec<u8> = name.bytes().collect();
    let num_chunks = bytes.len().div_ceil(13).max(1);
    let checksum = fat_common::lfn_checksum(short_name);

    let mut entries = alloc::vec::Vec::with_capacity(num_chunks);
    for chunk_index in (0..num_chunks).rev() {
        let start = chunk_index * 13;
        let end = (start + 13).min(bytes.len());
        let chunk_bytes = &bytes[start..end];

        let mut chars = [0xFFFFu16; 13];
        for (i, &b) in chunk_bytes.iter().enumerate() {
            chars[i] = b as u16;
        }
        if chunk_bytes.len() < 13 {
            chars[chunk_bytes.len()] = 0x0000;
        }

        let mut sequence = (chunk_index + 1) as u8;
        if chunk_index == num_chunks - 1 {
            sequence |= 0x40; // last chunk in string order = first on disk
        }

        let mut entry = [0u8; 32];
        entry[0] = sequence;
        for (i, c) in chars[0..5].iter().enumerate() {
            entry[1 + i * 2..3 + i * 2].copy_from_slice(&c.to_le_bytes());
        }
        entry[11] = 0x0F; // ATTR_LONG_NAME
        entry[12] = 0x00;
        entry[13] = checksum;
        for (i, c) in chars[5..11].iter().enumerate() {
            entry[14 + i * 2..16 + i * 2].copy_from_slice(&c.to_le_bytes());
        }
        // Bytes 26-27 (the "first cluster" field on a real short entry)
        // are always 0 on a long entry - already the case, `entry`
        // starts zeroed and nothing above ever touches this range.
        for (i, c) in chars[11..13].iter().enumerate() {
            entry[28 + i * 2..30 + i * 2].copy_from_slice(&c.to_le_bytes());
        }
        entries.push(entry);
    }
    entries
}

/// Packs a `RtcTime` into FAT's on-disk time/date u16 formats: time is
/// hours(5 bits):minutes(6 bits):seconds/2(5 bits); date is (year-1980)
/// (7 bits):month(4 bits):day(5 bits). `pub` (rather than private) so a
/// self-test can compute the exact expected value independently, instead
/// of just trusting that whatever got written reads back as itself.
pub fn to_fat_datetime(t: &crate::rtc::RtcTime) -> (u16, u16) {
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
