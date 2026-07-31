use x86_64::structures::paging::{PhysFrame, Size4KiB, FrameAllocator};
use x86_64::PhysAddr;

/// Physical Page Frame Allocator for AgentOS Kernel
pub struct BitmapFrameAllocator {
    next_frame_index: usize,
    max_frames: usize,
}

impl BitmapFrameAllocator {
    pub unsafe fn init(start_phys_addr: u64, memory_size_bytes: usize) -> Self {
        let max_frames = memory_size_bytes / 4096;
        BitmapFrameAllocator {
            next_frame_index: (start_phys_addr / 4096) as usize,
            max_frames,
        }
    }
}

unsafe impl FrameAllocator<Size4KiB> for BitmapFrameAllocator {
    fn allocate_frame(&mut self) -> Option<PhysFrame<Size4KiB>> {
        if self.next_frame_index < self.max_frames {
            let frame_addr = PhysAddr::new((self.next_frame_index * 4096) as u64);
            self.next_frame_index += 1;
            Some(PhysFrame::containing_address(frame_addr))
        } else {
            None
        }
    }
}
