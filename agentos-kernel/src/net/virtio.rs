//! Real VirtIO-net PCI access - `net::virtio_net` remains the fully
//! simulated placeholder module (kept untouched, same reasoning as
//! keeping `net::e1000` separate from it). QEMU's default machine has no
//! VirtIO device at all; this kernel's boot command now adds `-device
//! virtio-net-pci` specifically to give this module something real to
//! talk to. **Verified empirically before writing any of this code,
//! not assumed**: with that flag added, the new device lands at PCI
//! slot `00:04.0` - a genuinely new slot, confirmed to leave the
//! already-present e1000 at `00:03.0` (and every other existing device)
//! completely unaffected, so every prior PCI/e1000 self-test and CI
//! check stays valid unchanged.
//!
//! First stage only, mirroring `net::e1000`'s own Fase 19 opening:
//! find the device, reach its registers, read a few back - proof this
//! kernel can genuinely talk to a second, structurally different real
//! NIC. No virtqueue setup or TX/RX yet - that's real, substantial
//! follow-on work, the same multi-Fase progression `net::e1000` needed.
//!
//! **Genuinely different from e1000 in the one way that matters most
//! just to reach its registers at all**: e1000's BAR0 is memory-mapped
//! (reached through this kernel's existing identity-map trick, see
//! `net::e1000`'s own module doc); VirtIO's legacy PCI transport's BAR0
//! is I/O-port space instead - confirmed, not assumed, by reading the
//! real BAR0 value back and checking bit 0 (1 = I/O space). This needs
//! genuine `in`/`out` port instructions (`x86_64::instructions::port::
//! Port`, the same primitive `pci.rs`'s own `CONFIG_ADDRESS`/
//! `CONFIG_DATA` access already uses) rather than a `read_volatile`
//! through a mapped address - `find_e1000_mmio_base`-style code would
//! silently read garbage here if pointed at an I/O BAR's raw value.
//!
//! Register offsets (within the I/O port BAR) verified against the
//! Linux kernel's own real, canonical `include/uapi/linux/virtio_pci.h`,
//! the actual header real VirtIO drivers use, fetched directly rather
//! than assumed from a paraphrased description: `VIRTIO_PCI_HOST_
//! FEATURES`=0 (32-bit r/o), `VIRTIO_PCI_STATUS`=18 (8-bit r/w),
//! `VIRTIO_PCI_ISR`=19 (8-bit r/o, read clears it), and device-specific
//! configuration starting at offset 20 (no MSI-X) or 24 (with MSI-X),
//! `VIRTIO_PCI_CONFIG_OFF`. A network device's own config there
//! (`include/uapi/linux/virtio_net.h`'s `virtio_net_config`) starts
//! with a 6-byte MAC address, valid only if bit 5 (`VIRTIO_NET_F_MAC`)
//! is set in the features bitmask read back above. Which offset
//! actually applies for this exact QEMU instantiation was confirmed
//! empirically (both were read back and compared locally) rather than
//! assumed either way, exactly like the I/O-vs-memory BAR check above.

use crate::kprintln;
use crate::pci::{self, PciDevice};
use crate::serial_println;
use x86_64::instructions::port::Port;

const VIRTIO_VENDOR_ID: u16 = 0x1af4;
const VIRTIO_NET_DEVICE_ID: u16 = 0x1000;

const VIRTIO_PCI_HOST_FEATURES: u16 = 0;
const VIRTIO_PCI_STATUS: u16 = 18;
const VIRTIO_PCI_ISR: u16 = 19;
const VIRTIO_PCI_CONFIG_OFF_NO_MSIX: u16 = 20;

const VIRTIO_NET_F_MAC: u32 = 1 << 5;

pub fn find_virtio_net() -> Option<PciDevice> {
    pci::scan_bus0()
        .into_iter()
        .find(|d| d.vendor_id == VIRTIO_VENDOR_ID && d.device_id == VIRTIO_NET_DEVICE_ID)
}

fn find_io_base() -> Result<(PciDevice, u16), &'static str> {
    let dev = find_virtio_net().ok_or("no virtio-net device found on bus 0")?;
    let bar0 = dev.read_bar0();
    if bar0 & 0x1 == 0 {
        return Err("virtio-net BAR0 is memory-space - not supported yet");
    }
    // Bit 0 = 1 marks I/O space; bit 1 is reserved. Unlike a memory BAR
    // (address in bits 31:4), a real PCI I/O BAR only reserves the low
    // TWO bits for flags - the port base address lives in bits 31:2.
    let io_base = (bar0 & 0xFFFF_FFFC) as u16;
    Ok((dev, io_base))
}

unsafe fn read_port_u32(base: u16, offset: u16) -> u32 {
    unsafe {
        let mut port: Port<u32> = Port::new(base + offset);
        port.read()
    }
}

unsafe fn read_port_u8(base: u16, offset: u16) -> u8 {
    unsafe {
        let mut port: Port<u8> = Port::new(base + offset);
        port.read()
    }
}

fn read_mac_at(io_base: u16, config_offset: u16) -> [u8; 6] {
    let mut mac = [0u8; 6];
    for (i, byte) in mac.iter_mut().enumerate() {
        *byte = unsafe { read_port_u8(io_base, config_offset + i as u16) };
    }
    mac
}

/// Finds the device, reaches its I/O-port register window, and reads a
/// few real registers back - proof this kernel can genuinely talk to
/// this second, structurally different real NIC (I/O ports, not MMIO).
pub fn probe() {
    let (dev, io_base) = match find_io_base() {
        Ok(v) => v,
        Err(e) => {
            kprintln!("[VIRTIO] {}", e);
            serial_println!("[VIRTIO] {}", e);
            return;
        }
    };

    let bar0 = dev.read_bar0();
    kprintln!(
        "[VIRTIO] found at {:02x}:{:02x}.{} BAR0={:#010x} io_base={:#06x}",
        dev.bus,
        dev.device,
        dev.function,
        bar0,
        io_base
    );
    serial_println!(
        "[VIRTIO] found {:02x}:{:02x}.{} bar0={:#010x} io_base={:#06x}",
        dev.bus,
        dev.device,
        dev.function,
        bar0,
        io_base
    );

    let features = unsafe { read_port_u32(io_base, VIRTIO_PCI_HOST_FEATURES) };
    let status = unsafe { read_port_u8(io_base, VIRTIO_PCI_STATUS) };
    let isr = unsafe { read_port_u8(io_base, VIRTIO_PCI_ISR) };
    let mac_feature_present = features & VIRTIO_NET_F_MAC != 0;
    let mac = read_mac_at(io_base, VIRTIO_PCI_CONFIG_OFF_NO_MSIX);

    kprintln!(
        "[VIRTIO] features={:#010x} status={:#04x} isr={:#04x} mac_feature={} mac={:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
        features,
        status,
        isr,
        mac_feature_present,
        mac[0],
        mac[1],
        mac[2],
        mac[3],
        mac[4],
        mac[5]
    );
    serial_println!(
        "[VIRTIO] features={:#010x} status={:#04x} isr={:#04x} mac_feature={} mac={:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
        features,
        status,
        isr,
        mac_feature_present,
        mac[0],
        mac[1],
        mac[2],
        mac[3],
        mac[4],
        mac[5]
    );
}
