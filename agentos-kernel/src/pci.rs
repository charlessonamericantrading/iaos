//! Real PCI configuration-space access via the legacy I/O ports
//! (mechanism #1: `CONFIG_ADDRESS`/`CONFIG_DATA`, ports 0xCF8/0xCFC) -
//! universally supported since the original PCI spec, and simpler than
//! the newer memory-mapped ECAM mechanism.
//!
//! Mostly read-only, for enumeration - `scan_bus0`/`read_bar0` never
//! reconfigure anything. `PciDevice::enable_bus_mastering` (Fase 44) is
//! the one real exception: needed to set a device's Command register,
//! the one piece of real device bring-up this kernel's own PCI/e1000
//! work had never actually done - see that method's doc for why that
//! turned out to matter far more than "just" configuration.
//!
//! Groundwork for eventually replacing `net::virtio_net`'s fully
//! simulated driver with one that talks to a real VirtIO device - you
//! need to find it on the bus before you can talk to it.

use alloc::vec::Vec;
use x86_64::instructions::port::Port;

const CONFIG_ADDRESS: u16 = 0xCF8;
const CONFIG_DATA: u16 = 0xCFC;

fn config_address(bus: u8, device: u8, function: u8, offset: u8) -> u32 {
    // Bit 31 enable, bits 23-16 bus, 15-11 device, 10-8 function, 7-2
    // register index (offset is dword-aligned here - the low 2 bits are
    // masked off - since CONFIG_ADDRESS always selects a whole dword;
    // narrower reads/writes are a matter of which port width is used on
    // CONFIG_DATA once that dword is selected, not of this address).
    (1 << 31)
        | ((bus as u32) << 16)
        | ((device as u32) << 11)
        | ((function as u32) << 8)
        | ((offset as u32) & 0xFC)
}

fn read_config_dword(bus: u8, device: u8, function: u8, offset: u8) -> u32 {
    unsafe {
        let mut addr_port: Port<u32> = Port::new(CONFIG_ADDRESS);
        addr_port.write(config_address(bus, device, function, offset));
        let mut data_port: Port<u32> = Port::new(CONFIG_DATA);
        data_port.read()
    }
}

/// Reads just the low 16 bits of the dword at `offset` - e.g. the
/// Command register at offset 0x04, which shares its containing dword
/// with the Status register at 0x06. A 16-bit-wide access to
/// `CONFIG_DATA` (still addressed via the same dword-aligned
/// `CONFIG_ADDRESS`) naturally lands on exactly those first two bytes -
/// standard PCI host-bridge behavior, not something specific to this
/// kernel or QEMU.
fn read_config_word(bus: u8, device: u8, function: u8, offset: u8) -> u16 {
    unsafe {
        let mut addr_port: Port<u32> = Port::new(CONFIG_ADDRESS);
        addr_port.write(config_address(bus, device, function, offset));
        let mut data_port: Port<u16> = Port::new(CONFIG_DATA);
        data_port.read()
    }
}

/// Writes just the low 16 bits of the dword at `offset` - the write-side
/// counterpart to `read_config_word`, and real reconfiguration (unlike
/// every read-only function above). Deliberately narrower than a full
/// dword write specifically so modifying the Command register can never
/// accidentally disturb Status (several of whose bits are write-1-to-
/// clear event flags - a blind dword read-modify-write risks silently
/// clearing one that happened to already be set).
fn write_config_word(bus: u8, device: u8, function: u8, offset: u8, value: u16) {
    unsafe {
        let mut addr_port: Port<u32> = Port::new(CONFIG_ADDRESS);
        addr_port.write(config_address(bus, device, function, offset));
        let mut data_port: Port<u16> = Port::new(CONFIG_DATA);
        data_port.write(value);
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

const PCI_COMMAND_OFFSET: u8 = 0x04;
const PCI_COMMAND_BUS_MASTER: u16 = 1 << 2;

impl PciDevice {
    /// Reads BAR0 (config space offset 0x10) - the first Base Address
    /// Register, where a device's memory-mapped (or I/O-mapped) register
    /// window is advertised. Returns the raw register value, flag bits
    /// and all - callers that want the actual base address need to check
    /// bit 0 (0 = memory space, 1 = I/O space) and mask accordingly, since
    /// the meaning of the low bits differs between the two.
    pub fn read_bar0(&self) -> u32 {
        read_config_dword(self.bus, self.device, self.function, 0x10)
    }

    /// Sets the Command register's Bus Master Enable bit (bit 2) - the
    /// one PCI bring-up step real device drivers always perform that
    /// this kernel's own PCI/e1000 work had never actually done. A real,
    /// standard PCI/PCIe requirement, not a QEMU quirk: a freshly
    /// enumerated device starts with bus mastering *disabled* (a real
    /// safety property - an unconfigured device can't unexpectedly DMA
    /// into arbitrary memory), and system software is expected to enable
    /// it explicitly once the device is actually ready to be driven.
    /// Read-modify-write at word granularity (not a full dword) so the
    /// adjacent Status register (several of whose bits are write-1-to-
    /// clear) is never touched.
    ///
    /// Traced (Fase 44) through QEMU's own `hw/pci/pci.c`: any config
    /// write touching the Command register calls `pci_set_master`,
    /// which enables/disables `bus_master_enable_region` - the memory
    /// region every `pci_dma_read`/`pci_dma_write` call (`e1000.rs`'s TX
    /// descriptor writeback among them) actually routes through. Without
    /// this call, those DMA writes silently have nowhere real to land -
    /// a highly plausible root cause for a mystery 2 prior investigation
    /// rounds (see `net/e1000.rs`'s module doc) couldn't otherwise
    /// explain: `TDH` provably advances (an internal register update,
    /// unaffected by bus mastering) while the descriptor's own DD status
    /// bit (a real DMA write) never appears.
    pub fn enable_bus_mastering(&self) {
        let command = read_config_word(self.bus, self.device, self.function, PCI_COMMAND_OFFSET);
        write_config_word(
            self.bus,
            self.device,
            self.function,
            PCI_COMMAND_OFFSET,
            command | PCI_COMMAND_BUS_MASTER,
        );
    }
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
