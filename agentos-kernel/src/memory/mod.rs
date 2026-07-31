pub mod frame_allocator;
pub mod kv_allocator;
pub mod heap;

use x86_64::registers::control::Cr3;
use x86_64::structures::paging::{OffsetPageTable, PageTable};
use x86_64::VirtAddr;

/// Builds a `Mapper` over the kernel's *currently active* level-4 page
/// table, so `heap::init_heap` can install new mappings into it.
///
/// # Safety
/// The complete physical memory must already be identity-mapped at
/// `physical_memory_offset` (guaranteed here by `BOOTLOADER_CONFIG` in
/// main.rs setting `mappings.physical_memory = Mapping::Dynamic`), and the
/// caller must call this at most once - a second call would produce a
/// second `&mut PageTable` aliasing the same memory.
pub unsafe fn init(physical_memory_offset: VirtAddr) -> OffsetPageTable<'static> {
    let level_4_table = active_level_4_table(physical_memory_offset);
    OffsetPageTable::new(level_4_table, physical_memory_offset)
}

unsafe fn active_level_4_table(physical_memory_offset: VirtAddr) -> &'static mut PageTable {
    let (level_4_frame, _) = Cr3::read();
    let phys = level_4_frame.start_address();
    let virt = physical_memory_offset + phys.as_u64();
    let page_table_ptr: *mut PageTable = virt.as_mut_ptr();
    &mut *page_table_ptr
}
