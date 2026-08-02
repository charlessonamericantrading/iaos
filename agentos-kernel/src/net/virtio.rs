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

const RX_QUEUE_INDEX: u16 = 0;
const TX_QUEUE_INDEX: u16 = 1;
const VRING_ALIGN: u64 = 4096;
const FRAME_SIZE: u64 = 4096;
const VIRTIO_NET_HDR_LEN: usize = 10; // legacy virtio_net_hdr, no MRG_RXBUF

/// Marks a descriptor's buffer as one the DEVICE writes into, the
/// opposite direction from a plain (read-only) TX descriptor - verified
/// against the Linux kernel's own real `include/uapi/linux/
/// virtio_ring.h` rather than assumed. RX queue index 0 (above) is
/// likewise confirmed, not assumed, against the Linux virtio_net
/// driver's own `rxq2vq(0) = 0` / `txq2vq(0) = 1` (`drivers/net/
/// virtio_net.c`) - exactly matching this module's existing
/// `TX_QUEUE_INDEX = 1`.
const VRING_DESC_F_WRITE: u16 = 2;

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

pub struct RxQueueInfo {
    pub queue_num: u16,
    pub frames_needed: u64,
    pub pfn: u32,
    pub pfn_readback: u32,
    pub io_base: u16,
    pub virt_base: u64,
    pub buf_phys: u64,
    pub buf_virt: u64,
    pub buf_len: usize,
    pub desc_flags_readback: u16,
    pub avail_idx_after: u16,
}

