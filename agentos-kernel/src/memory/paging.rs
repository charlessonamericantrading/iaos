//! Global access to the kernel's page-table mapper (Fase 74), following the
//! exact same `Mutex<Option<T>>` + `install_global` pattern
//! `frame_allocator.rs` already established. Before this, the
//! `OffsetPageTable` `memory::init` builds only ever existed as a plain
//! local in `kernel_main` - fine for the boot-time-only callers that used
//! it directly (`heap::init_heap`, `user_page::map_user_test_page`/
//! `inspect_user_test_page`), but unreachable from anywhere else, in
//! particular `syscall.rs`, which needs it to validate a syscall's raw
//! pointer arguments before ever dereferencing them (see `pointer_is_mapped`
//! below - the actual motivating use, not exposed for its own sake).
//!
//! Only one `OffsetPageTable` can safely exist at a time (it wraps a
//! unique `&'static mut PageTable` - two live instances would be aliased
//! mutable access, real UB), so this REPLACES `kernel_main`'s own local
//! variable entirely rather than existing alongside it: `install_global`
//! is called immediately after `memory::init`, and every other call site
//! that used to hold `&mut mapper` directly now goes through
//! `with_mapper` instead.

use spin::Mutex;
use x86_64::structures::paging::mapper::TranslateResult;
use x86_64::structures::paging::{OffsetPageTable, PageTableFlags, Translate};
use x86_64::VirtAddr;

static MAPPER: Mutex<Option<OffsetPageTable<'static>>> = Mutex::new(None);

/// Moves `mapper` into the global slot. Called exactly once, from
/// `kernel_main` immediately after `memory::init` builds it - every other
/// use of the mapper anywhere in this kernel goes through `with_mapper`
/// from this point on.
pub fn install_global(mapper: OffsetPageTable<'static>) {
    *MAPPER.lock() = Some(mapper);
}

/// Runs `f` with exclusive access to the global mapper. Panics if called
/// before `install_global` (misuse - there's no meaningful fallback, same
/// reasoning as `frame_allocator::allocate_frame`'s own panic).
pub fn with_mapper<R>(f: impl FnOnce(&mut OffsetPageTable<'static>) -> R) -> R {
    let mut guard = MAPPER.lock();
    let mapper = guard
        .as_mut()
        .expect("global page-table mapper not installed yet - install_global wasn't called");
    f(mapper)
}

/// Checks whether `addr` is genuinely safe to dereference right now -
/// present in the page tables, not just a plausible-looking `u64` a
/// syscall caller happened to supply. The real motivation (Fase 74):
/// `syscall::dispatch_syscall`'s `SYS_TENSOR_EVAL` arm takes a raw
/// pointer argument and, until this Fase, dereferenced it unconditionally:
/// a caller passing a garbage or unmapped address (accidentally, from a
/// buggy ring-3 program, or deliberately) would trigger a real `#PF` from
/// INSIDE the syscall handler, hitting the existing `page_fault_handler`,
/// which halts the kernel forever, turning one bad syscall argument into
/// a dead machine rather than a rejected call. Now that ring-3 code can
/// genuinely invoke syscalls with real arguments (Fase 72) and cleanly
/// return control afterward (Fase 73), that gap is real, not theoretical.
///
/// Deliberately uses `VirtAddr::try_new`, NOT `VirtAddr::new` - the
/// latter PANICS on a non-canonical address (bits 48..64 not a proper
/// sign extension of bit 47), which an arbitrary, untrusted `u64` from a
/// syscall caller can easily be. A validation function that itself
/// panics on invalid input defeats its own purpose.
///
/// Only checks the mapping is genuinely PRESENT - deliberately does NOT
/// require `USER_ACCESSIBLE`. Safe to call regardless of who the caller
/// is (ring-0 or ring-3) - see `pointer_is_user_accessible` below for the
/// stricter check that DOES depend on that, now that Fase 75 made caller
/// privilege something `dispatch_syscall` can actually know.
pub fn pointer_is_mapped(addr: u64) -> bool {
    pointer_has_flags(addr, PageTableFlags::PRESENT)
}

/// Stricter than `pointer_is_mapped`: also requires `USER_ACCESSIBLE`,
/// i.e. genuinely safe for RING-3 code to have legitimately obtained -
/// not just present in the page tables at all. The real motivation
/// (Fase 76): now that `dispatch_syscall` can tell whether a syscall
/// genuinely came from ring-3 (Fase 75's own `caller_rpl`), a ring-3
/// caller passing a pointer to real, PRESENT but kernel-only memory
/// (the heap, in particular - see `memory/user_page.rs`'s own doc:
/// every mapping before Fase 70 was `PRESENT | WRITABLE` only, never
/// `USER_ACCESSIBLE`) should be REJECTED, exactly like an unmapped
/// pointer already is - a "confused deputy" risk `pointer_is_mapped`
/// alone can't catch, since kernel memory legitimately passes THAT
/// check. Only meaningful to call for a ring-3-originated pointer -
/// see `syscall.rs`'s own call site for how the two checks are chosen.
pub fn pointer_is_user_accessible(addr: u64) -> bool {
    pointer_has_flags(
        addr,
        PageTableFlags::PRESENT | PageTableFlags::USER_ACCESSIBLE,
    )
}

fn pointer_has_flags(addr: u64, required: PageTableFlags) -> bool {
    if addr == 0 {
        return false;
    }
    // Deliberately uses `VirtAddr::try_new`, NOT `VirtAddr::new` - the
    // latter PANICS on a non-canonical address (bits 48..64 not a proper
    // sign extension of bit 47), which an arbitrary, untrusted `u64`
    // from a syscall caller can easily be. A validation function that
    // itself panics on invalid input defeats its own purpose.
    let Ok(virt) = VirtAddr::try_new(addr) else {
        return false;
    };
    with_mapper(|mapper| {
        matches!(
            mapper.translate(virt),
            TranslateResult::Mapped { flags, .. } if flags.contains(required)
        )
    })
}
