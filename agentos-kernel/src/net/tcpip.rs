use crate::kprintln;
use crate::net::icmp;
use crate::net::udp;
use crate::net::virtio_net::VIRTIO_NET;

pub struct NativeNetworkStack;

impl NativeNetworkStack {
    /// Sends a UDP-carrying IPv4 broadcast packet over the simulated
    /// VirtIO-Net stack (see `virtio_net`'s own module doc for why it stays
    /// simulated). Builds the Ethernet/IPv4 headers via `net::icmp`'s own
    /// shared, already-verified helpers (Fase 146) instead of hand-rolling
    /// the bytes a second time - the old inline version left TTL and the
    /// IPv4 header checksum at 0, since nothing here ever filled them in.
    pub fn send_ipv4_packet(dest_ip: [u8; 4], payload: &[u8]) {
        let mut driver = VIRTIO_NET.lock();
        if !driver.is_initialized {
            driver.init();
        }

        const SRC_IP: [u8; 4] = [192, 168, 1, 100];
        let payload = &payload[..payload.len().min(90)];

        let eth_header = icmp::build_ethernet_header(
            [0xff, 0xff, 0xff, 0xff, 0xff, 0xff], // Broadcast MAC
            driver.mac_address,
            icmp::ETHERTYPE_IPV4,
        );
        let ip_header =
            icmp::build_ipv4_header(1, 64, udp::IP_PROTOCOL_UDP, SRC_IP, dest_ip, payload.len());

        let mut buffer = [0u8; 128];
        let ip_start = icmp::ETHERNET_HEADER_LEN;
        let payload_start = ip_start + icmp::IPV4_HEADER_LEN;
        let payload_end = payload_start + payload.len();
        buffer[..ip_start].copy_from_slice(&eth_header);
        buffer[ip_start..payload_start].copy_from_slice(&ip_header);
        buffer[payload_start..payload_end].copy_from_slice(payload);

        driver.transmit_packet(&buffer[..payload_end]);
        kprintln!(
            "[TCPIP STACK] IPv4 UDP packet sent to {}.{}.{}.{}",
            dest_ip[0],
            dest_ip[1],
            dest_ip[2],
            dest_ip[3]
        );
    }
}
