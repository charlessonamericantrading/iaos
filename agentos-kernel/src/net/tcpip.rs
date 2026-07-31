use crate::kprintln;
use crate::net::virtio_net::VIRTIO_NET;

#[repr(C, packed)]
pub struct EthernetHeader {
    pub dest_mac: [u8; 6],
    pub src_mac: [u8; 6],
    pub ethertype: u16,
}

#[repr(C, packed)]
pub struct Ipv4Header {
    pub version_ihl: u8,
    pub tos: u8,
    pub total_length: u16,
    pub identification: u16,
    pub flags_fragment: u16,
    pub ttl: u8,
    pub protocol: u8,
    pub checksum: u16,
    pub src_ip: [u8; 4],
    pub dest_ip: [u8; 4],
}

pub struct NativeNetworkStack;

impl NativeNetworkStack {
    pub fn send_ipv4_packet(dest_ip: [u8; 4], payload: &[u8]) {
        let mut driver = VIRTIO_NET.lock();
        if !driver.is_initialized {
            driver.init();
        }

        let mut buffer = [0u8; 128];
        // Fill Ethernet Header
        buffer[0..6].copy_from_slice(&[0xff, 0xff, 0xff, 0xff, 0xff, 0xff]); // Broadcast MAC
        buffer[6..12].copy_from_slice(&driver.mac_address);
        buffer[12] = 0x08; // IPv4 EtherType 0x0800
        buffer[13] = 0x00;

        // Fill IPv4 Header
        buffer[14] = 0x45; // Version 4, IHL 5
        buffer[15] = 0x00;
        let total_len = (20 + payload.len()) as u16;
        buffer[16..18].copy_from_slice(&total_len.to_be_bytes());
        buffer[23] = 17; // UDP protocol
        buffer[26..30].copy_from_slice(&[192, 168, 1, 100]); // Src IP
        buffer[30..34].copy_from_slice(&dest_ip); // Dest IP

        // Copy payload
        let payload_end = 34 + payload.len().min(90);
        buffer[34..payload_end].copy_from_slice(&payload[..payload.len().min(90)]);

        driver.transmit_packet(&buffer[..payload_end]);
        kprintln!("[TCPIP STACK] IPv4 UDP packet sent to {}.{}.{}.{}", dest_ip[0], dest_ip[1], dest_ip[2], dest_ip[3]);
    }
}
