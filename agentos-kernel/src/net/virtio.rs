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
//! First stage (Fase 62): find the device, reach its registers, read a
//! few back - proof this kernel can genuinely talk to a second,
//! structurally different real NIC. Second stage (Fase 63):
//! `init_tx_queue` completes the real device-initialization handshake
//! and sets up ONE real virtqueue (TX) - still no actual frame sent or
//! received yet, that's real, substantial follow-on work, the same
//! multi-Fase progression `net::e1000` needed (Fase 19 -> 22 -> 44/45).
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
use crate::vga_buffer::PHYS_MEM_OFFSET;
use core::sync::atomic::Ordering;
use x86_64::instructions::port::Port;

const VIRTIO_VENDOR_ID: u16 = 0x1af4;
const VIRTIO_NET_DEVICE_ID: u16 = 0x1000;

const VIRTIO_PCI_HOST_FEATURES: u16 = 0;
const VIRTIO_PCI_GUEST_FEATURES: u16 = 4;
const VIRTIO_PCI_QUEUE_PFN: u16 = 8;
const VIRTIO_PCI_QUEUE_NUM: u16 = 12;
const VIRTIO_PCI_QUEUE_SEL: u16 = 14;
const VIRTIO_PCI_STATUS: u16 = 18;
const VIRTIO_PCI_ISR: u16 = 19;
const VIRTIO_PCI_CONFIG_OFF_NO_MSIX: u16 = 20;

const VIRTIO_NET_F_MAC: u32 = 1 << 5;

const VIRTIO_STATUS_ACKNOWLEDGE: u8 = 1;
const VIRTIO_STATUS_DRIVER: u8 = 2;
const VIRTIO_STATUS_DRIVER_OK: u8 = 4;

const VIRTIO_PCI_QUEUE_NOTIFY: u16 = 16;

const TX_QUEUE_INDEX: u16 = 1;
const VRING_ALIGN: u64 = 4096;
const FRAME_SIZE: u64 = 4096;
const VIRTIO_NET_HDR_LEN: usize = 10; // legacy virtio_net_hdr, no MRG_RXBUF

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

unsafe fn read_port_u16(base: u16, offset: u16) -> u16 {
    unsafe {
        let mut port: Port<u16> = Port::new(base + offset);
        port.read()
    }
}

unsafe fn write_port_u16(base: u16, offset: u16, value: u16) {
    unsafe {
        let mut port: Port<u16> = Port::new(base + offset);
        port.write(value);
    }
}

unsafe fn write_port_u32(base: u16, offset: u16, value: u32) {
    unsafe {
        let mut port: Port<u32> = Port::new(base + offset);
        port.write(value);
    }
}

unsafe fn read_port_u8(base: u16, offset: u16) -> u8 {
    unsafe {
        let mut port: Port<u8> = Port::new(base + offset);
        port.read()
    }
}

