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
//!
//! **Fase 99 generalizes the mapping/inspection logic to an arbitrary
//! address** (`map_user_page_at`/`inspect_user_page_at`), with
//! `map_user_test_page`/`inspect_user_test_page` becoming thin wrappers
//! over `USER_TEST_PAGE_ADDR` so every existing call site (every ring-3
//! test since Fase 70) stays byte-identical. This exists to give
//! `USER_DISK_PROGRAM_PAGE_ADDR` (`0x6666_6666_0000`) - the page Fase
//! 98's disk-loaded ring-3 program now runs on instead of sharing
//! `USER_TEST_PAGE_ADDR` with every other test - the same PML4-distinctness
//! guarantee: index 204, vs. the heap's 136 and the test page's own 170,
//! so it shares no intermediate table with either existing mapping.
//!
//! **Fase 100 adds a FOURTH mapped address, `USER_DISK_PROGRAM_STACK_ADDR`**
//! (PML4 index 238, distinct from all three above) - a dedicated stack
//! page for the same disk-loaded program, so its code and stack no
//! longer share a page's tail the way every ring-3 test before it did.
//!
//! **Fase 103 adds a FIFTH mapped address, `USER_DISK_PROGRAM_DATA_ADDR`**
//! (PML4 index 102, distinct from all four above) - a real DATA segment
//! for the same disk-loaded program, so it can write and be verified to
//! have written somewhere that is neither its own code nor its own stack.

use x86_64::structures::paging::mapper::{MapToError, TranslateResult};
use x86_64::structures::paging::{
    FrameAllocator, Mapper, OffsetPageTable, Page, PageTableFlags, Size4KiB, Translate,
};
use x86_64::VirtAddr;

pub const USER_TEST_PAGE_ADDR: u64 = 0x5555_5555_0000;

/// A second, independent user page - Fase 99 - reserved for ring-3
/// programs that need their own memory instead of sharing
/// `USER_TEST_PAGE_ADDR` with every other test. PML4 index 204 (checked
/// the same way this module's own doc checks `USER_TEST_PAGE_ADDR`
/// against the heap): distinct from both the heap's 136 and
/// `USER_TEST_PAGE_ADDR`'s own 170, so mapping this address creates
/// entirely fresh intermediate page-table levels, never reusing (and
/// thus never risking silently inheriting the flags of) either existing
/// mapping's tables.
pub const USER_DISK_PROGRAM_PAGE_ADDR: u64 = 0x6666_6666_0000;

/// A THIRD independent page - Fase 100 - a dedicated stack for the
/// disk-loaded program, separate from its own code page above. PML4
/// index 238: distinct from the heap's 136, `USER_TEST_PAGE_ADDR`'s 170,
/// and `USER_DISK_PROGRAM_PAGE_ADDR`'s own 204. Exists because every
/// ring-3 test in this kernel's entire history (Fase 71 through 99) has
/// set `stack_top` to `code_addr + 4096` - the tail of the SAME page as
/// the code - so the `iretq`-based ring-3 transition has never actually
/// been proven to work when code and stack live on genuinely separate,
/// non-adjacent pages.
pub const USER_DISK_PROGRAM_STACK_ADDR: u64 = 0x7777_7777_0000;

/// A FOURTH independent page - Fase 103 - a real DATA segment for the
/// disk-loaded program, distinct from its own code and stack pages
/// above. PML4 index 102: distinct from the heap's 136, `USER_TEST_
/// PAGE_ADDR`'s 170, `USER_DISK_PROGRAM_PAGE_ADDR`'s 204, and `USER_
/// DISK_PROGRAM_STACK_ADDR`'s 238. Deliberately a SMALLER address than
/// all four: `0x3333_3333_0000` continues this module's own established
/// "one repeated hex digit, times four" naming, but the naive next step
/// in that sequence (`0x8888_8888_0000`) turns out non-canonical
/// (`>= 2^47`, so `VirtAddr::new` would panic on it) - checked this
/// before picking a candidate, not after. Exists because every ring-3
/// program in this kernel's history has only ever had code and (since
/// Fase 100) a stack, never a THIRD region to read or write - the
/// "multiple segments" half of the general-loader gap `run_ring3_disk_
/// loaded_test`'s own doc has named since Fase 98.
pub const USER_DISK_PROGRAM_DATA_ADDR: u64 = 0x3333_3333_0000;

/// Maps one 4KiB page at `addr` with `PRESENT | WRITABLE |
/// USER_ACCESSIBLE` - the shared implementation behind both
/// `map_user_test_page` (Fase 70) and any later caller needing its own
/// independent user page (Fase 99).
pub fn map_user_page_at(
    mapper: &mut OffsetPageTable<'static>,
    frame_allocator: &mut impl FrameAllocator<Size4KiB>,
    addr: u64,
) -> Result<(), MapToError<Size4KiB>> {
    let page = Page::containing_address(VirtAddr::new(addr));
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

/// Deliberately distinct from the heap's own `HEAP_START` (`0x4444_
/// 4444_0000`) - see this module's own doc for why the specific choice
/// matters (no shared intermediate page-table entries).
pub fn map_user_test_page(
    mapper: &mut OffsetPageTable<'static>,
    frame_allocator: &mut impl FrameAllocator<Size4KiB>,
) -> Result<(), MapToError<Size4KiB>> {
    map_user_page_at(mapper, frame_allocator, USER_TEST_PAGE_ADDR)
}

pub struct UserPageInfo {
    pub present: bool,
    pub writable: bool,
    pub user_accessible: bool,
    pub write_read_back_ok: bool,
}

/// Verifies a mapping two independent ways, not just one. First, a
/// structural check: reads the REAL leaf PTE's own flags back through
/// the page tables (via `Translate`, not trusting `map_to`'s own
/// `Ok(())` return value alone). Second, a functional check: writes a
/// real, distinctive byte pattern into the page and reads it back -
/// ring-0 can access this page freely, since `USER_ACCESSIBLE` only
/// ever restricts ring-3, never ring-0 - proving the mapping backs
/// genuinely usable memory, not just that its flags look right in
/// isolation. Shared by `inspect_user_test_page` (Fase 70) and any
/// later caller inspecting its own independent page (Fase 99).
pub fn inspect_user_page_at(mapper: &OffsetPageTable<'static>, addr: u64) -> UserPageInfo {
    let addr = VirtAddr::new(addr);

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

pub fn inspect_user_test_page(mapper: &OffsetPageTable<'static>) -> UserPageInfo {
    inspect_user_page_at(mapper, USER_TEST_PAGE_ADDR)
}
