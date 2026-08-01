//! A single, dedicated user-accessible page - Fase 70, the third step of
//! the ring-3 transition arc (after Fase 68's GDT/TSS foundation and
//! Fase 69's DPL=3 `int 0x80` gate). Ring-3 code cannot legally read,
//! write, or execute ANY memory this kernel has mapped so far: every
//! existing mapping (the heap, in particular) was created via plain
//! `PRESENT | WRITABLE` flags, with `USER_ACCESSIBLE` never set - by
//! design, not oversight, since none of that memory was ever meant to
//! be reachable from a lower privilege level. This module maps one new,
//! deliberately separate page with that bit genuinely set, the
//! prerequisite a future ring-3 program needs before it can do
//! anything at all once the real `iretq` transition is attempted.
//!
//! A real, easy-to-miss correctness subtlety worth naming explicitly:
//! the `USER_ACCESSIBLE` bit must be set at EVERY level of the page-table
//! walk (PML4, PDPT, PD, and the final PT entry), not just the leaf -
//! x86_64 ANDs the effective permission across all four levels, so a
//! single non-user-accessible entry anywhere in the chain silently
//! blocks ring-3 access regardless of what the leaf PTE says. Confirmed,
//! not assumed, that this is handled correctly here: the `x86_64` crate's
//! own `Mapper::map_to` derives `parent_table_flags` automatically from
//! the leaf flags passed in (`flags & (PRESENT | WRITABLE |
//! USER_ACCESSIBLE)`, per its own source) whenever it has to CREATE a
//! new intermediate table level - and the chosen virtual address
//! (`0x5555_5555_0000`) was checked against the heap's own range
//! (`0x4444_4444_0000..+1MiB`) at every page-table level before writing
//! any code: PML4 index 170 vs. 136 - completely different top-level
//! slots, so this mapping shares NO intermediate table with the heap's
//! own (which would otherwise keep its original, non-user-accessible
//! flags unchanged, silently defeating this page's own USER_ACCESSIBLE
//! bit).

use x86_64::structures::paging::mapper::{MapToError, TranslateResult};
use x86_64::structures::paging::{
    FrameAllocator, Mapper, OffsetPageTable, Page, PageTableFlags, Size4KiB, Translate,
};
use x86_64::VirtAddr;

pub const USER_TEST_PAGE_ADDR: u64 = 0x5555_5555_0000;

/// Deliberately distinct from the heap's own `HEAP_START` (`0x4444_
/// 4444_0000`) - see this module's own doc for why the specific choice
/// matters (no shared intermediate page-table entries).
pub fn map_user_test_page(
    mapper: &mut OffsetPageTable<'static>,
    frame_allocator: &mut impl FrameAllocator<Size4KiB>,
) -> Result<(), MapToError<Size4KiB>> {
    let page = Page::containing_address(VirtAddr::new(USER_TEST_PAGE_ADDR));
    let frame = frame_allocator
        .allocate_frame()
        .ok_or(MapToError::FrameAllocationFailed)?;
    let flags =
        PageTableFlags::PRESENT | PageTableFlags::WRITABLE | PageTableFlags::USER_ACCESSIBLE;
    unsafe {
        mapper.map_to(page, frame, flags, frame_allocator)?.flush();
    }
    Ok(())
}

pub struct UserPageInfo {
    pub present: bool,
    pub writable: bool,
    pub user_accessible: bool,
    pub write_read_back_ok: bool,
}

/// Verifies the mapping two independent ways, not just one. First, a
/// structural check: reads the REAL leaf PTE's own flags back through
/// the page tables (via `Translate`, not trusting `map_to`'s own
/// `Ok(())` return value alone). Second, a functional check: writes a
/// real, distinctive byte pattern into the page and reads it back -
/// ring-0 can access this page freely, since `USER_ACCESSIBLE` only
/// ever restricts ring-3, never ring-0 - proving the mapping backs
/// genuinely usable memory, not just that its flags look right in
/// isolation.
pub fn inspect_user_test_page(mapper: &OffsetPageTable<'static>) -> UserPageInfo {
    let addr = VirtAddr::new(USER_TEST_PAGE_ADDR);

    let (present, writable, user_accessible) = match mapper.translate(addr) {
        TranslateResult::Mapped { flags, .. } => (
            flags.contains(PageTableFlags::PRESENT),
            flags.contains(PageTableFlags::WRITABLE),
            flags.contains(PageTableFlags::USER_ACCESSIBLE),
        ),
        _ => (false, false, false),
    };

    const PATTERN: [u8; 8] = [0xDE, 0xAD, 0xBE, 0xEF, 0xCA, 0xFE, 0xBA, 0xBE];
    let ptr = addr.as_mut_ptr::<[u8; 8]>();
    let write_read_back_ok = unsafe {
        core::ptr::write_volatile(ptr, PATTERN);
        core::ptr::read_volatile(ptr) == PATTERN
    };

    UserPageInfo {
        present,
        writable,
        user_accessible,
        write_read_back_ok,
    }
}