/// Sets up the RX virtqueue (index 0) and arms ONE real, write-only
/// receive descriptor - the RX equivalent of `init_tx_queue`'s own
/// scope (Fase 63): proving the queue's memory is correctly sized,
/// allocated, and accepted by the device, and that a real descriptor
/// can be published into its available ring, but not yet triggering or
/// observing an actual received frame - that's separate, substantial
/// follow-on work, matching the same two-step progression this
/// module's own TX side already took (Fase 63 -> 64).
///
/// Deliberately called AFTER `init_tx_queue` has already completed the
/// full `ACKNOWLEDGE -> DRIVER -> DRIVER_OK` handshake, and does NOT
/// re-touch `STATUS` at all here: only writing `STATUS=0` means
/// "reset" a legacy VirtIO device, and re-selecting a queue via
/// `QUEUE_SEL`/`QUEUE_PFN` is a per-queue operation, not gated on the
/// status handshake having just completed. This is verified
/// empirically below (`pfn_ok`), not just assumed spec-compliant -
/// real drivers are recommended to size every virtqueue before
/// signaling `DRIVER_OK`, but nothing in this Fase's own boot-test
/// suggested QEMU's legacy `virtio-pci` implementation actually
/// enforces that ordering.
///
/// The one genuinely new piece beyond `init_tx_queue`'s own shape: the
/// descriptor's `flags` field is written as `VRING_DESC_F_WRITE` (not
/// `0`, which every TX descriptor so far has used) - telling the
/// device this descriptor's buffer is for the DEVICE to write data
/// INTO, the opposite direction from TX. `desc_flags_readback` reads
/// the flags word back from the virtqueue's own memory afterward as a
/// real check that the write landed correctly, not just that it didn't
/// crash.
pub fn init_rx_queue(tx_info: &TxQueueInfo) -> Result<RxQueueInfo, &'static str> {
    let io_base = tx_info.io_base;

    unsafe {
        write_port_u16(io_base, VIRTIO_PCI_QUEUE_SEL, RX_QUEUE_INDEX);
    }
    let queue_num = unsafe { read_port_u16(io_base, VIRTIO_PCI_QUEUE_NUM) };
    if queue_num == 0 {
        return Err("virtio: RX queue_num read back as 0 - queue not available");
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

    // A fresh, separate buffer frame the DEVICE will write a received
    // frame into - large enough for a full legacy virtio_net_hdr (10
    // bytes) plus a maximum-size Ethernet frame (1514 bytes without a
    // trailing FCS), rounded up generously while still fitting a
    // single 4 KiB frame.
    let buf_len: usize = 2048;
    let buf_frame = crate::memory::frame_allocator::allocate_frame();
    let buf_phys = buf_frame.start_address().as_u64();
    let buf_virt_ptr = (phys_offset + buf_phys) as *mut u8;
    unsafe {
        core::ptr::write_bytes(buf_virt_ptr, 0, buf_len);
    }

    let queue_num_u64 = queue_num as u64;
    let (desc_off, avail_off, _used_off) = vring_offsets(queue_num_u64, VRING_ALIGN);
    let desc_ptr = (virt_base_ptr as u64 + desc_off) as *mut u8;
    let avail_ptr = (virt_base_ptr as u64 + avail_off) as *mut u16;

    unsafe {
        // vring_desc { addr: u64, len: u32, flags: u16, next: u16 }
        core::ptr::write_volatile(desc_ptr as *mut u64, buf_phys);
        core::ptr::write_volatile(desc_ptr.add(8) as *mut u32, buf_len as u32);
        core::ptr::write_volatile(desc_ptr.add(12) as *mut u16, VRING_DESC_F_WRITE);
        core::ptr::write_volatile(desc_ptr.add(14) as *mut u16, 0); // next: unused
    }
    let desc_flags_readback = unsafe { core::ptr::read_volatile(desc_ptr.add(12) as *mut u16) };

    unsafe {
        // vring_avail { flags: u16, idx: u16, ring: [u16; num] }
        let avail_idx = core::ptr::read_volatile(avail_ptr.add(1));
        let ring_slot = (avail_idx as u64 % queue_num_u64) as usize;
        core::ptr::write_volatile(avail_ptr.add(2 + ring_slot), 0u16); // descriptor 0
        core::ptr::write_volatile(avail_ptr.add(1), avail_idx.wrapping_add(1));
    }
    let avail_idx_after = unsafe { core::ptr::read_volatile(avail_ptr.add(1)) };

    // Tells the device a receive buffer is now available, the same
    // notify-after-publish requirement TX's own send_test_frame
    // follows - without this, the device has no reason to look at the
    // avail ring at all before some later, unrelated event.
    unsafe {
        write_port_u16(io_base, VIRTIO_PCI_QUEUE_NOTIFY, RX_QUEUE_INDEX);
    }

    Ok(RxQueueInfo {
        queue_num,
        frames_needed,
        pfn,
        pfn_readback,
        io_base,
        virt_base: virt_base_ptr as u64,
        buf_phys,
        buf_virt: buf_virt_ptr as u64,
        buf_len,
        desc_flags_readback,
        avail_idx_after,
    })
}

pub struct RxRecvInfo {
    pub used_idx_before: u16,
    pub used_idx_after: u16,
    pub received_len: u32,
    pub is_arp_reply: bool,
    pub gateway_ip: Option<[u8; 4]>,
}

