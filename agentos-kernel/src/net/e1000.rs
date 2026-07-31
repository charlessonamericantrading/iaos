//! Minimal real e1000 NIC access - the first step toward replacing
//! `net::virtio_net`'s fully simulated driver with something that talks to
//! actual hardware. QEMU's default machine (no extra `-device` flags)
//! exposes a real Intel 8254x-family NIC at PCI vendor:device 8086:100e -
//! confirmed present via `pci.rs`'s real enumeration (see README/Fase 11).
//!
//! This step deliberately stops at "find it, reach its registers, read a
//! couple back" - a real TX/RX driver (descriptor rings, actually
//! sending/receiving frames) is substantially more work and is left for
//! separate future iterations.
//!
//! ## Why no new page-table mapping code was needed
//! The e1000's BAR0 is a *memory-mapped* register window (not I/O-port
//! based) - normally reaching that means mapping its physical address to
//! a virtual one first. But this kernel already identity-maps the entire
//! physical address space at a fixed offset (`PHYS_MEM_OFFSET`, set from
//! `BootInfo::physical_memory_offset` - see `vga_buffer.rs`, which already
//! relies on this exact mechanism to reach the VGA text buffer's physical
//! address 0xb8000). Since 0xb8000 is itself outside any BIOS-reported
//! "usable RAM" region and that already works, this reuses the identical,
//! already-proven technique instead of writing new mapping code -
//! confirmed to actually work for *this* physical address by booting and
//! reading real register values below, not just assumed by analogy.

use crate::pci::{self, PciDevice};
use crate::vga_buffer::PHYS_MEM_OFFSET;
use crate::{kprintln, serial_println};
use core::sync::atomic::Ordering;

const E1000_VENDOR_ID: u16 = 0x8086;
const E1000_DEVICE_ID: u16 = 0x100e;

const REG_STATUS: u64 = 0x0008;
const REG_RAL0: u64 = 0x5400;
const REG_RAH0: u64 = 0x5404;

/// Finds the real e1000 NIC on bus 0, if present.
pub fn find_e1000() -> Option<PciDevice> {
    pci::scan_bus0()
        .into_iter()
        .find(|d| d.vendor_id == E1000_VENDOR_ID && d.device_id == E1000_DEVICE_ID)
}

/// Reads a 32-bit register from the e1000's memory-mapped register
/// window. `read_volatile` (not a plain dereference) matters here exactly
/// like it does for the VGA buffer's `Volatile<T>` wrapper - MMIO reads
/// can have side effects and must never be reordered, cached, or elided
/// by the compiler the way a normal memory read could be.
///
/// # Safety
/// `mmio_base` must be a valid, mapped virtual address for the start of a
/// real e1000 register window, and `offset` must be within that window.
unsafe fn read_reg(mmio_base: u64, offset: u64) -> u32 {
    core::ptr::read_volatile((mmio_base + offset) as *const u32)
}

/// Finds the e1000, reads its BAR0 to locate its register window, and
/// reads a couple of real registers back through it - proof this kernel
/// can genuinely talk to real device MMIO, not just enumerate config
/// space. Does not touch TX/RX yet.
pub fn probe() {
    let Some(dev) = find_e1000() else {
        kprintln!("[E1000] no e1000 NIC found on bus 0");
        serial_println!("[E1000] not found");
        return;
    };

    let bar0 = dev.read_bar0();
    let is_io_space = bar0 & 0x1 == 1;
    if is_io_space {
        kprintln!(
            "[E1000] BAR0 is I/O-space ({:#010x}) - not supported yet",
            bar0
        );
        serial_println!("[E1000] BAR0={:#010x} is I/O-space, unsupported", bar0);
        return;
    }
    // Bits 2:1 of a memory BAR encode its type (0 = 32-bit, 2 = 64-bit);
    // either way the base address itself lives in bits 31:4 - the low 4
    // bits are these flag bits, not part of the address.
    let phys_base = (bar0 & 0xFFFF_FFF0) as u64;
    let mmio_base = PHYS_MEM_OFFSET.load(Ordering::Relaxed) + phys_base;

    kprintln!(
        "[E1000] found at {:02x}:{:02x}.{} BAR0={:#010x} phys={:#x}",
        dev.bus,
        dev.device,
        dev.function,
        bar0,
        phys_base
    );
    serial_println!(
        "[E1000] found {:02x}:{:02x}.{} bar0={:#010x} phys={:#x}",
        dev.bus,
        dev.device,
        dev.function,
        bar0,
        phys_base
    );

    unsafe {
        let status = read_reg(mmio_base, REG_STATUS);
        let ral0 = read_reg(mmio_base, REG_RAL0);
        let rah0 = read_reg(mmio_base, REG_RAH0);

        let mac = [
            (ral0 & 0xFF) as u8,
            ((ral0 >> 8) & 0xFF) as u8,
            ((ral0 >> 16) & 0xFF) as u8,
            ((ral0 >> 24) & 0xFF) as u8,
            (rah0 & 0xFF) as u8,
            ((rah0 >> 8) & 0xFF) as u8,
        ];
        let mac_valid = rah0 & (1 << 31) != 0;

        kprintln!(
            "[E1000] STATUS={:#010x} MAC={:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x} (valid={})",
            status,
            mac[0],
            mac[1],
            mac[2],
            mac[3],
            mac[4],
            mac[5],
            mac_valid
        );
        serial_println!(
            "[E1000] status={:#010x} mac={:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x} valid={}",
            status,
            mac[0],
            mac[1],
            mac[2],
            mac[3],
            mac[4],
            mac[5],
            mac_valid
        );
    }
}