unsafe fn write_port_u8(base: u16, offset: u16, value: u8) {
    unsafe {
        let mut port: Port<u8> = Port::new(base + offset);
        port.write(value);
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

/// Computes the total byte size of a legacy VirtIO virtqueue holding
/// `num` descriptors - verified against the Linux kernel's own real
/// `vring_size()` (`include/uapi/linux/virtio_ring.h`), not derived
/// from memory alone: the descriptor table (16 bytes/entry) and
/// available ring (a `u16` flags/idx pair, `num` `u16` entries, and a
/// trailing `u16` event-index field) come first, rounded up to
/// `align` (so the used ring that follows starts on its own alignment
/// boundary - `VRING_ALIGN`=4096 for the legacy interface); then the
/// used ring (a `u16` flags/idx pair, `num` 8-byte entries, and a
/// trailing `u16` event-index field).
fn vring_size(num: u64, align: u64) -> u64 {
    let desc_and_avail = 16 * num + 2 * (num + 3);
    let aligned = desc_and_avail.div_ceil(align) * align;
    aligned + 2 * 3 + 8 * num
}

/// Allocates `count` frames and verifies they landed contiguously (each
/// exactly `FRAME_SIZE` after the previous) before trusting the result.
/// The current frame allocator is a simple bump allocator (confirmed by
/// Fase 21's own self-test: sequential calls return frames exactly
/// 4 KiB apart), so this holds in practice - but a 256-entry virtqueue
/// (needing several frames, see `init_tx_queue`) is the first thing in
/// this kernel to actually NEED that property, so it's checked
/// explicitly here rather than trusted silently - a genuinely
/// different allocation shape than `net::e1000`'s own rings, which
/// always fit in a single frame and never exercised this at all.
fn allocate_contiguous_frames(count: u64) -> Result<u64, &'static str> {
    let first = crate::memory::frame_allocator::allocate_frame();
    let first_addr = first.start_address().as_u64();
    let mut expected_next = first_addr + FRAME_SIZE;
    for _ in 1..count {
        let next = crate::memory::frame_allocator::allocate_frame();
        if next.start_address().as_u64() != expected_next {
            return Err("virtio: frame allocator did not return contiguous frames");
        }
        expected_next += FRAME_SIZE;
    }
    Ok(first_addr)
}

pub struct TxQueueInfo {
    pub queue_num: u16,
    pub frames_needed: u64,
    pub pfn: u32,
    pub pfn_readback: u32,
    pub final_status: u8,
    pub io_base: u16,
    pub virt_base: u64,
}

/// Offsets of the descriptor table, available ring, and used ring
/// within a virtqueue's memory, relative to its own base address -
/// the exact same layout `vring_size` computes the total size for
/// (see that function's own doc), just returning the individual
/// pieces `send_test_frame` needs to actually read/write them.
fn vring_offsets(queue_num: u64, align: u64) -> (u64, u64, u64) {
    let desc_offset = 0u64;
    let avail_offset = 16 * queue_num;
    let avail_end = avail_offset + 2 * (queue_num + 3);
    let used_offset = avail_end.div_ceil(align) * align;
    (desc_offset, avail_offset, used_offset)
}

/// Completes the legacy VirtIO device-initialization handshake
/// (`ACKNOWLEDGE` -> `DRIVER` -> ... -> `DRIVER_OK`, the real status
/// bits from `include/uapi/linux/virtio_config.h`) and sets up ONE real
/// virtqueue - the TX queue, index 1 by virtio-net's own convention
/// (`VIRTIO_PCI_QUEUE_SEL` selects which queue every subsequent
/// `QUEUE_NUM`/`QUEUE_PFN` access refers to). Declines every optional
/// feature (`GUEST_FEATURES=0`) - correct and sufficient for proving
/// basic virtqueue setup, not yet negotiating checksum offload/TSO/etc.
///
/// Mirrors `net::e1000`'s own next-stage-after-probe shape (Fase 22's
/// TX descriptor ring) but for a genuinely different hardware protocol:
/// no descriptor ring built or frame sent yet, just proving the
/// virtqueue's *memory* is correctly sized, allocated, zeroed, and
/// accepted by the device - actually building and sending a frame
/// through it is separate, substantial follow-on work (the same
/// multi-Fase shape `net::e1000`'s own TX needed).
///
/// The memory needs real care picking a size: `QUEUE_NUM` (read back,
/// not assumed - confirmed empirically at 256 for this QEMU version)
/// fixes the descriptor count the device expects, and the legacy
/// interface has no way for the driver to negotiate a smaller
/// virtqueue - so the full `vring_size(256, 4096)` (10246 bytes) must
/// be allocated, needing 3 physical frames, not 1 - the first time
/// this kernel's frame allocator has needed to hand back *contiguous*
/// memory across several calls (see `allocate_contiguous_frames`).
pub fn init_tx_queue() -> Result<TxQueueInfo, &'static str> {
    let (dev, io_base) = find_io_base()?;
    // The same real, standard PCI requirement `net::e1000` needed
    // (Fase 44) and had never done before that: a freshly enumerated
    // device starts with DMA (bus mastering) disabled, so without this
    // the device could complete every register-level handshake step
    // yet still never actually be able to read the virtqueue memory
    // itself via DMA.
    dev.enable_bus_mastering();

    unsafe {
        write_port_u8(io_base, VIRTIO_PCI_STATUS, VIRTIO_STATUS_ACKNOWLEDGE);
        write_port_u8(
            io_base,
            VIRTIO_PCI_STATUS,
            VIRTIO_STATUS_ACKNOWLEDGE | VIRTIO_STATUS_DRIVER,
        );
        write_port_u32(io_base, VIRTIO_PCI_GUEST_FEATURES, 0);
        write_port_u16(io_base, VIRTIO_PCI_QUEUE_SEL, TX_QUEUE_INDEX);
    }

    let queue_num = unsafe { read_port_u16(io_base, VIRTIO_PCI_QUEUE_NUM) };
    if queue_num == 0 {
        return Err("virtio: TX queue_num read back as 0 - queue not available");
    }

    let size = vring_size(queue_num as u64, VRING_ALIGN);
    let frames_needed = size.div_ceil(FRAME_SIZE);
    let phys_base = allocate_contiguous_frames(frames_needed)?;

    let phys_offset = PHYS_MEM_OFFSET.load(Ordering::Relaxed);
    let virt_base_ptr = (phys_offset + phys_base) as *mut u8;
    unsafe {
        core::ptr::write_bytes(virt_base_ptr, 0, (frames_needed * FRAME_SIZE) as usize);
    }

    let pfn = (phys_base / FRAME_SIZE) as u32;
    unsafe {
        write_port_u32(io_base, VIRTIO_PCI_QUEUE_PFN, pfn);
    }
    let pfn_readback = unsafe { read_port_u32(io_base, VIRTIO_PCI_QUEUE_PFN) };

    unsafe {
        write_port_u8(
            io_base,
            VIRTIO_PCI_STATUS,
            VIRTIO_STATUS_ACKNOWLEDGE | VIRTIO_STATUS_DRIVER | VIRTIO_STATUS_DRIVER_OK,
        );
    }
    let final_status = unsafe { read_port_u8(io_base, VIRTIO_PCI_STATUS) };

    Ok(TxQueueInfo {
        queue_num,
        frames_needed,
        pfn,
        pfn_readback,
        final_status,
        io_base,
        virt_base: virt_base_ptr as u64,
    })
}

