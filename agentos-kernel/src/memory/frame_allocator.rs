use bootloader_api::info::{MemoryRegionKind, MemoryRegions};
use spin::Mutex;
use x86_64::structures::paging::{FrameAllocator, PhysFrame, Size4KiB};
use x86_64::PhysAddr;

/// Hands out physical frames from the bootloader's memory map instead of a
/// blind address range - the previous `BitmapFrameAllocator` had no idea
/// which physical pages were already occupied by the kernel image, the
/// bootloader's own page tables, or BIOS/UEFI-reserved regions, so handing
/// out its frames could have silently corrupted any of those.
///
/// This never reclaims a frame once handed out (matches the KV-cache and
/// scheduler's static-array style already used elsewhere in this kernel -
/// nothing frees memory yet, so a free-list would be unused complexity).
pub struct BootInfoFrameAllocator {
    memory_regions: &'static MemoryRegions,
    next: usize,
}

impl BootInfoFrameAllocator {
    /// # Safety
    /// `memory_regions` must be the memory map handed to us by the
    /// `bootloader` crate via `BootInfo` - it must accurately describe which
    /// physical regions are actually unused.
    pub unsafe fn init(memory_regions: &'static MemoryRegions) -> Self {
        BootInfoFrameAllocator {
            memory_regions,
            next: 0,
        }
    }

    fn usable_frames(&self) -> impl Iterator<Item = PhysFrame> {
        self.memory_regions
            .iter()
            .filter(|r| r.kind == MemoryRegionKind::Usable)
            .flat_map(|r| r.start..r.end)
            .step_by(4096)
            .map(|addr| PhysFrame::containing_address(PhysAddr::new(addr)))
    }
}

unsafe impl FrameAllocator<Size4KiB> for BootInfoFrameAllocator {
    fn allocate_frame(&mut self) -> Option<PhysFrame<Size4KiB>> {
        let frame = self.usable_frames().nth(self.next);
        self.next += 1;
        frame
    }
}

/// Holds the *same* `BootInfoFrameAllocator` instance `kernel_main` used
/// for heap init, made reachable after boot for anything else that needs
/// dedicated physical frames outside the heap - a future real NIC driver's
/// TX/RX descriptor rings being the motivating case. This has to be the
/// literal same instance (with its `next` cursor wherever heap init left
/// it), not a fresh one built later from the same memory map - a fresh
/// instance would start handing out frame 0 again, which by then is
/// already backing the heap. `kernel_main` installs it via
/// `install_global` right after heap init.
static FRAME_ALLOCATOR: Mutex<Option<BootInfoFrameAllocator>> = Mutex::new(None);

/// Moves `allocator` into the global slot. Called exactly once, from
/// `kernel_main` right after heap init hands it back - anything calling
/// `allocate_frame` below before this has run will panic, since there is
/// no meaningful fallback (there's no *other* source of physical frames).
pub fn install_global(allocator: BootInfoFrameAllocator) {
    *FRAME_ALLOCATOR.lock() = Some(allocator);
}

/// Allocates one fresh physical frame from the global allocator installed
/// by `install_global`. Panics if called before that (misuse - there's no
/// boot ordering where this should legitimately happen) or if physical
/// memory is exhausted (this kernel's `BootInfoFrameAllocator` never
/// reclaims frames - see its doc comment - so exhaustion is permanent).
pub fn allocate_frame() -> PhysFrame<Size4KiB> {
    FRAME_ALLOCATOR
        .lock()
        .as_mut()
        .expect("global frame allocator not installed yet - install_global wasn't called")
        .allocate_frame()
        .expect("out of physical memory - BootInfoFrameAllocator never reclaims frames")
}
