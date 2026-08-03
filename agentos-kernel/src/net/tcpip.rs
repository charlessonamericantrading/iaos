use crate::kprintln;
use crate::net::icmp;
use crate::net::udp;
use crate::net::virtio_net::VIRTIO_NET;

pub struct NativeNetworkStack;

impl NativeNetworkStack {
    /// Sends a UDP-carrying IPv4 broadcast packet over the simulated
    /// VirtIO-Net stack (see `virtio_net`'s own module doc for why it stays
    /// simulated). Builds the Ethernet/IPv4/UDP headers via `net::icmp`'s
    /// and `net::udp`'s own shared, already-verified helpers (Fase 146)
    /// instead of hand-rolling the bytes a second time - the old inline
    /// version left TTL and the IPv4 header checksum at 0, since nothing
    /// here ever filled them in.
    ///
    /// Fase 161: closes the 12th Explore-agent survey's candidate #3 - the
    /// IPv4 header declared `protocol=UDP` but no UDP header (source/dest
    /// port, length, checksum) was ever actually built; `payload` was
    /// written directly after the IPv4 header as if it were the whole IP
    /// payload. Never a live wrong-result bug, since `transmit_packet`
    /// below is fully simulated and only counts/logs byte length - never
    /// parses what it's given - but a genuine code/doc inconsistency: the
    /// function's own log line ("IPv4 UDP packet sent") and the header's
    /// own protocol field both claimed something the bytes on the wire
    /// didn't back up. `SRC_PORT`/`DEST_PORT` are synthetic - there is no
    /// real listener on the other end of this simulated broadcast - but
    /// the datagram itself is now a genuine, checksummed UDP packet rather
    /// than raw bytes mislabeled as one.
    pub fn send_ipv4_packet(dest_ip: [u8; 4], payload: &[u8]) {
        let mut driver = VIRTIO_NET.lock();
        if !driver.is_initialized {
            driver.init();
        }

        const SRC_IP: [u8; 4] = [192, 168, 1, 100];
        const SRC_PORT: u16 = 49152;
        const DEST_PORT: u16 = 9000;
        let payload = &payload[..payload.len().min(90)];

        let eth_header = icmp::build_ethernet_header(
            [0xff, 0xff, 0xff, 0xff, 0xff, 0xff], // Broadcast MAC
            driver.mac_address,
            icmp::ETHERTYPE_IPV4,
        );
        let udp_header = udp::build_udp_header(SRC_IP, dest_ip, SRC_PORT, DEST_PORT, payload);
        let ip_header = icmp::build_ipv4_header(
            1,
            64,
            udp::IP_PROTOCOL_UDP,
            SRC_IP,
            dest_ip,
            udp::UDP_HEADER_LEN + payload.len(),
        );

        let mut buffer = [0u8; 132];
        let ip_start = icmp::ETHERNET_HEADER_LEN;
        let udp_start = ip_start + icmp::IPV4_HEADER_LEN;
        let payload_start = udp_start + udp::UDP_HEADER_LEN;
        let payload_end = payload_start + payload.len();
        buffer[..ip_start].copy_from_slice(&eth_header);
        buffer[ip_start..udp_start].copy_from_slice(&ip_header);
        buffer[udp_start..payload_start].copy_from_slice(&udp_header);
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
