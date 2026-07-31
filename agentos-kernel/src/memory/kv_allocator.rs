use spin::Mutex;
use lazy_static::lazy_static;

pub const KV_CACHE_PAGE_SIZE: usize = 4096 * 16;

#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KVCacheLocation {
    Vram,
    SystemRam,
    PagedDisk,
}

#[allow(dead_code)]
#[derive(Debug)]
pub struct KVCacheBlock {
    pub block_id: u32,
    pub pid: u32,
    pub token_capacity: usize,
    pub location: KVCacheLocation,
    pub phys_addr: u64,
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
        for (idx, slot) in self.blocks.iter_mut().enumerate() {
            if slot.is_none() {
                let block_id = idx as u32 + 1;
                *slot = Some(KVCacheBlock {
                    block_id,
                    pid,
                    token_capacity: tokens,
                    location: KVCacheLocation::Vram,
                    phys_addr: 0x2000000 + (idx * KV_CACHE_PAGE_SIZE) as u64,
                });
                self.allocated_count += 1;
                return Some(block_id);
            }
        }
        None
    }

    #[allow(dead_code)]
    pub fn free_kv_block(&mut self, block_id: u32) -> bool {
        let idx = (block_id - 1) as usize;
        if idx < self.blocks.len() && self.blocks[idx].is_some() {
            self.blocks[idx] = None;
            self.allocated_count -= 1;
            true
        } else {
            false
        }
    }

    #[allow(dead_code)]
    pub fn get_allocated_count(&self) -> usize {
        self.allocated_count
    }
}

lazy_static! {
    pub static ref KV_MANAGER: Mutex<KVCacheMemoryManager> = Mutex::new(KVCacheMemoryManager::new());
}
