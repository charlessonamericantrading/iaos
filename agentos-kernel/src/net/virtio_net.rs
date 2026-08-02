//! This module is exactly what `net::e1000`'s and `net::virtio`'s own doc
//! comments already correctly call it: the fully simulated placeholder.
//! `init()` and `transmit_packet()` below do no real PCI enumeration, MMIO
//! access, or virtqueue I/O - just a boolean flip and a counter. Dates to
//! this project's very first commit and was never revisited even as this
//! kernel built two genuinely real NIC drivers since: `net::e1000` (Fase
//! 19+, real MMIO-mapped 8254x hardware) and `net::virtio` (Fase 62+, real
//! I/O-port VirtIO PCI transport with its own real virtqueues). Kept around
//! as an honest, self-admittedly-fake early demo rather than deleted
//! outright - Fase 134 fixed this file's own printed strings, which
//! (unlike its two real siblings' module docs) still called this
//! "Hardware" until then.

use crate::{kprintln, serial_println};
use lazy_static::lazy_static;
use spin::Mutex;

pub struct VirtIONetDriver {
    pub mac_address: [u8; 6],
    pub is_initialized: bool,
    pub rx_packets_count: usize,
    pub tx_packets_count: usize,
}

impl VirtIONetDriver {
    pub const fn new() -> Self {
        VirtIONetDriver {
            mac_address: [0x52, 0x54, 0x00, 0x12, 0x34, 0x56], // Standard VirtIO MAC
            is_initialized: false,
            rx_packets_count: 0,
            tx_packets_count: 0,
        }
    }

    pub fn init(&mut self) {
        self.is_initialized = true;
        kprintln!(
            "[VIRTIO-NET] Simulated driver initialized. MAC: {:02X}:{:02X}:{:02X}:{:02X}:{:02X}:{:02X}",
            self.mac_address[0], self.mac_address[1], self.mac_address[2],
            self.mac_address[3], self.mac_address[4], self.mac_address[5]
        );
        serial_println!(
            "[VIRTIO-NET] Simulated driver MAC: {:02X}:{:02X}:{:02X}:{:02X}:{:02X}:{:02X}",
            self.mac_address[0],
            self.mac_address[1],
            self.mac_address[2],
            self.mac_address[3],
            self.mac_address[4],
            self.mac_address[5]
        );
    }

    pub fn transmit_packet(&mut self, packet: &[u8]) -> bool {
        if !self.is_initialized {
            return false;
        }
        self.tx_packets_count += 1;
        kprintln!(
            "[VIRTIO-NET] Simulated transmit of {} byte packet (no real virtqueue).",
            packet.len()
        );
        serial_println!("[VIRTIO-NET] Simulated TX {} bytes", packet.len());
        true
    }
}

lazy_static! {
    pub static ref VIRTIO_NET: Mutex<VirtIONetDriver> = Mutex::new(VirtIONetDriver::new());
}
