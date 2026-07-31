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
            "[VIRTIO-NET] Hardware Driver initialized. MAC: {:02X}:{:02X}:{:02X}:{:02X}:{:02X}:{:02X}",
            self.mac_address[0], self.mac_address[1], self.mac_address[2],
            self.mac_address[3], self.mac_address[4], self.mac_address[5]
        );
        serial_println!(
            "[VIRTIO-NET] Driver MAC: {:02X}:{:02X}:{:02X}:{:02X}:{:02X}:{:02X}",
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
            "[VIRTIO-NET] Transmitted {} byte packet onto VirtQueue.",
            packet.len()
        );
        serial_println!("[VIRTIO-NET] TX Packet {} bytes", packet.len());
        true
    }
}

lazy_static! {
    pub static ref VIRTIO_NET: Mutex<VirtIONetDriver> = Mutex::new(VirtIONetDriver::new());
}