pub struct TxSendInfo {
    pub used_idx_before: u16,
    pub used_idx_after: u16,
    pub used_elem_id: u32,
    pub used_elem_len: u32,
}

/// Builds a real 10-byte legacy `virtio_net_hdr` (`include/uapi/linux/
/// virtio_net.h`, all-zero: no offload/segmentation - correct given
/// `init_tx_queue` declined every optional feature) followed by a
/// minimal broadcast Ethernet frame, in a freshly allocated buffer
/// frame (separate from the virtqueue's own memory, the same
/// descriptor-plus-data-buffer split `net::e1000`'s own rings use).
/// Writes ONE descriptor (read-only, no chaining), publishes it via
/// the available ring (`avail->ring[avail->idx % queue_num] =
/// descriptor index`, THEN `avail->idx += 1` - ring entry before index,
/// so the device can never observe an advanced index pointing at an
/// unwritten slot), kicks the device via `QUEUE_NOTIFY`, and polls the
/// used ring's `idx` for it to advance - the VirtIO equivalent of
/// `net::e1000::send_test_frame`'s own `TDH`-advancing proof, though
/// genuinely a different protocol shape (an index into a ring of
/// completions, not a single hardware head-pointer register).
pub fn send_test_frame(info: &TxQueueInfo) -> Result<TxSendInfo, &'static str> {
    let (_dev, io_base) = find_io_base()?;
    let mac = read_mac_at(io_base, VIRTIO_PCI_CONFIG_OFF_NO_MSIX);

    let payload = b"agentos virtio tx test";
    let frame_len = 14 + payload.len(); // Ethernet header + payload
    let buf_len = VIRTIO_NET_HDR_LEN + frame_len;

    let buf_frame = crate::memory::frame_allocator::allocate_frame();
    let buf_phys = buf_frame.start_address().as_u64();
    let phys_offset = PHYS_MEM_OFFSET.load(Ordering::Relaxed);
    let buf_virt = (phys_offset + buf_phys) as *mut u8;

    unsafe {
        core::ptr::write_bytes(buf_virt, 0, VIRTIO_NET_HDR_LEN); // virtio_net_hdr, all zero
        let eth = buf_virt.add(VIRTIO_NET_HDR_LEN);
        core::ptr::write_bytes(eth, 0xFF, 6); // broadcast destination
        core::ptr::copy_nonoverlapping(mac.as_ptr(), eth.add(6), 6); // real source MAC
        let ethertype: [u8; 2] = 0x88B5u16.to_be_bytes(); // IEEE 802 local experimental
        core::ptr::copy_nonoverlapping(ethertype.as_ptr(), eth.add(12), 2);
        core::ptr::copy_nonoverlapping(payload.as_ptr(), eth.add(14), payload.len());
    }

    let queue_num = info.queue_num as u64;
    let (desc_off, avail_off, used_off) = vring_offsets(queue_num, VRING_ALIGN);
    let desc_ptr = (info.virt_base + desc_off) as *mut u8;
    let avail_ptr = (info.virt_base + avail_off) as *mut u16;
    let used_ptr = (info.virt_base + used_off) as *mut u8;

    unsafe {
        // vring_desc { addr: u64, len: u32, flags: u16, next: u16 }
        core::ptr::write_volatile(desc_ptr as *mut u64, buf_phys);
        core::ptr::write_volatile(desc_ptr.add(8) as *mut u32, buf_len as u32);
        core::ptr::write_volatile(desc_ptr.add(12) as *mut u16, 0); // flags: read-only, no NEXT
        core::ptr::write_volatile(desc_ptr.add(14) as *mut u16, 0); // next: unused

        // vring_avail { flags: u16, idx: u16, ring: [u16; num] }
        let avail_idx = core::ptr::read_volatile(avail_ptr.add(1));
        let ring_slot = (avail_idx as u64 % queue_num) as usize;
        core::ptr::write_volatile(avail_ptr.add(2 + ring_slot), 0u16); // descriptor 0
        core::ptr::write_volatile(avail_ptr.add(1), avail_idx.wrapping_add(1));
    }

    // vring_used { flags: u16, idx: u16, ring: [{id: u32, len: u32}; num] }
    let used_idx_ptr = unsafe { (used_ptr as *mut u16).add(1) };
    let used_idx_before = unsafe { core::ptr::read_volatile(used_idx_ptr) };

    unsafe {
        write_port_u16(io_base, VIRTIO_PCI_QUEUE_NOTIFY, TX_QUEUE_INDEX);
    }

    let mut used_idx_after = used_idx_before;
    for _ in 0..1_000_000u32 {
        used_idx_after = unsafe { core::ptr::read_volatile(used_idx_ptr) };
        if used_idx_after != used_idx_before {
            break;
        }
    }

    // past flags+idx (4 bytes) to ring[0]'s {id, len}
    let used_elem_ptr = unsafe { (used_ptr as *mut u32).add(1) };
    let used_elem_id = unsafe { core::ptr::read_volatile(used_elem_ptr) };
    let used_elem_len = unsafe { core::ptr::read_volatile(used_elem_ptr.add(1)) };

    Ok(TxSendInfo {
        used_idx_before,
        used_idx_after,
        used_elem_id,
        used_elem_len,
    })
}