/// Triggers a real ARP request through the TX queue - the exact same
/// request byte layout `net::e1000::send_test_frame` already builds and
/// has already proven SLIRP answers ("who has 10.0.2.2? tell 10.0.2.15",
/// SLIRP's default gateway/guest addresses) - then polls the RX queue
/// `init_tx_queue`/`init_rx_queue` (Fase 63/65) already set up and armed,
/// attempting the first complete, real VirtIO-net round trip this
/// kernel has tried, matching `net::e1000`'s own Fase 45
/// `receive_test_frame`.
///
/// **Honest status (Fase 82 - the mystery is now FULLY RESOLVED, not
/// merely narrowed)**: Fase 78 fixed the ORIGINAL Fase 66 mystery ("the
/// armed buffer stays entirely untouched") by re-arming a fresh
/// descriptor here before this function's own request goes out. What
/// Fase 78 left open - "a second, narrower mystery": a byte-perfect,
/// TX-confirmed second request that never seemed to get a reply - turned
/// out to have NOTHING to do with QEMU, SLIRP, or virtqueue descriptor
/// recycling, despite three further investigation rounds (78 -> 79
/// technically unrelated ring-3 work in between, then three dedicated
/// VirtIO rounds) that read every line of the real, pinned-revision
/// QEMU/libslirp source in the actual delivery path and found it all
/// correct and stateless.
///
/// **The real bug, found only once kernel-side polling was cross-checked
/// against genuinely independent, device-external evidence**: a real
/// packet capture (QEMU's own `-object filter-dump` attached to the
/// virtio-net netdev) showed SLIRP replying in under 100 MICROSECONDS -
/// the reply was never missing. A QEMU-side virtqueue trace
/// (`-trace events=virtqueue_fill,virtqueue_flush,...`) then showed
/// QEMU's own `virtqueue_flush` for the RX queue genuinely firing, for
/// real, exactly once. Lining that up against this function's OWN
/// source order (as it stood before this fix) revealed the actual bug:
/// `used_idx_before` for the RX side was sampled AFTER this function's
/// own TX notify (and after that TX completion was even polled and
/// confirmed) - not before. SLIRP is an in-process, fully synchronous
/// backend with no real async socket I/O needed for ARP, so the ENTIRE
/// request -> SLIRP -> reply -> RX-delivery chain can complete inside
/// the single `out` instruction that notifies the TX queue, before that
/// instruction even returns to this kernel. By the time the OLD code
/// sampled `used_idx_before`, the one-and-only completion had ALREADY
/// happened - the wait loop was watching for a change that had already
/// occurred, so it could only ever time out, no matter how long it
/// waited (the ~5s vs ~22s test from Fase 78's own notes already hinted
/// at this: a race that occurs is either always-too-late or
/// always-in-time, never "needs more patience").
///
/// **The fix**: `used_idx_before` is now sampled immediately after the
/// RX re-arm above, before this function builds or sends its own
/// request at all - see the inline comment at that capture site.
/// Verified (see the module's own toolchain notes for the exact
/// dual-explicit-`-netdev` local diagnostic command): `is_arp_reply=true`,
/// `gateway_ip=Some([10, 0, 2, 2])` - the actual success path, reached
/// for the first time in this mystery's whole 5-round history.
///
/// **Update (Fase 82)**: the kernel's own standard boot command
/// (`kernel-ci.yml`'s invocation, matching `boot_kernel.bat`) now DOES
/// give virtio-net-pci its own explicit `-netdev user`, alongside
/// e1000's own explicit `-netdev` (previously relying on QEMU's one
/// implicit default, always claimed by e1000, which is what left this
/// device with no working backend at all under CI). This was the
/// follow-on work this paragraph used to describe as not yet bundled in -
/// verified locally first (3 repeated boots under this exact dual-netdev
/// config, full-suite regression unchanged, e1000's own PCI slot/MAC/
/// every existing e1000 test untouched) before landing in CI, where this
/// device's own second-round-trip test (this fix) now has a real backend
/// to actually run against.
///
/// Deliberately a NEW, independent function rather than reusing or
/// refactoring `send_test_frame` - its exact behavior and log output are
/// already CI-asserted (Fase 64), so this duplicates its small TX-publish
/// pattern instead of risking any change to it, the same reasoning
/// `net::e1000::arp_resolve`'s own module doc gives for making the same
/// choice there. Uses descriptor index 1 in the TX queue (distinct from
/// `send_test_frame`'s own descriptor 0) - the avail ring's current
/// `idx` (already 1, from `send_test_frame`'s own earlier publish this
/// same boot) naturally lands this second entry at ring position 1
/// without needing any special-casing.
pub fn receive_test_frame(
    tx_info: &TxQueueInfo,
    rx_info: &RxQueueInfo,
) -> Result<RxRecvInfo, &'static str> {
    let mac = read_mac_at(tx_info.io_base, VIRTIO_PCI_CONFIG_OFF_NO_MSIX);

    // Fase 78 fix (full reasoning in this function's own module doc
    // above): re-arm a fresh descriptor into the RX avail ring, BEFORE
    // building/sending this function's own ARP request below - not
    // after, which was a real, self-caught race in an earlier version
    // of this same fix (a fast-arriving reply could otherwise land
    // with no available descriptor yet and be silently dropped).
    let (_rx_desc_off, rx_avail_off, rx_used_off) =
        vring_offsets(rx_info.queue_num as u64, VRING_ALIGN);
    let rx_avail_ptr = (rx_info.virt_base + rx_avail_off) as *mut u16;
    unsafe {
        core::ptr::write_bytes(rx_info.buf_virt as *mut u8, 0, rx_info.buf_len);
        let avail_idx = core::ptr::read_volatile(rx_avail_ptr.add(1));
        let ring_slot = (avail_idx as u64 % rx_info.queue_num as u64) as usize;
        core::ptr::write_volatile(rx_avail_ptr.add(2 + ring_slot), 0u16); // descriptor 0
        core::ptr::write_volatile(rx_avail_ptr.add(1), avail_idx.wrapping_add(1));
        write_port_u16(tx_info.io_base, VIRTIO_PCI_QUEUE_NOTIFY, RX_QUEUE_INDEX);
    }

    // Fase 82 fix (full reasoning in this function's own module doc above):
    // capture the RX used_idx BASELINE here, right after the re-arm above -
    // BEFORE building/sending the ARP request below, not after confirming
    // its own TX completion like an earlier version of this function did.
    let rx_used_ptr = (rx_info.virt_base + rx_used_off) as *mut u8;
    let rx_used_idx_ptr = unsafe { (rx_used_ptr as *mut u16).add(1) };
    let used_idx_before = unsafe { core::ptr::read_volatile(rx_used_idx_ptr) };

    let frame_len = 42usize; // 14-byte Ethernet header + 28-byte ARP payload
    let buf_len = VIRTIO_NET_HDR_LEN + frame_len;
    let buf_frame = crate::memory::frame_allocator::allocate_frame();
    let buf_phys = buf_frame.start_address().as_u64();
    let phys_offset = PHYS_MEM_OFFSET.load(Ordering::Relaxed);
    let buf_virt = (phys_offset + buf_phys) as *mut u8;

    unsafe {
        core::ptr::write_bytes(buf_virt, 0, VIRTIO_NET_HDR_LEN); // virtio_net_hdr, all zero
        let eth = buf_virt.add(VIRTIO_NET_HDR_LEN);
        core::ptr::write_bytes(eth, 0xFF, 6); // broadcast dest
        core::ptr::copy_nonoverlapping(mac.as_ptr(), eth.add(6), 6); // real source MAC
        core::ptr::write_volatile(eth.add(12), 0x08);
        core::ptr::write_volatile(eth.add(13), 0x06); // EtherType: ARP
        core::ptr::write_volatile(eth.add(14), 0x00);
        core::ptr::write_volatile(eth.add(15), 0x01); // HTYPE: Ethernet
        core::ptr::write_volatile(eth.add(16), 0x08);
        core::ptr::write_volatile(eth.add(17), 0x00); // PTYPE: IPv4
        core::ptr::write_volatile(eth.add(18), 6); // HLEN
        core::ptr::write_volatile(eth.add(19), 4); // PLEN
        core::ptr::write_volatile(eth.add(20), 0x00);
        core::ptr::write_volatile(eth.add(21), 0x01); // OPER: request
        core::ptr::copy_nonoverlapping(mac.as_ptr(), eth.add(22), 6); // SHA
        let spa: [u8; 4] = [10, 0, 2, 15]; // SPA (SLIRP guest default)
        core::ptr::copy_nonoverlapping(spa.as_ptr(), eth.add(28), 4);
        core::ptr::write_bytes(eth.add(32), 0x00, 6); // THA: unknown
        let tpa: [u8; 4] = [10, 0, 2, 2]; // TPA (SLIRP gateway default)
        core::ptr::copy_nonoverlapping(tpa.as_ptr(), eth.add(38), 4);
    }

    let queue_num = tx_info.queue_num as u64;
    let (desc_off, avail_off, used_off) = vring_offsets(queue_num, VRING_ALIGN);
    let desc_ptr = (tx_info.virt_base + desc_off) as *mut u8;
    let avail_ptr = (tx_info.virt_base + avail_off) as *mut u16;
    let used_ptr = (tx_info.virt_base + used_off) as *mut u8;

    const ARP_DESC_INDEX: u16 = 1;
    unsafe {
        let arp_desc_ptr = desc_ptr.add(16 * ARP_DESC_INDEX as usize);
        core::ptr::write_volatile(arp_desc_ptr as *mut u64, buf_phys);
        core::ptr::write_volatile(arp_desc_ptr.add(8) as *mut u32, buf_len as u32);
        core::ptr::write_volatile(arp_desc_ptr.add(12) as *mut u16, 0); // read-only, no NEXT
        core::ptr::write_volatile(arp_desc_ptr.add(14) as *mut u16, 0); // next: unused

        let avail_idx = core::ptr::read_volatile(avail_ptr.add(1));
        let ring_slot = (avail_idx as u64 % queue_num) as usize;
        core::ptr::write_volatile(avail_ptr.add(2 + ring_slot), ARP_DESC_INDEX);
        core::ptr::write_volatile(avail_ptr.add(1), avail_idx.wrapping_add(1));
    }

    // Confirm the request actually went out - a local DMA/descriptor
    // completion, not a network round trip, so a tight bounded poll is
    // appropriate here (the same convention send_test_frame's own TX
    // completion check already uses).
    let tx_used_idx_ptr = unsafe { (used_ptr as *mut u16).add(1) };
    let tx_used_idx_before = unsafe { core::ptr::read_volatile(tx_used_idx_ptr) };
    unsafe {
        write_port_u16(tx_info.io_base, VIRTIO_PCI_QUEUE_NOTIFY, TX_QUEUE_INDEX);
    }
    let mut tx_used_idx_after = tx_used_idx_before;
    for _ in 0..1_000_000u32 {
        tx_used_idx_after = unsafe { core::ptr::read_volatile(tx_used_idx_ptr) };
        if tx_used_idx_after != tx_used_idx_before {
            break;
        }
    }
    let tx_ok = tx_used_idx_after != tx_used_idx_before;
    serial_println!(
        "[VIRTIO] rx_test tx_phase={}",
        if tx_ok { "ok" } else { "failed" }
    );
    if !tx_ok {
        return Err("virtio: RX test's own ARP request TX never completed");
    }

    // Now wait for SLIRP's reply to land in the freshly re-armed RX
    // buffer above - a genuine network round trip, so this yields the
    // CPU via hlt() for real wall-clock time (~90 ticks, about 5
    // seconds at this kernel's ~18.2Hz rate) rather than a tight
    // busy-spin, the same reasoning net::e1000::receive_test_frame
    // already established for the identical kind of wait. `used_idx_before`
    // was already captured above, BEFORE the TX request went out (Fase 82) -
    // see this function's own module doc for why that ordering is what
    // actually matters here.
    let start_tick = crate::interrupts::timer_ticks();
    let used_idx_after = loop {
        let current = unsafe { core::ptr::read_volatile(rx_used_idx_ptr) };
        if current != used_idx_before || crate::interrupts::timer_ticks() - start_tick > 90 {
            break current;
        }
        x86_64::instructions::hlt();
    };

    if used_idx_after == used_idx_before {
        // Diagnostic left in place for whoever resumes this: the used
        // ring never advancing could mean the reply never arrived at
        // all, or that it arrived but something about how this queue
        // was armed keeps the device from ever marking it used - these
        // look identical from `used_idx` alone, so dump the ISR
        // (cleared by this read - fine, only read on this failure
        // path) and the RX descriptor's own raw memory (readable back
        // regardless of whether the device ever touched it) to tell
        // them apart without needing to re-add debug prints from
        // scratch.
        let isr = unsafe { read_port_u8(tx_info.io_base, VIRTIO_PCI_ISR) };
        let (rx_desc_off, rx_avail_off, _rx_used_off2) =
            vring_offsets(rx_info.queue_num as u64, VRING_ALIGN);
        let rx_desc_ptr = (rx_info.virt_base + rx_desc_off) as *const u8;
        let raw_desc = unsafe { core::slice::from_raw_parts(rx_desc_ptr, 16) };
        let raw_buf_head =
            unsafe { core::slice::from_raw_parts(rx_info.buf_virt as *const u8, 16) };
        // used_idx_before is the value this SAME call's own poll loop
        // started from, captured right after the re-arm above, before
        // this function's own request even went out (Fase 82) - printing
        // it tells us whether the ring was already sitting past 0 at that
        // point (meaning something completed even before this function's
        // own request was sent) versus genuinely stuck at 0 (no reception
        // at all, ever, by the time this diagnostic runs).
        let rx_avail_ptr = (rx_info.virt_base + rx_avail_off) as *const u16;
        let avail_idx_now = unsafe { core::ptr::read_volatile(rx_avail_ptr.add(1)) };
        serial_println!(
            "[VIRTIO] rx timeout: isr={:#04x} used_idx_before={} used_idx_after={} avail_idx_now={} raw_desc0={:02x?} raw_buf_head={:02x?}",
            isr,
            used_idx_before,
            used_idx_after,
            avail_idx_now,
            raw_desc,
            raw_buf_head
        );
        return Err("virtio: RX timed out waiting for a received frame");
    }

    // past flags+idx (4 bytes) to ring[0]'s {id, len} - descriptor 0 is
    // the only one init_rx_queue ever armed.
    let rx_used_elem_ptr = unsafe { (rx_used_ptr as *mut u32).add(1) };
    let received_len = unsafe { core::ptr::read_volatile(rx_used_elem_ptr.add(1)) };

    // The device writes its own virtio_net_hdr (10 bytes) ahead of the
    // actual received Ethernet frame - skip past it, the RX mirror of
    // how send_test_frame's own TX buffer is laid out.
    let recv_buf = rx_info.buf_virt as *const u8;
    let eth_len = (received_len as usize).saturating_sub(VIRTIO_NET_HDR_LEN);
    let eth_data =
        unsafe { core::slice::from_raw_parts(recv_buf.add(VIRTIO_NET_HDR_LEN), eth_len) };

    // Same ARP-reply verification net::e1000::receive_test_frame
    // already established: OPER=0x0002 (reply, not request) and SHA
    // (the replying device's own MAC) matching the Ethernet header's
    // source MAC - real internal cross-consistency a malformed or
    // unrelated frame wouldn't have, not just "something arrived".
    let is_arp_reply = eth_data.len() >= 28
        && eth_data[12] == 0x08
        && eth_data[13] == 0x06
        && eth_data[20] == 0x00
        && eth_data[21] == 0x02
        && eth_data[22..28] == eth_data[6..12];
    let gateway_ip = if eth_data.len() >= 32 {
        Some([eth_data[28], eth_data[29], eth_data[30], eth_data[31]])
    } else {
        None
    };

    Ok(RxRecvInfo {
        used_idx_before,
        used_idx_after,
        received_len,
        is_arp_reply,
        gateway_ip,
    })
}
