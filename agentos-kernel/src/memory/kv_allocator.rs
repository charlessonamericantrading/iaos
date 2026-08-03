use alloc::boxed::Box;
use alloc::vec;
use lazy_static::lazy_static;
use spin::Mutex;

/// Bytes allocated per KV cache block. Kept small (not a realistic
/// token-cache size) because it comes out of the kernel's current 1 MiB
/// heap (see memory/heap.rs, raised from an original 100 KiB once a real
/// file read proved that too small) - a real model-serving KV cache
/// would need a much larger heap still. `token_capacity` is still
/// recorded on each block separately for whenever that distinction
/// starts to matter.
pub const KV_CACHE_PAGE_SIZE: usize = 4096;

#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KVCacheLocation {
    Vram,
    SystemRam,
    PagedDisk,
}

#[allow(dead_code)]
pub struct KVCacheBlock {
    pub block_id: u32,
    pub pid: u32,
    pub token_capacity: usize,
    pub location: KVCacheLocation,
    data: Box<[u8]>,
}

impl KVCacheBlock {
    /// Real heap address backing this block. Previously this was
    /// `0x2000000 + idx*PAGE_SIZE`, a number that *looked* like a physical
    /// address but was never actually allocated or mapped anywhere -
    /// reading or writing through it would have page-faulted.
    pub fn addr(&self) -> usize {
        self.data.as_ptr() as usize
    }

    pub fn size_bytes(&self) -> usize {
        self.data.len()
    }
}

pub struct KVCacheMemoryManager {
    blocks: [Option<KVCacheBlock>; 128],
    allocated_count: usize,
}

impl KVCacheMemoryManager {
    pub const fn new() -> Self {
        const EMPTY_BLOCK: Option<KVCacheBlock> = None;
        KVCacheMemoryManager {
            blocks: [EMPTY_BLOCK; 128],
            allocated_count: 0,
        }
    }

    pub fn allocate_kv_block(&mut self, pid: u32, tokens: usize) -> Option<u32> {
        let idx = self.blocks.iter().position(|slot| slot.is_none())?;
        let block_id = idx as u32 + 1;
        let data = vec![0u8; KV_CACHE_PAGE_SIZE].into_boxed_slice();
        self.blocks[idx] = Some(KVCacheBlock {
            block_id,
            pid,
            token_capacity: tokens,
            location: KVCacheLocation::SystemRam,
            data,
        });
        self.allocated_count += 1;
        Some(block_id)
    }

    /// Returns the freed block's own owning `pid` on success, so a caller
    /// can reconcile that process's own bookkeeping (see Fase 162's
    /// `SYS_KV_FREE` syscall arm) - `None` if `block_id` was never
    /// allocated or was already freed.
    ///
    /// Fase 163: closes the 14th Explore-agent survey's candidate #1 -
    /// `block_id` is a raw, unvalidated syscall argument a ring-3 caller
    /// controls directly, and real block ids start at 1, so `block_id ==
    /// 0` is at least as plausible a caller mistake as the `9999` this
    /// function's own existing self-test already covers. `checked_sub`
    /// rejects it the same clean way an out-of-range id already was,
    /// instead of relying on the plain `block_id - 1` this used to be:
    /// harmless today only because this project's release builds run
    /// with `overflow-checks` off (so it silently wrapped to `u32::MAX`,
    /// which then correctly failed the bounds check below anyway) - but
    /// an explicit debug build, or `overflow-checks = true` in
    /// `[profile.release]`, would have turned the exact same call into a
    /// genuine panic, and this kernel's own panic handler is an
    /// unconditional `loop { hlt() }` - the same "clean rejection
    /// expected, permanent hang possible instead" class Fase 132/159
    /// already closed elsewhere, just via raw arithmetic on an id here
    /// instead of a buffer length.
    pub fn free_kv_block(&mut self, block_id: u32) -> Option<u32> {
        let idx = block_id.checked_sub(1)? as usize;
        if idx < self.blocks.len() {
            if let Some(block) = self.blocks[idx].take() {
                self.allocated_count -= 1;
                return Some(block.pid); // drops block, incl. its Box<[u8]> - really frees the heap memory
            }
        }
        None
    }

    pub fn get_allocated_count(&self) -> usize {
        self.allocated_count
    }

    /// Read-only iteration for diagnostics (the `mem` shell command) -
    /// mirrors `NativeAgentScheduler::for_each_process`.
    pub fn for_each_block<F: FnMut(&KVCacheBlock)>(&self, mut f: F) {
        for slot in self.blocks.iter().flatten() {
            f(slot);
        }
    }
}

lazy_static! {
    pub static ref KV_MANAGER: Mutex<KVCacheMemoryManager> =
        Mutex::new(KVCacheMemoryManager::new());
}
