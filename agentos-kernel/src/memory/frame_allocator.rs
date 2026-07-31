use bootloader_api::info::{MemoryRegionKind, MemoryRegions};
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
