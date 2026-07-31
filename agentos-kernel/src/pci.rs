//! Real PCI configuration-space access via the legacy I/O ports
//! (mechanism #1: `CONFIG_ADDRESS`/`CONFIG_DATA`, ports 0xCF8/0xCFC) -
//! universally supported since the original PCI spec, and simpler than
//! the newer memory-mapped ECAM mechanism. Read-only: this only ever
//! reads config space to enumerate what's present, never writes to
//! reconfigure a device.
//!
//! Groundwork for eventually replacing `net::virtio_net`'s fully
//! simulated driver with one that talks to a real VirtIO device - you
//! need to find it on the bus before you can talk to it.

use alloc::vec::Vec;
use x86_64::instructions::port::Port;

const CONFIG_ADDRESS: u16 = 0xCF8;
const CONFIG_DATA: u16 = 0xCFC;

fn read_config_dword(bus: u8, device: u8, function: u8, offset: u8) -> u32 {
    // Bit 31 enable, bits 23-16 bus, 15-11 device, 10-8 function, 7-2
    // register index (offset is already byte-aligned to 4 here since we
    // only ever read whole dwords at offsets 0x00 and 0x08).
    let address: u32 = (1 << 31)
        | ((bus as u32) << 16)
        | ((device as u32) << 11)
        | ((function as u32) << 8)
        | ((offset as u32) & 0xFC);
    unsafe {
        let mut addr_port: Port<u32> = Port::new(CONFIG_ADDRESS);
        addr_port.write(address);
        let mut data_port: Port<u32> = Port::new(CONFIG_DATA);
        data_port.read()
    }
}

pub struct PciDevice {
    pub bus: u8,
    pub device: u8,
    pub function: u8,
    pub vendor_id: u16,
    pub device_id: u16,
    pub class: u8,
    pub subclass: u8,
    pub prog_if: u8,
}

pub fn class_name(class: u8) -> &'static str {
    match class {
        0x00 => "Unclassified",
        0x01 => "Mass Storage",
        0x02 => "Network",
        0x03 => "Display",
        0x04 => "Multimedia",
        0x06 => "Bridge",
        0x0C => "Serial Bus",
        _ => "Other",
    }
}

/// Brute-force scans every device/function slot on bus 0 (no recursion
/// through PCI-to-PCI bridges onto other buses yet - bus 0 is enough to
/// see what a typical QEMU machine exposes by default). A vendor ID of
/// `0xFFFF` means nothing answered at that slot, which is the normal case
/// for the vast majority of the 256 slots checked.
pub fn scan_bus0() -> Vec<PciDevice> {
    let mut found = Vec::new();
    for device in 0..32u8 {
        for function in 0..8u8 {
            let id_word = read_config_dword(0, device, function, 0x00);
            let vendor_id = (id_word & 0xFFFF) as u16;
            if vendor_id == 0xFFFF {
                continue;
            }
            let device_id = (id_word >> 16) as u16;

            let class_word = read_config_dword(0, device, function, 0x08);
            let prog_if = ((class_word >> 8) & 0xFF) as u8;
            let subclass = ((class_word >> 16) & 0xFF) as u8;
            let class = ((class_word >> 24) & 0xFF) as u8;

            found.push(PciDevice {
                bus: 0,
                device,
                function,
                vendor_id,
                device_id,
                class,
                subclass,
                prog_if,
            });
        }
    }
    found
}
