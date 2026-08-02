//! Real e1000 NIC access. `net::virtio_net` remains fully simulated - this
//! module talks to actual hardware. QEMU's default machine (no extra
//! `-device` flags) exposes a real Intel 8254x-family NIC at PCI
//! vendor:device 8086:100e - confirmed present via `pci.rs`'s real
//! enumeration (see README/Fase 11).
//!
//! Stages so far:
//! - `probe()` (Fase 19): find the device, reach its registers, read a
//!   couple back (STATUS, MAC via RAL0/RAH0) - proof this kernel can
//!   genuinely talk to real device MMIO at all.
//! - `send_test_frame()` (Fase 22, **fully confirmed working Fase 44**):
//!   build a real transmit descriptor ring and hand a frame to the
//!   hardware. Register offsets and the legacy TX descriptor layout were
//!   cross-checked against a real shipped driver (MINIX's `e1000_reg.h`)
//!   and QEMU's own `e1000.c`/`e1000_regs.h` rather than relied on from
//!   memory alone, given the cost of getting hardware-register-level
//!   code wrong. See "TX DD-bit mystery, resolved (Fase 44)" below for
//!   the two-round investigation and its actual resolution.
//! - `receive_test_frame()` (Fase 45): a real receive descriptor ring,
//!   proven by genuinely receiving a real ARP reply from QEMU's SLIRP
//!   backend on the very first attempt.
//! - `arp_resolve()` (Fase 48): generalizes that same TX+RX round trip
//!   to resolve *any* target IP's MAC, not just SLIRP's hardcoded
//!   gateway - the prerequisite a real `ping` (see `net::icmp`, Fase 47)
//!   needs before it can address an Ethernet frame at all.
//! - `ping()` (Fase 49): converges `arp_resolve` and `net::icmp` - the
//!   first complete, real, routable IP packet this kernel has ever
//!   assembled and sent, not just a raw Ethernet-level test frame.
//! - `tcp_syn_test()`/`tcp_echo_test()` (Fase 89/90, graceful FIN close
//!   added Fase 102): a real TCP three-way handshake, data exchange, and
//!   connection close over `net::tcp`, reusing this module's own
//!   `arp_resolve`-then-send-real-IP-packet pattern.
//! - `dns_query_test()` (Fase 106): a real UDP round trip carrying a DNS
//!   query over `net::dns`/`net::udp`, the same pattern again one layer
//!   up the stack.
//! - `resolve_hostname()` (Fase 113): wraps `dns_query_test`'s own
//!   underlying round trip into a name -> `[u8; 4]` API, reused by the
//!   `ping <hostname>` shell command.
//!
//! ## Why no new page-table mapping code was needed
//! The e1000's BAR0 is a *memory-mapped* register window (not I/O-port
//! based) - normally reaching that means mapping its physical address to
//! a virtual one first. But this kernel already identity-maps the entire
//! physical address space at a fixed offset (`PHYS_MEM_OFFSET`, set from
//! `BootInfo::physical_memory_offset` - see `vga_buffer.rs`, which already
//! relies on this exact mechanism to reach the VGA text buffer's physical
//! address 0xb8000). Since 0xb8000 is itself outside any BIOS-reported
//! "usable RAM" region and that already works, this reuses the identical,
//! already-proven technique instead of writing new mapping code -
//! confirmed to actually work for *this* physical address by booting and
//! reading real register values below, not just assumed by analogy. The
//! TX descriptor ring and packet buffer below reuse it too, for the exact
//! same reason: `memory::frame_allocator::allocate_frame()` (Fase 21)
//! hands back a physical frame, and `PHYS_MEM_OFFSET + that address` is
//! already a valid, mapped virtual address to write through - no dedicated
//! DMA-region mapping code needed.
//!
//! ## TX DD-bit mystery, resolved (Fase 44) - two-round investigation kept below
//! `send_test_frame` genuinely engages the hardware: the descriptor ring's
//! physical address, the descriptor's own content (verified correct via a
//! direct memory read-back before the kick), and the TDT-triggered dequeue
//! all work - **TDH provably advances from 0 to 1 the instant TDT is
//! written**, proof the device really read the descriptor off the ring.
//! What does *not* happen, in every variant tried locally: the
//! descriptor's `status` byte (the DD/Descriptor-Done bit `TXD_STATUS_DD`)
//! never gets written back - not even a partial write; a full 16-byte raw
//! dump after timeout shows every byte exactly as this code left it,
//! `status` included.
//!
//! **Ruled out in Fase 22**, each with its own real test, not just
//! reasoning: wrong register offsets (cross-checked against multiple
//! independent sources, confirmed via memory read-back); `TIPG`
//! misconfiguration (writes read back as `0` regardless - likely a no-op
//! in QEMU's model); a tight-spin-loop starving QEMU's own event loop
//! (tested by halting - interrupts enabled - between polls for ~11 real
//! seconds; no change); `CTRL.SLU` not set (already reads as set by
//! default); packet content being unrecognized by the SLIRP backend
//! (tested both an experimental EtherType and a fully valid ARP request -
//! identical result).
//!
//! **Ruled out in Fase 32**, this time by fetching QEMU's *complete*,
//! current `hw/net/e1000.c`/`e1000x_regs.h` source directly (not the
//! piecemeal targeted-question approach Fase 22 was limited to) and
//! reading `process_tx_desc`/`txdesc_writeback`/`start_xmit` in full:
//! - **Descriptor layout and every `TXD_CMD_*`/`TXD_STAT_DD` bit
//!   constant were checked byte-for-byte against QEMU's actual struct
//!   and `#define`s** - all match exactly (this crate's byte-level `cmd`/
//!   `status` constants and QEMU's `u32`-shifted equivalents describe the
//!   identical bits). Not the bug.
//! - **`TXDCTL`/`WTHRESH`** (a real quirk on actual Intel hardware and in
//!   QEMU's newer `igb` model - writeback can wait for a descriptor
//!   "batch") - checked directly in `txdesc_writeback`'s source: this
//!   basic e1000 model's writeback is unconditional once the descriptor's
//!   own `RS`/`RPS` bit is set, no `TXDCTL` consultation at all. Not
//!   applicable here.
//! - **Register-write dispatch, traced completely**: `TDT` and `TCTL`
//!   share the *same* write handler (`set_tctl`), so writing either one
//!   triggers `start_xmit()` - confirmed this kernel's write order (TDBAL/
//!   TDBAH/TDLEN/TDH/TDT=0/TIPG/TCTL, *then* TDT=1) correctly leaves
//!   `TCTL.EN` set and `TDT=1 != TDH=0` by the time the final write fires,
//!   which should reach `txdesc_writeback` with `RS` set. Not the bug, as
//!   far as this reasoning goes.
//! - **CPU cache coherency** (a genuinely new angle: this kernel identity-
//!   maps memory as normal cacheable RAM, and if QEMU is running with
//!   hardware-accelerated virtualization - WHPX on Windows is the default
//!   accelerator when no `-accel` flag is given - the guest CPU has real
//!   caching, and a DMA write from the emulated device could land in RAM
//!   without invalidating a stale value already in the polling CPU's
//!   cache) - tested directly with `_mm_clflush` before every poll read.
//!   No change. Not the bug (or at least, `clflush`-based invalidation
//!   doesn't fix it).
//! - **Deferred/asynchronous completion needing a real guest yield** -
//!   re-tested with a `hlt()`-based wait (interrupts enabled, ~5 real
//!   seconds via the timer tick counter, replacing the old tight busy-
//!   spin) rather than assuming Fase 22's shorter test still applies
//!   unchanged. No change - reconfirms Fase 22's finding rather than
//!   contradicting it.
//!
//! As of Fase 32's own close, every software-side explanation *within
//! this driver's own code* had been ruled out with a real experiment,
//! not just argued away - the reasoning traced the write-then-writeback
//! path completely and found no gap, yet the observed behavior
//! contradicted it. That contradiction was itself the honest state to
//! report at the time, rather than papered over.
//!
//! **The actual root cause (Fase 44): PCI Bus Mastering was never
//! enabled.** Found by reading QEMU's source for an *unrelated* reason -
//! investigating real RX support, whose own readiness check
//! (`e1000x_rx_ready` in `hw/net/e1000x_common.c`) explicitly requires
//! `d->config[PCI_COMMAND] & PCI_COMMAND_MASTER` - which led to tracing
//! how that same bit affects the *TX* path too, just far less visibly:
//! `hw/pci/pci.c`'s `pci_default_write_config` calls `pci_set_master`
//! whenever a config write touches the Command register, which toggles
//! `memory_region_set_enabled(&d->bus_master_enable_region, enable)` -
//! and *every* `pci_dma_read`/`pci_dma_write` call (`txdesc_writeback`'s
//! DD-bit write among them) routes through exactly that region via
//! `pci_get_address_space(dev)` → `dev->bus_master_as`. A real PCI/PCIe
//! device starts with bus mastering *disabled* after reset (a genuine
//! safety property, not a QEMU quirk - an unconfigured device shouldn't
//! be able to DMA into arbitrary memory), and system software is
//! expected to enable it explicitly once ready to drive the device. This
//! driver never had - `pci.rs` was read-only by design until Fase 44
//! added `PciDevice::enable_bus_mastering`, now called at the top of
//! `send_test_frame`, right after finding the device.
//!
//! This explains the exact symptom both investigation rounds
//! documented, precisely: `TDH` is an internal register QEMU updates
//! directly in its own state (`s->mac_reg[TDH]`), never a guest-memory
//! DMA write, so it was completely unaffected by bus mastering and
//! always advanced correctly - while the DD status bit is written back
//! via `pci_dma_write`, which silently had nowhere real to land with
//! `bus_master_enable_region` disabled. Confirmed, not just plausible:
//! adding the one `enable_bus_mastering()` call and nothing else made
//! `send_test_frame` succeed - `DD` sets, identically across 3 repeated
//! boots, zero other change to the ring/descriptor/register-write logic
//! extensively verified correct across both prior rounds. Neither
//! investigation round missed something *wrong* in this driver's own
//! code - both were correct that the write-then-writeback path, as
//! written, had no bug; the missing piece was a real device-bring-up
//! step this driver had simply never performed, one level below
//! anything either round's tracing had reason to inspect.
//!
//! **RX implication**: `e1000x_rx_ready`'s explicit `pci_master` check
//! (unlike TX's, buried in shared DMA plumbing rather than a named
//! condition) means bus mastering is very likely a hard prerequisite for
//! any future real RX work too, not just a TX fix - worth remembering
//! before assuming a from-scratch RX attempt would need its own,
//! separate investigation into a similar-looking stall.

use crate::pci::{self, PciDevice};
use crate::vga_buffer::PHYS_MEM_OFFSET;
use crate::{kprintln, serial_println};
use core::sync::atomic::Ordering;

const E1000_VENDOR_ID: u16 = 0x8086;
const E1000_DEVICE_ID: u16 = 0x100e;

const REG_CTRL: u64 = 0x0000;
const REG_STATUS: u64 = 0x0008;
const REG_RAL0: u64 = 0x5400;
const REG_RAH0: u64 = 0x5404;
const REG_TCTL: u64 = 0x0400;
const REG_TDBAL: u64 = 0x3800;
const REG_TDBAH: u64 = 0x3804;
const REG_TDLEN: u64 = 0x3808;
const REG_TDH: u64 = 0x3810;
const REG_TDT: u64 = 0x3818;
const REG_TIPG: u64 = 0x0410;

const CTRL_SLU: u32 = 1 << 6; // Set Link Up - standard bring-up bit, already set by default in local testing

const TCTL_EN: u32 = 1 << 1;
const TCTL_PSP: u32 = 1 << 3;
const TCTL_CT_DEFAULT: u32 = 0x0F << 4; // recommended collision threshold
const TCTL_COLD_FULL_DUPLEX: u32 = 0x40 << 12; // recommended collision distance, full duplex

// IEEE 802.3's standard Inter Packet Gap timing (IPGT=10, IPGR1=8, IPGR2=6,
// packed into TIPG's low three 10-bit fields). Written for correctness/
// completeness even though it reads back as 0 regardless in local testing
// - see the module doc's "TX DD-bit mystery, resolved" section.
const TIPG_DEFAULT: u32 = 0x0060_200A;

const TXD_CMD_EOP: u8 = 0x01; // End Of Packet - this is the last (only) descriptor of the frame
const TXD_CMD_IFCS: u8 = 0x02; // Insert FCS/CRC - let the hardware compute it
const TXD_CMD_RS: u8 = 0x08; // Report Status - hardware sets DD in `status` once processed
const TXD_STATUS_DD: u8 = 0x01; // Descriptor Done

const NUM_TX_DESCRIPTORS: usize = 8; // 8 * 16 bytes = 128, satisfies TDLEN's 128-byte-multiple rule

// RX registers - offsets verified (Fase 44) against QEMU's actual
// hw/net/e1000_regs.h/e1000x_regs.h, the same discipline Fase 22/32 used
// for the TX side.
const REG_RDBAL: u64 = 0x2800;
const REG_RDBAH: u64 = 0x2804;
const REG_RDLEN: u64 = 0x2808;
const REG_RDH: u64 = 0x2810;
const REG_RDT: u64 = 0x2818;
const REG_RCTL: u64 = 0x0100;

const RCTL_EN: u32 = 1 << 1; // Receiver Enable
const RCTL_BAM: u32 = 1 << 15; // Broadcast Accept Mode - defensive; a unicast
                               // reply to our own (already-filtered) MAC
                               // shouldn't need this, but costs nothing
const RCTL_SECRC: u32 = 1 << 26; // Strip Ethernet CRC - simplifies reading
                                 // a received frame back (no trailing
                                 // 4-byte FCS to account for)
                                 // RCTL's buffer-size bits (17:16) are left at their reset value 00, which
                                 // (with BSEX also 0, also left at reset) selects the 2048-byte default -
                                 // comfortably larger than anything this test expects to receive.

const RXD_STAT_DD: u8 = 0x01; // Descriptor Done
const RXD_STAT_EOP: u8 = 0x02; // End Of Packet

const NUM_RX_DESCRIPTORS: usize = 8; // matches NUM_TX_DESCRIPTORS - same
                                     // likely ring-length alignment rule,
                                     // even though only descriptor 0 is
                                     // ever given a real buffer below

/// Legacy transmit descriptor - 16 bytes, exact field order matters (this
/// is read by the NIC's own DMA engine, not just other Rust code).
/// `packed` since the hardware format has no padding this struct should
/// introduce either.
#[repr(C, packed)]
#[derive(Clone, Copy)]
struct TxDescriptor {
    addr: u64,
    length: u16,
    cso: u8,
    cmd: u8,
    status: u8,
    css: u8,
    special: u16,
}

/// Legacy receive descriptor - 16 bytes, same packing reasoning as
/// `TxDescriptor` (this is DMA'd into by the NIC itself, not just read by
/// Rust code). Field layout verified (Fase 44) against QEMU's actual
/// `struct e1000_rx_desc` in `hw/net/e1000x_regs.h`.
#[repr(C, packed)]
#[derive(Clone, Copy)]
struct RxDescriptor {
    addr: u64,
    length: u16,
    checksum: u16,
    status: u8,
    errors: u8,
    special: u16,
}

/// Finds the real e1000 NIC on bus 0, if present.
pub fn find_e1000() -> Option<PciDevice> {
    pci::scan_bus0()
        .into_iter()
        .find(|d| d.vendor_id == E1000_VENDOR_ID && d.device_id == E1000_DEVICE_ID)
}

/// Reads a 32-bit register from the e1000's memory-mapped register
/// window. `read_volatile` (not a plain dereference) matters here exactly
/// like it does for the VGA buffer's `Volatile<T>` wrapper - MMIO reads
/// can have side effects and must never be reordered, cached, or elided
/// by the compiler the way a normal memory read could be.
///
/// # Safety
/// `mmio_base` must be a valid, mapped virtual address for the start of a
/// real e1000 register window, and `offset` must be within that window.
unsafe fn read_reg(mmio_base: u64, offset: u64) -> u32 {
    core::ptr::read_volatile((mmio_base + offset) as *const u32)
}

/// Writes a 32-bit register - same volatility reasoning as `read_reg`,
/// mirrored for writes: the compiler must never treat this as dead-store
/// eliminable or reorderable relative to other register accesses.
///
/// # Safety
/// Same as `read_reg`.
unsafe fn write_reg(mmio_base: u64, offset: u64, value: u32) {
    core::ptr::write_volatile((mmio_base + offset) as *mut u32, value);
}

/// Finds the e1000 and computes its MMIO register window's virtual base
/// address (BAR0, masked to its physical address, plus `PHYS_MEM_OFFSET`).
/// Shared by `probe` and `send_test_frame` so the BAR0-interpretation
/// logic exists in exactly one place.
fn find_mmio_base() -> Result<(PciDevice, u64), &'static str> {
    let dev = find_e1000().ok_or("no e1000 NIC found on bus 0")?;
    let bar0 = dev.read_bar0();
    if bar0 & 0x1 == 1 {
        return Err("e1000 BAR0 is I/O-space - not supported yet");
    }
    // Bits 2:1 of a memory BAR encode its type (0 = 32-bit, 2 = 64-bit);
    // either way the base address itself lives in bits 31:4 - the low 4
    // bits are these flag bits, not part of the address.
    let phys_base = (bar0 & 0xFFFF_FFF0) as u64;
    let mmio_base = PHYS_MEM_OFFSET.load(Ordering::Relaxed) + phys_base;
    Ok((dev, mmio_base))
}

/// Reads the device's own MAC address back through RAL0/RAH0.
fn read_mac(mmio_base: u64) -> [u8; 6] {
    unsafe {
        let ral0 = read_reg(mmio_base, REG_RAL0);
        let rah0 = read_reg(mmio_base, REG_RAH0);
        [
            (ral0 & 0xFF) as u8,
            ((ral0 >> 8) & 0xFF) as u8,
            ((ral0 >> 16) & 0xFF) as u8,
            ((ral0 >> 24) & 0xFF) as u8,
            (rah0 & 0xFF) as u8,
            ((rah0 >> 8) & 0xFF) as u8,
        ]
    }
}

/// Finds the e1000, reads its BAR0 to locate its register window, and
/// reads a couple of real registers back through it - proof this kernel
/// can genuinely talk to real device MMIO, not just enumerate config
/// space.
pub fn probe() {
    let (dev, mmio_base) = match find_mmio_base() {
        Ok(v) => v,
        Err(e) => {
            kprintln!("[E1000] {}", e);
            serial_println!("[E1000] {}", e);
            return;
        }
    };

    let bar0 = dev.read_bar0();
    let phys_base = (bar0 & 0xFFFF_FFF0) as u64;
    kprintln!(
        "[E1000] found at {:02x}:{:02x}.{} BAR0={:#010x} phys={:#x}",
        dev.bus,
        dev.device,
        dev.function,
        bar0,
        phys_base
    );
    serial_println!(
        "[E1000] found {:02x}:{:02x}.{} bar0={:#010x} phys={:#x}",
        dev.bus,
        dev.device,
        dev.function,
        bar0,
        phys_base
    );

    let status = unsafe { read_reg(mmio_base, REG_STATUS) };
    let mac = read_mac(mmio_base);
    let rah0 = unsafe { read_reg(mmio_base, REG_RAH0) };
    let mac_valid = rah0 & (1 << 31) != 0;

    kprintln!(
        "[E1000] STATUS={:#010x} MAC={:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x} (valid={})",
        status,
        mac[0],
        mac[1],
        mac[2],
        mac[3],
        mac[4],
        mac[5],
        mac_valid
    );
    serial_println!(
        "[E1000] status={:#010x} mac={:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x} valid={}",
        status,
        mac[0],
        mac[1],
        mac[2],
        mac[3],
        mac[4],
        mac[5],
        mac_valid
    );
}

/// Builds a real ARP request and hands it to the hardware through a real
/// TX descriptor ring - proof this kernel can *drive* the hardware
/// (program its registers, format a descriptor it accepts, trigger a
/// dequeue), not just read from it. Sets up a dedicated descriptor ring
/// and packet buffer using fresh physical frames from the global
/// allocator (`memory::frame_allocator` - the prerequisite Fase 21 built
/// for exactly this), writes one descriptor, programs
/// TDBAL/TDBAH/TDLEN/TIPG/TCTL, advances TDT to hand the descriptor to the
/// hardware, and polls the descriptor's own DD (Descriptor Done) status
/// bit.
///
/// **Fully confirmed working as of Fase 44** - see the module doc's "TX
/// DD-bit mystery, resolved" section for the two-round investigation and
/// its actual resolution (`enable_bus_mastering`, called below). `TDH`
/// (an internal register update) always advanced correctly; `DD` (a real
/// DMA write to guest memory) had nowhere to land without PCI bus
/// mastering enabled - the one real device-bring-up step this driver had
/// never performed.
pub fn send_test_frame() -> Result<(), &'static str> {
    let (dev, mmio_base) = find_mmio_base()?;
    // Fase 44: the one real PCI bring-up step this driver had never done.
    // See `PciDevice::enable_bus_mastering`'s own doc for why tracing
    // QEMU's source all the way down to `bus_master_enable_region` made
    // this a genuinely plausible explanation for the DD-bit mystery
    // below, not just routine completeness.
    dev.enable_bus_mastering();
    let src_mac = read_mac(mmio_base);

    let ring_frame = crate::memory::frame_allocator::allocate_frame();
    let buf_frame = crate::memory::frame_allocator::allocate_frame();
    let phys_offset = PHYS_MEM_OFFSET.load(Ordering::Relaxed);
    let ring_virt = (phys_offset + ring_frame.start_address().as_u64()) as *mut TxDescriptor;
    let buf_virt = (phys_offset + buf_frame.start_address().as_u64()) as *mut u8;

    // A real, valid ARP request ("who has 10.0.2.2? tell 10.0.2.15" -
    // SLIRP's default gateway/guest addresses) - tried in place of an
    // experimental-EtherType frame specifically to rule out the backend
    // silently dropping unrecognized traffic as the cause of the
    // DD-never-set issue (it wasn't - identical result either way, see
    // module doc).
    let frame_len = 42usize; // 14-byte Ethernet header + 28-byte ARP payload
    unsafe {
        let dst = core::slice::from_raw_parts_mut(buf_virt, frame_len);
        dst[0..6].copy_from_slice(&[0xFF; 6]); // broadcast dest
        dst[6..12].copy_from_slice(&src_mac);
        dst[12] = 0x08;
        dst[13] = 0x06; // EtherType: ARP
        dst[14] = 0x00;
        dst[15] = 0x01; // HTYPE: Ethernet
        dst[16] = 0x08;
        dst[17] = 0x00; // PTYPE: IPv4
        dst[18] = 6; // HLEN
        dst[19] = 4; // PLEN
        dst[20] = 0x00;
        dst[21] = 0x01; // OPER: request
        dst[22..28].copy_from_slice(&src_mac); // SHA
        dst[28..32].copy_from_slice(&[10, 0, 2, 15]); // SPA (SLIRP guest default)
        dst[32..38].copy_from_slice(&[0x00; 6]); // THA: unknown
        dst[38..42].copy_from_slice(&[10, 0, 2, 2]); // TPA (SLIRP gateway default)
    }

    unsafe {
        // Zero the whole ring first - only descriptor 0 is used, but
        // stale bytes elsewhere in a freshly-allocated (not necessarily
        // zeroed) frame could otherwise look like additional pending
        // descriptors once TDT advances past them.
        let empty = TxDescriptor {
            addr: 0,
            length: 0,
            cso: 0,
            cmd: 0,
            status: 0,
            css: 0,
            special: 0,
        };
        for i in 0..NUM_TX_DESCRIPTORS {
            core::ptr::write_volatile(ring_virt.add(i), empty);
        }
        core::ptr::write_volatile(
            ring_virt,
            TxDescriptor {
                addr: buf_frame.start_address().as_u64(),
                length: frame_len as u16,
                cso: 0,
                cmd: TXD_CMD_EOP | TXD_CMD_IFCS | TXD_CMD_RS,
                status: 0,
                css: 0,
                special: 0,
            },
        );

        // CTRL.SLU (Set Link Up) - standard bring-up step; already set by
        // default in local testing, kept for completeness/portability.
        let ctrl = read_reg(mmio_base, REG_CTRL);
        write_reg(mmio_base, REG_CTRL, ctrl | CTRL_SLU);

        let ring_phys = ring_frame.start_address().as_u64();
        write_reg(mmio_base, REG_TDBAL, (ring_phys & 0xFFFF_FFFF) as u32);
        write_reg(mmio_base, REG_TDBAH, (ring_phys >> 32) as u32);
        write_reg(mmio_base, REG_TDLEN, (NUM_TX_DESCRIPTORS * 16) as u32);
        write_reg(mmio_base, REG_TDH, 0);
        write_reg(mmio_base, REG_TDT, 0);
        write_reg(mmio_base, REG_TIPG, TIPG_DEFAULT);
        write_reg(
            mmio_base,
            REG_TCTL,
            TCTL_EN | TCTL_PSP | TCTL_CT_DEFAULT | TCTL_COLD_FULL_DUPLEX,
        );

        // The one line that actually tells the hardware "descriptor 0 is
        // ready, go send it" - everything above was just setup.
        write_reg(mmio_base, REG_TDT, 1);

        // Waits via `hlt` (interrupts enabled, timer ticking) rather than a
        // tight busy-spin - genuinely yields the CPU back between checks,
        // in case QEMU's own completion of this send is processed via a
        // deferred/bottom-half mechanism that only gets a chance to run
        // when the guest actually traps out, not during an uninterrupted
        // spin. ~90 timer ticks at this kernel's ~18.2Hz rate is close to
        // 5 real seconds - comparable to the longer real-time window
        // already tried once before (see module doc).
        let start_tick = crate::interrupts::timer_ticks();
        loop {
            let status = core::ptr::read_volatile(core::ptr::addr_of!((*ring_virt).status));
            if status & TXD_STATUS_DD != 0 {
                kprintln!(
                    "[E1000] sent {} byte ARP request - DD set (hardware confirmed)",
                    frame_len
                );
                serial_println!("[E1000] tx_sent {} bytes dd=true", frame_len);
                return Ok(());
            }
            if crate::interrupts::timer_ticks() - start_tick > 90 {
                break;
            }
            x86_64::instructions::hlt();
        }

        // Diagnostic left in place for whoever resumes this: shows
        // exactly what state things were left in on timeout, without
        // needing to re-add debug prints from scratch.
        let tdh = read_reg(mmio_base, REG_TDH);
        let tdt = read_reg(mmio_base, REG_TDT);
        let raw = core::slice::from_raw_parts(ring_virt as *const u8, 16);
        serial_println!(
            "[E1000] tx timeout: ring_phys={:#x} TDH={:#x} TDT={:#x} raw_desc0={:02x?}",
            ring_frame.start_address().as_u64(),
            tdh,
            tdt,
            raw
        );
    }
    Err("e1000 TX: timed out waiting for descriptor DD - hardware dequeued it (TDH advanced) but never confirmed the send; see module doc's \"TX DD-bit mystery, resolved\" section (this path should be unreachable since Fase 44's bus-mastering fix - a real regression if it fires)")
}

/// Sets up a real receive descriptor ring, triggers a real ARP request
/// via `send_test_frame` (SLIRP's default gateway at `10.0.2.2` should
/// reply to it), and polls for a genuine received frame - the first real
/// RX attempt this driver has made. Register offsets, `RCTL` bits, and
/// the legacy RX descriptor layout were verified (Fase 44) against
/// QEMU's actual `hw/net/e1000x_regs.h`/`e1000x_common.c` source, the
/// same discipline the TX side's own investigation used.
///
/// The RX ring is armed *before* the TX send, not after - a reply could
/// arrive at any point once the request goes out, and `send_test_frame`
/// itself already waits several real seconds internally for its own DD
/// confirmation, during which an un-armed RX ring would simply drop
/// anything QEMU's SLIRP backend sent back.
///
/// Only descriptor 0 is ever given a real buffer; the rest are left
/// zeroed (`addr = 0`), which QEMU's own model explicitly tolerates ("as
/// per intel docs; skip descriptors with null buf addr" - confirmed by
/// reading `e1000_receive_iov`'s source directly) - fine for a 42-byte
/// ARP reply that fits entirely in one 2048-byte buffer, never spilling
/// into a second descriptor.
pub fn receive_test_frame() -> Result<(), &'static str> {
    let (dev, mmio_base) = find_mmio_base()?;
    dev.enable_bus_mastering();

    let ring_frame = crate::memory::frame_allocator::allocate_frame();
    let buf_frame = crate::memory::frame_allocator::allocate_frame();
    let phys_offset = PHYS_MEM_OFFSET.load(Ordering::Relaxed);
    let ring_virt = (phys_offset + ring_frame.start_address().as_u64()) as *mut RxDescriptor;

    unsafe {
        let empty = RxDescriptor {
            addr: 0,
            length: 0,
            checksum: 0,
            status: 0,
            errors: 0,
            special: 0,
        };
        for i in 0..NUM_RX_DESCRIPTORS {
            core::ptr::write_volatile(ring_virt.add(i), empty);
        }
        // Only descriptor 0 gets a real buffer - the only one this test
        // ever expects hardware to fill in.
        core::ptr::write_volatile(
            ring_virt,
            RxDescriptor {
                addr: buf_frame.start_address().as_u64(),
                length: 0,
                checksum: 0,
                status: 0,
                errors: 0,
                special: 0,
            },
        );

        let ring_phys = ring_frame.start_address().as_u64();
        write_reg(mmio_base, REG_RDBAL, (ring_phys & 0xFFFF_FFFF) as u32);
        write_reg(mmio_base, REG_RDBAH, (ring_phys >> 32) as u32);
        write_reg(mmio_base, REG_RDLEN, (NUM_RX_DESCRIPTORS * 16) as u32);
        write_reg(mmio_base, REG_RDH, 0);
        // Exactly descriptor 0 available to hardware - confirmed via
        // QEMU's own `e1000_has_rxbufs` source that the ring uses a
        // [RDH, RDT) convention where RDH == RDT reads as "nothing
        // available", not "everything" - so RDT must sit one slot past
        // the single descriptor this test actually wants filled.
        write_reg(mmio_base, REG_RDT, 1);
        write_reg(mmio_base, REG_RCTL, RCTL_EN | RCTL_BAM | RCTL_SECRC);
    }

    // Trigger the real ARP request this ring is now waiting to see a
    // reply to. Its own internal DD-confirmation wait already yields the
    // CPU via hlt() for up to ~5 real seconds - plenty of time for
    // SLIRP's virtual gateway to answer, if it's going to.
    let tx_result = send_test_frame();
    serial_println!(
        "[E1000] rx_test tx_phase={}",
        if tx_result.is_ok() { "ok" } else { "failed" }
    );

    unsafe {
        let start_tick = crate::interrupts::timer_ticks();
        loop {
            let status = core::ptr::read_volatile(core::ptr::addr_of!((*ring_virt).status));
            if status & RXD_STAT_DD != 0 {
                let length = core::ptr::read_volatile(core::ptr::addr_of!((*ring_virt).length));
                let eop = status & RXD_STAT_EOP != 0;
                let buf_virt = (phys_offset + buf_frame.start_address().as_u64()) as *const u8;
                let data = core::slice::from_raw_parts(buf_virt, length as usize);

                // Beyond the Ethernet header (dest MAC ours, EtherType
                // ARP - already true of any frame this filter would ever
                // pass through to us at all), check the ARP payload
                // itself is a genuine *reply*, not just a same-shaped
                // coincidence: OPER at bytes 20-21 should be 0x0002
                // (reply), and SHA at bytes 22-27 (the replying device's
                // own MAC) should match the Ethernet header's source MAC
                // - real, internal cross-consistency a malformed or
                // unrelated frame wouldn't have.
                let is_arp_reply = data.len() >= 28
                    && data[12] == 0x08
                    && data[13] == 0x06
                    && data[20] == 0x00
                    && data[21] == 0x02
                    && data[22..28] == data[6..12];
                let gateway_ip = if data.len() >= 32 {
                    Some([data[28], data[29], data[30], data[31]])
                } else {
                    None
                };

                kprintln!(
                    "[E1000] received {} bytes (EOP={}) - a real frame from the network, is_arp_reply={}",
                    length, eop, is_arp_reply
                );
                serial_println!(
                    "[E1000] rx_received {} bytes eop={} is_arp_reply={} gateway_ip={:?} src_mac={:02x?}",
                    length, eop, is_arp_reply, gateway_ip, &data[6..12]
                );
                return Ok(());
            }
            if crate::interrupts::timer_ticks() - start_tick > 90 {
                break;
            }
            x86_64::instructions::hlt();
        }

        serial_println!(
            "[E1000] rx timeout: ring_phys={:#x} RDH={:#x} RDT={:#x}",
            ring_frame.start_address().as_u64(),
            read_reg(mmio_base, REG_RDH),
            read_reg(mmio_base, REG_RDT)
        );
    }
    Err("e1000 RX: timed out waiting for a received frame - either SLIRP didn't reply, or something in this ring's own setup is wrong; see rx timeout diagnostic above")
}

/// Resolves `target_ip`'s real MAC address via a genuine ARP request/reply
/// round trip - the same TX+RX mechanics `send_test_frame`/
/// `receive_test_frame` proved (Fase 44/45), generalized to any target
/// instead of hardcoded to SLIRP's gateway (10.0.2.2). Deliberately a new,
/// independent function rather than a refactor of those two: their exact
/// behavior and log output are CI-asserted across three Fases (44-46), so
/// this duplicates their proven ring-setup shape (same constants, same
/// descriptor structs, same bounded `hlt`-based poll pattern) rather than
/// risk changing anything about them. The natural prerequisite for a real
/// `ping` (net::icmp, Fase 47) - sending an IP packet needs the
/// destination's MAC first, unless hardcoded.
///
/// Re-arms the RX descriptor on a non-matching frame instead of failing
/// on the first thing received - other traffic isn't expected on SLIRP's
/// simple guest/gateway topology, but nothing guarantees the very first
/// frame this ring sees is necessarily the reply being waited for.
pub fn arp_resolve(target_ip: [u8; 4]) -> Result<[u8; 6], &'static str> {
    let (dev, mmio_base) = find_mmio_base()?;
    dev.enable_bus_mastering();
    let src_mac = read_mac(mmio_base);

    let rx_ring_frame = crate::memory::frame_allocator::allocate_frame();
    let rx_buf_frame = crate::memory::frame_allocator::allocate_frame();
    let tx_ring_frame = crate::memory::frame_allocator::allocate_frame();
    let tx_buf_frame = crate::memory::frame_allocator::allocate_frame();
    let phys_offset = PHYS_MEM_OFFSET.load(Ordering::Relaxed);
    let rx_ring_virt = (phys_offset + rx_ring_frame.start_address().as_u64()) as *mut RxDescriptor;
    let tx_ring_virt = (phys_offset + tx_ring_frame.start_address().as_u64()) as *mut TxDescriptor;
    let tx_buf_virt = (phys_offset + tx_buf_frame.start_address().as_u64()) as *mut u8;

    // Arm the RX ring FIRST, same ordering reasoning as receive_test_frame:
    // a reply could arrive at any point once the request goes out.
    unsafe {
        let empty_rx = RxDescriptor {
            addr: 0,
            length: 0,
            checksum: 0,
            status: 0,
            errors: 0,
            special: 0,
        };
        for i in 0..NUM_RX_DESCRIPTORS {
            core::ptr::write_volatile(rx_ring_virt.add(i), empty_rx);
        }
        core::ptr::write_volatile(
            rx_ring_virt,
            RxDescriptor {
                addr: rx_buf_frame.start_address().as_u64(),
                length: 0,
                checksum: 0,
                status: 0,
                errors: 0,
                special: 0,
            },
        );
        let rx_ring_phys = rx_ring_frame.start_address().as_u64();
        write_reg(mmio_base, REG_RDBAL, (rx_ring_phys & 0xFFFF_FFFF) as u32);
        write_reg(mmio_base, REG_RDBAH, (rx_ring_phys >> 32) as u32);
        write_reg(mmio_base, REG_RDLEN, (NUM_RX_DESCRIPTORS * 16) as u32);
        write_reg(mmio_base, REG_RDH, 0);
        write_reg(mmio_base, REG_RDT, 1);
        write_reg(mmio_base, REG_RCTL, RCTL_EN | RCTL_BAM | RCTL_SECRC);
    }

    // Build a real ARP request for target_ip - parameterized, unlike
    // send_test_frame's hardcoded-to-10.0.2.2 version.
    let frame_len = 42usize;
    unsafe {
        let dst = core::slice::from_raw_parts_mut(tx_buf_virt, frame_len);
        dst[0..6].copy_from_slice(&[0xFF; 6]); // broadcast dest
        dst[6..12].copy_from_slice(&src_mac);
        dst[12] = 0x08;
        dst[13] = 0x06; // EtherType: ARP
        dst[14] = 0x00;
        dst[15] = 0x01; // HTYPE: Ethernet
        dst[16] = 0x08;
        dst[17] = 0x00; // PTYPE: IPv4
        dst[18] = 6; // HLEN
        dst[19] = 4; // PLEN
        dst[20] = 0x00;
        dst[21] = 0x01; // OPER: request
        dst[22..28].copy_from_slice(&src_mac); // SHA
        dst[28..32].copy_from_slice(&[10, 0, 2, 15]); // SPA (SLIRP guest default)
        dst[32..38].copy_from_slice(&[0x00; 6]); // THA: unknown
        dst[38..42].copy_from_slice(&target_ip); // TPA: the address being resolved
    }

    unsafe {
        let empty_tx = TxDescriptor {
            addr: 0,
            length: 0,
            cso: 0,
            cmd: 0,
            status: 0,
            css: 0,
            special: 0,
        };
        for i in 0..NUM_TX_DESCRIPTORS {
            core::ptr::write_volatile(tx_ring_virt.add(i), empty_tx);
        }
        core::ptr::write_volatile(
            tx_ring_virt,
            TxDescriptor {
                addr: tx_buf_frame.start_address().as_u64(),
                length: frame_len as u16,
                cso: 0,
                cmd: TXD_CMD_EOP | TXD_CMD_IFCS | TXD_CMD_RS,
                status: 0,
                css: 0,
                special: 0,
            },
        );

        let ctrl = read_reg(mmio_base, REG_CTRL);
        write_reg(mmio_base, REG_CTRL, ctrl | CTRL_SLU);

        let tx_ring_phys = tx_ring_frame.start_address().as_u64();
        write_reg(mmio_base, REG_TDBAL, (tx_ring_phys & 0xFFFF_FFFF) as u32);
        write_reg(mmio_base, REG_TDBAH, (tx_ring_phys >> 32) as u32);
        write_reg(mmio_base, REG_TDLEN, (NUM_TX_DESCRIPTORS * 16) as u32);
        write_reg(mmio_base, REG_TDH, 0);
        write_reg(mmio_base, REG_TDT, 0);
        write_reg(mmio_base, REG_TIPG, TIPG_DEFAULT);
        write_reg(
            mmio_base,
            REG_TCTL,
            TCTL_EN | TCTL_PSP | TCTL_CT_DEFAULT | TCTL_COLD_FULL_DUPLEX,
        );
        write_reg(mmio_base, REG_TDT, 1);
    }

    let tx_start_tick = crate::interrupts::timer_ticks();
    let mut tx_ok = false;
    unsafe {
        loop {
            let status = core::ptr::read_volatile(core::ptr::addr_of!((*tx_ring_virt).status));
            if status & TXD_STATUS_DD != 0 {
                tx_ok = true;
                break;
            }
            if crate::interrupts::timer_ticks() - tx_start_tick > 90 {
                break;
            }
            x86_64::instructions::hlt();
        }
    }
    if !tx_ok {
        return Err("arp_resolve: TX timed out waiting for descriptor DD");
    }

    let rx_start_tick = crate::interrupts::timer_ticks();
    unsafe {
        loop {
            let status = core::ptr::read_volatile(core::ptr::addr_of!((*rx_ring_virt).status));
            if status & RXD_STAT_DD != 0 {
                let length =
                    core::ptr::read_volatile(core::ptr::addr_of!((*rx_ring_virt).length)) as usize;
                let buf_virt = (phys_offset + rx_buf_frame.start_address().as_u64()) as *const u8;
                let data = core::slice::from_raw_parts(buf_virt, length);

                // Same cross-consistency checks receive_test_frame already
                // uses (OPER=reply, SHA matches the Ethernet source MAC),
                // plus the new one this function actually needs: the
                // reply's SPA must equal the target_ip being resolved -
                // proof this is really an answer to OUR question, not
                // some other exchange happening to also be a valid ARP
                // reply.
                let is_matching_reply = data.len() >= 32
                    && data[12] == 0x08
                    && data[13] == 0x06
                    && data[20] == 0x00
                    && data[21] == 0x02
                    && data[22..28] == data[6..12]
                    && data[28..32] == target_ip;
                if is_matching_reply {
                    let mut mac = [0u8; 6];
                    mac.copy_from_slice(&data[22..28]);
                    serial_println!(
                        "[E1000] arp_resolve target={:?} mac={:02x?}",
                        target_ip,
                        mac
                    );
                    return Ok(mac);
                }

                // Not the reply being waited for - rearm this same
                // descriptor and keep polling instead of giving up.
                core::ptr::write_volatile(
                    rx_ring_virt,
                    RxDescriptor {
                        addr: rx_buf_frame.start_address().as_u64(),
                        length: 0,
                        checksum: 0,
                        status: 0,
                        errors: 0,
                        special: 0,
                    },
                );
                write_reg(mmio_base, REG_RDT, 1);
            }
            if crate::interrupts::timer_ticks() - rx_start_tick > 90 {
                break;
            }
            x86_64::instructions::hlt();
        }
    }
    serial_println!(
        "[E1000] arp_resolve target={:?} timeout RDH={:#x} RDT={:#x}",
        target_ip,
        unsafe { read_reg(mmio_base, REG_RDH) },
        unsafe { read_reg(mmio_base, REG_RDT) }
    );
    Err("arp_resolve: timed out waiting for a matching ARP reply")
}

/// Sends a single real ICMP echo request to `target_ip` and waits for a
/// genuine echo reply - the natural convergence of `arp_resolve` (Fase 48,
/// resolves the destination MAC) and `net::icmp` (Fase 47, builds/parses
/// the actual IPv4+ICMP bytes, pure logic with no hardware dependency of
/// its own). This is the first time this kernel has assembled and sent a
/// complete, real, routable IP packet - not just a raw Ethernet-level test
/// frame like `send_test_frame`'s ARP request. A new, independent
/// function, same reasoning as `arp_resolve` itself: `send_test_frame`/
/// `receive_test_frame`'s exact behavior and log output stay untouched.
///
/// Honestly uncertain going in, the same way Fase 45's first RX attempt
/// was: SLIRP is confirmed to answer ARP for its own gateway (Fase 45/48),
/// but this is the first time this kernel has ever asked it to answer a
/// *ping* - expected to work (replying to an ICMP echo addressed to its
/// own IP is about as basic as an IP stack gets), but not proven before
/// this function's own test exercises it for real.
pub fn ping(target_ip: [u8; 4]) -> Result<(), &'static str> {
    use crate::net::icmp;

    let dest_mac = arp_resolve(target_ip)?;
    let (dev, mmio_base) = find_mmio_base()?;
    dev.enable_bus_mastering();
    let src_mac = read_mac(mmio_base);
    const SRC_IP: [u8; 4] = [10, 0, 2, 15]; // SLIRP guest default
    const IDENTIFIER: u16 = 0xABCD;
    const SEQUENCE: u16 = 1;

    let icmp_message = icmp::build_icmp_echo(false, IDENTIFIER, SEQUENCE, b"AgentOS real ping");
    let ipv4_header = icmp::build_ipv4_header(
        1,
        64,
        icmp::IP_PROTOCOL_ICMP,
        SRC_IP,
        target_ip,
        icmp_message.len(),
    );
    let eth_header = icmp::build_ethernet_header(dest_mac, src_mac, icmp::ETHERTYPE_IPV4);

    let mut frame: alloc::vec::Vec<u8> = alloc::vec::Vec::with_capacity(
        icmp::ETHERNET_HEADER_LEN + icmp::IPV4_HEADER_LEN + icmp_message.len(),
    );
    frame.extend_from_slice(&eth_header);
    frame.extend_from_slice(&ipv4_header);
    frame.extend_from_slice(&icmp_message);

    let rx_ring_frame = crate::memory::frame_allocator::allocate_frame();
    let rx_buf_frame = crate::memory::frame_allocator::allocate_frame();
    let tx_ring_frame = crate::memory::frame_allocator::allocate_frame();
    let tx_buf_frame = crate::memory::frame_allocator::allocate_frame();
    let phys_offset = PHYS_MEM_OFFSET.load(Ordering::Relaxed);
    let rx_ring_virt = (phys_offset + rx_ring_frame.start_address().as_u64()) as *mut RxDescriptor;
    let tx_ring_virt = (phys_offset + tx_ring_frame.start_address().as_u64()) as *mut TxDescriptor;
    let tx_buf_virt = (phys_offset + tx_buf_frame.start_address().as_u64()) as *mut u8;

    // Arm RX first, same ordering reasoning as arp_resolve/receive_test_frame.
    unsafe {
        let empty_rx = RxDescriptor {
            addr: 0,
            length: 0,
            checksum: 0,
            status: 0,
            errors: 0,
            special: 0,
        };
        for i in 0..NUM_RX_DESCRIPTORS {
            core::ptr::write_volatile(rx_ring_virt.add(i), empty_rx);
        }
        core::ptr::write_volatile(
            rx_ring_virt,
            RxDescriptor {
                addr: rx_buf_frame.start_address().as_u64(),
                length: 0,
                checksum: 0,
                status: 0,
                errors: 0,
                special: 0,
            },
        );
        let rx_ring_phys = rx_ring_frame.start_address().as_u64();
        write_reg(mmio_base, REG_RDBAL, (rx_ring_phys & 0xFFFF_FFFF) as u32);
        write_reg(mmio_base, REG_RDBAH, (rx_ring_phys >> 32) as u32);
        write_reg(mmio_base, REG_RDLEN, (NUM_RX_DESCRIPTORS * 16) as u32);
        write_reg(mmio_base, REG_RDH, 0);
        write_reg(mmio_base, REG_RDT, 1);
        write_reg(mmio_base, REG_RCTL, RCTL_EN | RCTL_BAM | RCTL_SECRC);
    }

    unsafe {
        core::ptr::copy_nonoverlapping(frame.as_ptr(), tx_buf_virt, frame.len());

        let empty_tx = TxDescriptor {
            addr: 0,
            length: 0,
            cso: 0,
            cmd: 0,
            status: 0,
            css: 0,
            special: 0,
        };
        for i in 0..NUM_TX_DESCRIPTORS {
            core::ptr::write_volatile(tx_ring_virt.add(i), empty_tx);
        }
        core::ptr::write_volatile(
            tx_ring_virt,
            TxDescriptor {
                addr: tx_buf_frame.start_address().as_u64(),
                length: frame.len() as u16,
                cso: 0,
                cmd: TXD_CMD_EOP | TXD_CMD_IFCS | TXD_CMD_RS,
                status: 0,
                css: 0,
                special: 0,
            },
        );

        let ctrl = read_reg(mmio_base, REG_CTRL);
        write_reg(mmio_base, REG_CTRL, ctrl | CTRL_SLU);

        let tx_ring_phys = tx_ring_frame.start_address().as_u64();
        write_reg(mmio_base, REG_TDBAL, (tx_ring_phys & 0xFFFF_FFFF) as u32);
        write_reg(mmio_base, REG_TDBAH, (tx_ring_phys >> 32) as u32);
        write_reg(mmio_base, REG_TDLEN, (NUM_TX_DESCRIPTORS * 16) as u32);
        write_reg(mmio_base, REG_TDH, 0);
        write_reg(mmio_base, REG_TDT, 0);
        write_reg(mmio_base, REG_TIPG, TIPG_DEFAULT);
        write_reg(
            mmio_base,
            REG_TCTL,
            TCTL_EN | TCTL_PSP | TCTL_CT_DEFAULT | TCTL_COLD_FULL_DUPLEX,
        );
        write_reg(mmio_base, REG_TDT, 1);
    }

    let tx_start_tick = crate::interrupts::timer_ticks();
    let mut tx_ok = false;
    unsafe {
        loop {
            let status = core::ptr::read_volatile(core::ptr::addr_of!((*tx_ring_virt).status));
            if status & TXD_STATUS_DD != 0 {
                tx_ok = true;
                break;
            }
            if crate::interrupts::timer_ticks() - tx_start_tick > 90 {
                break;
            }
            x86_64::instructions::hlt();
        }
    }
    if !tx_ok {
        return Err("ping: TX timed out waiting for descriptor DD");
    }

    let rx_start_tick = crate::interrupts::timer_ticks();
    unsafe {
        loop {
            let status = core::ptr::read_volatile(core::ptr::addr_of!((*rx_ring_virt).status));
            if status & RXD_STAT_DD != 0 {
                let length =
                    core::ptr::read_volatile(core::ptr::addr_of!((*rx_ring_virt).length)) as usize;
                let buf_virt = (phys_offset + rx_buf_frame.start_address().as_u64()) as *const u8;
                let data = core::slice::from_raw_parts(buf_virt, length);

                // Ethernet: EtherType IPv4. IPv4: protocol ICMP, header
                // checksum valid, source address is the host being
                // pinged (its reply, addressed back to us). Each check
                // is independent real evidence, not just "something
                // arrived" - a malformed or unrelated frame would very
                // likely fail at least one.
                let ip_start = icmp::ETHERNET_HEADER_LEN;
                let icmp_start = ip_start + icmp::IPV4_HEADER_LEN;
                let is_our_reply = data.len() > icmp_start
                    && data[12] == 0x08
                    && data[13] == 0x00
                    && data[ip_start + 9] == icmp::IP_PROTOCOL_ICMP
                    && icmp::checksum_is_valid(&data[ip_start..icmp_start])
                    && data[ip_start + 12..ip_start + 16] == target_ip;

                if is_our_reply {
                    if let Ok(info) = icmp::parse_icmp_echo(&data[icmp_start..]) {
                        if info.is_reply
                            && info.identifier == IDENTIFIER
                            && info.sequence == SEQUENCE
                        {
                            let rtt_ticks = crate::interrupts::timer_ticks() - rx_start_tick;
                            kprintln!(
                                "[E1000] ping to {:?}: real ICMP echo reply received (rtt~{} ticks)",
                                target_ip,
                                rtt_ticks
                            );
                            serial_println!(
                                "[E1000] ping target={:?} reply=true rtt_ticks={}",
                                target_ip,
                                rtt_ticks
                            );
                            return Ok(());
                        }
                    }
                }

                // Not our reply - rearm this same descriptor and keep
                // polling, same reasoning as arp_resolve.
                core::ptr::write_volatile(
                    rx_ring_virt,
                    RxDescriptor {
                        addr: rx_buf_frame.start_address().as_u64(),
                        length: 0,
                        checksum: 0,
                        status: 0,
                        errors: 0,
                        special: 0,
                    },
                );
                write_reg(mmio_base, REG_RDT, 1);
            }
            if crate::interrupts::timer_ticks() - rx_start_tick > 90 {
                break;
            }
            x86_64::instructions::hlt();
        }
    }
    Err("ping: timed out waiting for a genuine ICMP echo reply")
}

/// Fase 89: the first real TCP round trip this kernel has ever
/// attempted, sending a genuine SYN segment (`net::tcp::build_tcp_
/// header`) to `target_ip:target_port` and polling for a genuine
/// SYN-ACK reply, validating it fully: checksum, flags, ports, and that
/// the acknowledgment number is really our own sequence number plus one,
/// not just "some TCP-shaped bytes arrived". Mirrors `ping`'s own
/// structure line for line (own RX/TX ring setup, `arp_resolve` for the
/// destination MAC, the same TX-poll-DD/RX-poll-and-rearm shape), the
/// same "duplicate the proven ring-setup shape rather than risk
/// touching it" reasoning `arp_resolve`/`ping` themselves already
/// established, now extended to a third protocol.
///
/// Deliberately targets a `guestfwd` address (see `boot_kernel.bat`/
/// `kernel-ci.yml`'s own `-netdev ...,guestfwd=tcp:10.0.2.100:9999-cmd:
/// cat`), not a real external server: QEMU's own SLIRP terminates the
/// TCP connection itself with its own real, standards-compliant stack,
/// piping the resulting byte stream to a spawned `cat` process on the
/// host - genuinely real TCP protocol behavior, entirely local, with
/// zero dependency on real network reachability (this kernel's own
/// established testing philosophy - see e.g. `arp_resolve`/`ping`
/// against SLIRP's own fixed gateway, never a real external address).
///
/// Does NOT yet send the final ACK to complete the 3-way handshake, or
/// track any ongoing connection state - this Fase proves the send/
/// receive/validate primitives are correct in isolation first, the same
/// order e1000 TX (Fase 22/53) preceded RX (Fase 45) by a full Fase gap.
pub fn tcp_syn_test(
    target_ip: [u8; 4],
    target_port: u16,
) -> Result<crate::net::tcp::TcpSegmentInfo, &'static str> {
    use crate::net::{icmp, tcp};

    let dest_mac = arp_resolve(target_ip)?;
    let (dev, mmio_base) = find_mmio_base()?;
    dev.enable_bus_mastering();
    let src_mac = read_mac(mmio_base);
    const SRC_IP: [u8; 4] = [10, 0, 2, 15]; // SLIRP guest default
    const SRC_PORT: u16 = 54321;
    const INITIAL_SEQ: u32 = 0x1000_0000;

    let tcp_header = tcp::build_tcp_header(
        SRC_IP,
        target_ip,
        SRC_PORT,
        target_port,
        INITIAL_SEQ,
        0,
        tcp::TCP_FLAG_SYN,
        65535,
        &[],
    );
    let ipv4_header = icmp::build_ipv4_header(
        1,
        64,
        tcp::IP_PROTOCOL_TCP,
        SRC_IP,
        target_ip,
        tcp_header.len(),
    );
    let eth_header = icmp::build_ethernet_header(dest_mac, src_mac, icmp::ETHERTYPE_IPV4);

    let mut frame: alloc::vec::Vec<u8> = alloc::vec::Vec::with_capacity(
        icmp::ETHERNET_HEADER_LEN + icmp::IPV4_HEADER_LEN + tcp_header.len(),
    );
    frame.extend_from_slice(&eth_header);
    frame.extend_from_slice(&ipv4_header);
    frame.extend_from_slice(&tcp_header);

    let rx_ring_frame = crate::memory::frame_allocator::allocate_frame();
    let rx_buf_frame = crate::memory::frame_allocator::allocate_frame();
    let tx_ring_frame = crate::memory::frame_allocator::allocate_frame();
    let tx_buf_frame = crate::memory::frame_allocator::allocate_frame();
    let phys_offset = PHYS_MEM_OFFSET.load(Ordering::Relaxed);
    let rx_ring_virt = (phys_offset + rx_ring_frame.start_address().as_u64()) as *mut RxDescriptor;
    let tx_ring_virt = (phys_offset + tx_ring_frame.start_address().as_u64()) as *mut TxDescriptor;
    let tx_buf_virt = (phys_offset + tx_buf_frame.start_address().as_u64()) as *mut u8;

    // Arm RX first, same ordering reasoning as arp_resolve/ping.
    unsafe {
        let empty_rx = RxDescriptor {
            addr: 0,
            length: 0,
            checksum: 0,
            status: 0,
            errors: 0,
            special: 0,
        };
        for i in 0..NUM_RX_DESCRIPTORS {
            core::ptr::write_volatile(rx_ring_virt.add(i), empty_rx);
        }
        core::ptr::write_volatile(
            rx_ring_virt,
            RxDescriptor {
                addr: rx_buf_frame.start_address().as_u64(),
                length: 0,
                checksum: 0,
                status: 0,
                errors: 0,
                special: 0,
            },
        );
        let rx_ring_phys = rx_ring_frame.start_address().as_u64();
        write_reg(mmio_base, REG_RDBAL, (rx_ring_phys & 0xFFFF_FFFF) as u32);
        write_reg(mmio_base, REG_RDBAH, (rx_ring_phys >> 32) as u32);
        write_reg(mmio_base, REG_RDLEN, (NUM_RX_DESCRIPTORS * 16) as u32);
        write_reg(mmio_base, REG_RDH, 0);
        write_reg(mmio_base, REG_RDT, 1);
        write_reg(mmio_base, REG_RCTL, RCTL_EN | RCTL_BAM | RCTL_SECRC);
    }

    unsafe {
        core::ptr::copy_nonoverlapping(frame.as_ptr(), tx_buf_virt, frame.len());

        let empty_tx = TxDescriptor {
            addr: 0,
            length: 0,
            cso: 0,
            cmd: 0,
            status: 0,
            css: 0,
            special: 0,
        };
        for i in 0..NUM_TX_DESCRIPTORS {
            core::ptr::write_volatile(tx_ring_virt.add(i), empty_tx);
        }
        core::ptr::write_volatile(
            tx_ring_virt,
            TxDescriptor {
                addr: tx_buf_frame.start_address().as_u64(),
                length: frame.len() as u16,
                cso: 0,
                cmd: TXD_CMD_EOP | TXD_CMD_IFCS | TXD_CMD_RS,
                status: 0,
                css: 0,
                special: 0,
            },
        );

        let ctrl = read_reg(mmio_base, REG_CTRL);
        write_reg(mmio_base, REG_CTRL, ctrl | CTRL_SLU);

        let tx_ring_phys = tx_ring_frame.start_address().as_u64();
        write_reg(mmio_base, REG_TDBAL, (tx_ring_phys & 0xFFFF_FFFF) as u32);
        write_reg(mmio_base, REG_TDBAH, (tx_ring_phys >> 32) as u32);
        write_reg(mmio_base, REG_TDLEN, (NUM_TX_DESCRIPTORS * 16) as u32);
        write_reg(mmio_base, REG_TDH, 0);
        write_reg(mmio_base, REG_TDT, 0);
        write_reg(mmio_base, REG_TIPG, TIPG_DEFAULT);
        write_reg(
            mmio_base,
            REG_TCTL,
            TCTL_EN | TCTL_PSP | TCTL_CT_DEFAULT | TCTL_COLD_FULL_DUPLEX,
        );
        write_reg(mmio_base, REG_TDT, 1);
    }

    let tx_start_tick = crate::interrupts::timer_ticks();
    let mut tx_ok = false;
    unsafe {
        loop {
            let status = core::ptr::read_volatile(core::ptr::addr_of!((*tx_ring_virt).status));
            if status & TXD_STATUS_DD != 0 {
                tx_ok = true;
                break;
            }
            if crate::interrupts::timer_ticks() - tx_start_tick > 90 {
                break;
            }
            x86_64::instructions::hlt();
        }
    }
    if !tx_ok {
        return Err("tcp_syn_test: TX timed out waiting for descriptor DD");
    }

    let rx_start_tick = crate::interrupts::timer_ticks();
    unsafe {
        loop {
            let status = core::ptr::read_volatile(core::ptr::addr_of!((*rx_ring_virt).status));
            if status & RXD_STAT_DD != 0 {
                let length =
                    core::ptr::read_volatile(core::ptr::addr_of!((*rx_ring_virt).length)) as usize;
                let buf_virt = (phys_offset + rx_buf_frame.start_address().as_u64()) as *const u8;
                let data = core::slice::from_raw_parts(buf_virt, length);

                // Ethernet: EtherType IPv4. IPv4: protocol TCP, header
                // checksum valid, source address is the guestfwd target
                // (its reply, addressed back to us) - the same
                // independent-checks shape ping's own is_our_reply
                // check already established, just for TCP instead of
                // ICMP.
                let ip_start = icmp::ETHERNET_HEADER_LEN;
                let tcp_start = ip_start + icmp::IPV4_HEADER_LEN;
                let is_our_reply = data.len() > tcp_start
                    && data[12] == 0x08
                    && data[13] == 0x00
                    && data[ip_start + 9] == tcp::IP_PROTOCOL_TCP
                    && icmp::checksum_is_valid(&data[ip_start..tcp_start])
                    && data[ip_start + 12..ip_start + 16] == target_ip;

                // Fase 89's own real-world bug, caught by this exact
                // test's first boot (confirmed via a real packet
                // capture, not guessed): the received buffer's own
                // length is NOT the same as the real TCP segment's own
                // length - a real reply is very likely padded up to
                // Ethernet's 60-byte minimum frame size, and a real TCP
                // stack's own SYN-ACK almost always carries the
                // standard MSS option too, both adding real, extra
                // bytes beyond a bare 20-byte header that must NOT be
                // fed into the checksum sum (the ORIGINAL sender never
                // included them in its own tcp_len calculation). The
                // IPv4 header's own Total Length field is the one
                // authoritative source for how many bytes past the IP
                // header genuinely belong to this packet - trimming to
                // exactly that, not "everything left in the receive
                // buffer", is what let parse_tcp_segment's own checksum
                // verification agree with the real sender's.
                let ip_total_len = if is_our_reply {
                    u16::from_be_bytes([data[ip_start + 2], data[ip_start + 3]]) as usize
                } else {
                    0
                };
                let real_tcp_len = ip_total_len.saturating_sub(icmp::IPV4_HEADER_LEN);
                let tcp_end = (tcp_start + real_tcp_len).min(data.len());

                if is_our_reply {
                    if let Ok(info) =
                        tcp::parse_tcp_segment(target_ip, SRC_IP, &data[tcp_start..tcp_end])
                    {
                        let is_syn_ack = info.flags == (tcp::TCP_FLAG_SYN | tcp::TCP_FLAG_ACK)
                            && info.source_port == target_port
                            && info.dest_port == SRC_PORT
                            && info.ack_num == INITIAL_SEQ.wrapping_add(1);
                        if is_syn_ack {
                            kprintln!(
                                "[E1000] tcp_syn_test to {:?}:{}: real SYN-ACK received (seq={:#x} ack={:#x})",
                                target_ip,
                                target_port,
                                info.seq_num,
                                info.ack_num
                            );
                            serial_println!(
                                "[E1000] tcp_syn_test target={:?} port={} syn_ack_received=true ack_matches=true",
                                target_ip,
                                target_port
                            );
                            return Ok(info);
                        }
                    }
                }

                // Not our reply - rearm this same descriptor and keep
                // polling, same reasoning as arp_resolve/ping.
                core::ptr::write_volatile(
                    rx_ring_virt,
                    RxDescriptor {
                        addr: rx_buf_frame.start_address().as_u64(),
                        length: 0,
                        checksum: 0,
                        status: 0,
                        errors: 0,
                        special: 0,
                    },
                );
                write_reg(mmio_base, REG_RDT, 1);
            }
            if crate::interrupts::timer_ticks() - rx_start_tick > 90 {
                break;
            }
            x86_64::instructions::hlt();
        }
    }
    Err("tcp_syn_test: timed out waiting for a genuine SYN-ACK reply")
}

/// Assembles a full Ethernet+IPv4+TCP frame (the header already carrying
/// a correct checksum over `payload` too, per `tcp::build_tcp_header`'s
/// own contract). A small private helper shared by `tcp_echo_test`'s
/// three distinct sends (SYN, final ACK, PSH|ACK data) so the frame-
/// assembly logic `tcp_syn_test` already proved correct isn't hand-
/// copied three times in one function.
fn build_tcp_frame(
    dest_mac: [u8; 6],
    src_mac: [u8; 6],
    src_ip: [u8; 4],
    dest_ip: [u8; 4],
    tcp_header: [u8; crate::net::tcp::TCP_HEADER_LEN],
    payload: &[u8],
) -> alloc::vec::Vec<u8> {
    use crate::net::icmp;
    let ipv4_header = icmp::build_ipv4_header(
        1,
        64,
        crate::net::tcp::IP_PROTOCOL_TCP,
        src_ip,
        dest_ip,
        tcp_header.len() + payload.len(),
    );
    let eth_header = icmp::build_ethernet_header(dest_mac, src_mac, icmp::ETHERTYPE_IPV4);
    let mut frame: alloc::vec::Vec<u8> = alloc::vec::Vec::with_capacity(
        icmp::ETHERNET_HEADER_LEN + icmp::IPV4_HEADER_LEN + tcp_header.len() + payload.len(),
    );
    frame.extend_from_slice(&eth_header);
    frame.extend_from_slice(&ipv4_header);
    frame.extend_from_slice(&tcp_header);
    frame.extend_from_slice(payload);
    frame
}

/// Parses `data` (a raw received Ethernet frame's bytes) as a
/// checksummed TCP reply from `target_ip` back to `src_ip`, returning
/// the segment's parsed fields plus its payload slice only when every
/// independent check passes (EtherType IPv4, IP protocol TCP, a valid
/// IP header checksum, source IP matches, and `net::tcp::parse_tcp_
/// segment`'s own TCP-level checksum) - the shared filtering logic
/// `tcp_echo_test`'s own multi-slot polling phases both need, factored
/// out once rather than hand-copied per phase.
fn parse_reply_frame(
    data: &[u8],
    target_ip: [u8; 4],
    src_ip: [u8; 4],
) -> Option<(crate::net::tcp::TcpSegmentInfo, &[u8])> {
    use crate::net::{icmp, tcp};
    let ip_start = icmp::ETHERNET_HEADER_LEN;
    let tcp_start = ip_start + icmp::IPV4_HEADER_LEN;
    let is_our_reply = data.len() > tcp_start
        && data[12] == 0x08
        && data[13] == 0x00
        && data[ip_start + 9] == tcp::IP_PROTOCOL_TCP
        && icmp::checksum_is_valid(&data[ip_start..tcp_start])
        && data[ip_start + 12..ip_start + 16] == target_ip;
    if !is_our_reply {
        return None;
    }
    let ip_total_len = u16::from_be_bytes([data[ip_start + 2], data[ip_start + 3]]) as usize;
    let real_tcp_len = ip_total_len.saturating_sub(icmp::IPV4_HEADER_LEN);
    let tcp_end = (tcp_start + real_tcp_len).min(data.len());
    let info = tcp::parse_tcp_segment(target_ip, src_ip, &data[tcp_start..tcp_end]).ok()?;
    // Fase 118: header_len (already validated inside parse_tcp_segment
    // itself) replaces what used to be a second, independent re-read of
    // data[tcp_start + 12] >> 4 here - the same data_offset_words value,
    // computed twice by two separate expressions that happened to agree.
    let payload_start = tcp_start + info.header_len;
    let payload = if payload_start <= tcp_end {
        &data[payload_start..tcp_end]
    } else {
        &[]
    };
    Some((info, payload))
}

/// Fase 90: picks up exactly where `tcp_syn_test` deliberately stopped
/// (its own doc comment: "does NOT yet send the final ACK ... or track
/// any ongoing connection state") and proves the resulting connection
/// is genuinely usable - not just "the peer accepted our SYN" but "the
/// peer accepts real application data over this connection and answers
/// back on it".
///
/// Sequence, against the same `guestfwd` target `tcp_syn_test` already
/// established (SLIRP's own real TCP stack, piping the byte stream to a
/// spawned `cat` process - see `boot_kernel.bat`/`kernel-ci.yml`):
///   1. SYN (own ISN `INITIAL_SEQ`) - same as `tcp_syn_test`.
///   2. Receive + validate the real SYN-ACK, capturing the peer's own
///      ISN (`info.seq_num`) - the one piece of state a genuine
///      handshake cannot skip, since the final ACK's own ack-number
///      depends on a value only the peer chose.
///   3. Final ACK (seq = `INITIAL_SEQ`+1, ack = peer ISN+1, no
///      payload): the segment `tcp_syn_test` never sent. Completes
///      the handshake; SLIRP now considers the connection
///      ESTABLISHED. No reply is expected for a bare ACK, so this
///      step doesn't wait on RX.
///   4. A real PSH|ACK data segment carrying `payload`.
///   5. Poll RX for `cat`'s own echoed reply - skipping any bare
///      (payload-less) ACK the stack may send first to acknowledge step
///      4 before `cat` itself has actually written the echo back - and
///      return the echoed bytes so the caller can compare them byte for
///      byte against what was sent.
///
/// Uses three separate TX descriptors (one per send) at fixed 256-byte
/// offsets within a single shared TX buffer page, rather than reusing
/// descriptor 0 three times: once a descriptor's DD bit is set the NIC
/// itself is done with it, but the ring's own tail pointer has already
/// advanced past it, so re-arming the same slot would need to wait for
/// a full wraparound. Advancing to a fresh descriptor index each time
/// (0, 1, 2 - well within this ring's 8-descriptor capacity) is the
/// standard, unambiguous way to queue several sends in sequence.
/// Assumes `payload` is short enough (well under the 256-byte slot
/// stride, once its own headers are counted) to share the same buffer
/// page as the SYN/ACK slots - true for this Fase's own self-test, not
/// a general-purpose send API yet.
///
/// Fase 90's own local verification hit a real, environment-specific
/// limit worth recording: on this machine's Windows QEMU build, the
/// `guestfwd=...-cmd:cat` helper process fails to spawn at all (QEMU
/// itself logs `Slirp: fork_exec: Failed to execute helper program`,
/// a POSIX-`fork()`-shaped feature with no real Windows equivalent).
/// Confirmed via a real packet capture that this does NOT indict this
/// function's own logic: the SYN, real SYN-ACK, final ACK (correct
/// seq/ack numbers), and PSH|ACK data segment all appear on the wire
/// exactly as intended - SLIRP's own stack simply tears the connection
/// down (FIN, then RST once more data arrives) because there is no
/// helper process on the other end to hand the bytes to, not because
/// anything this function sent was wrong. CI runs on `ubuntu-latest`,
/// where `cat` and POSIX `fork()`/`exec()` are native, so the full
/// round trip (including the real echo) is expected to complete there
/// even though it cannot locally on this machine.
///
/// Fase 112: `src_port`/`initial_seq` were originally internal
/// constants (`54322`/`0x3000_0000`) - now real parameters, the
/// smallest safe step toward the TCP-connection-abstraction thread's
/// own long-flagged goal (genuinely concurrent connections structurally
/// need distinct 4-tuples, which a hardcoded source port could never
/// provide). Every pre-existing caller still passes those exact
/// original values, so this is a pure signature widening with zero
/// behavior change for them. Caller-chosen, not validated here: picking
/// a value that collides with another in-flight connection's own
/// 4-tuple is a real, caller-responsibility mistake - the same trust
/// model `tcp_syn_test`'s own doc already established by picking
/// `54321` specifically to avoid colliding with this function's own
/// former hardcoded default.
pub fn tcp_echo_test(
    target_ip: [u8; 4],
    target_port: u16,
    src_port: u16,
    initial_seq: u32,
    payload: &[u8],
) -> Result<alloc::vec::Vec<u8>, &'static str> {
    use crate::net::tcp;

    let dest_mac = arp_resolve(target_ip)?;
    let (dev, mmio_base) = find_mmio_base()?;
    dev.enable_bus_mastering();
    let src_mac = read_mac(mmio_base);
    const SRC_IP: [u8; 4] = [10, 0, 2, 15]; // SLIRP guest default
    const NUM_RX_SLOTS: usize = 7; // this connection's own SYN-ACK, the stack's own immediate bare ACK, the real echo, and (Fase 102) the peer's own ACK-of-our-FIN and/or its own FIN, plus spares - bumped from 4 since the graceful close below needs real headroom beyond the handshake+echo NUM_RX_DESCRIPTORS is 8 (module-level), so 7 leaves the ring's own last slot genuinely unarmed rather than cutting it exactly at the cap
    const RX_SLOT_STRIDE: u64 = 256;

    let syn_header = tcp::build_tcp_header(
        SRC_IP,
        target_ip,
        src_port,
        target_port,
        initial_seq,
        0,
        tcp::TCP_FLAG_SYN,
        65535,
        &[],
    );
    let syn_frame = build_tcp_frame(dest_mac, src_mac, SRC_IP, target_ip, syn_header, &[]);

    let rx_ring_frame = crate::memory::frame_allocator::allocate_frame();
    let rx_buf_frame = crate::memory::frame_allocator::allocate_frame();
    let tx_ring_frame = crate::memory::frame_allocator::allocate_frame();
    let tx_buf_frame = crate::memory::frame_allocator::allocate_frame();
    let phys_offset = PHYS_MEM_OFFSET.load(Ordering::Relaxed);
    let rx_ring_virt = (phys_offset + rx_ring_frame.start_address().as_u64()) as *mut RxDescriptor;
    let tx_ring_virt = (phys_offset + tx_ring_frame.start_address().as_u64()) as *mut TxDescriptor;
    let rx_buf_virt = (phys_offset + rx_buf_frame.start_address().as_u64()) as *const u8;
    let rx_buf_phys = rx_buf_frame.start_address().as_u64();
    let tx_buf_virt = (phys_offset + tx_buf_frame.start_address().as_u64()) as *mut u8;
    let tx_buf_phys = tx_buf_frame.start_address().as_u64();
    const TX_SLOT_STRIDE: u64 = 256; // comfortably bigger than any single SYN/ACK/DATA frame here (<= 54 + payload.len() bytes)

    // Arm RX with NUM_RX_SLOTS real, distinct-buffer descriptors up
    // front, at fixed offsets within one shared buffer page (the same
    // approach the TX side below already uses) - deliberately NOT the
    // single-slot-reused-mid-flight pattern `tcp_syn_test`/`arp_resolve`/
    // `ping` all use. That pattern has a real, latent bug this function
    // is the first to actually expose: confirmed against QEMU's own
    // real `hw/net/e1000.c` (`e1000_has_rxbufs`/`e1000_receive_iov`,
    // fetched at this project's own pinned QEMU commit), the device
    // only accepts a new frame while `RDH != RDT`. Receiving one frame
    // advances `RDH` past the slot it used, and rewriting that SAME
    // slot's descriptor while re-writing `RDT` with the value it
    // already had changes nothing from the device's point of view - the
    // ring silently stays "full" (`RDH == RDT`) and every later frame is
    // dropped via `e1000_receiver_overrun`, forever. `tcp_syn_test` and
    // friends never notice, because they only ever need ONE real
    // receive per call; this function needs up to three within one
    // session (the SYN-ACK, the stack's own immediate bare ACK, and the
    // real echo), so it pre-arms enough real slots up front instead of
    // ever rearming one mid-flight.
    unsafe {
        let empty_rx = RxDescriptor {
            addr: 0,
            length: 0,
            checksum: 0,
            status: 0,
            errors: 0,
            special: 0,
        };
        for i in 0..NUM_RX_DESCRIPTORS {
            core::ptr::write_volatile(rx_ring_virt.add(i), empty_rx);
        }
        for i in 0..NUM_RX_SLOTS {
            core::ptr::write_volatile(
                rx_ring_virt.add(i),
                RxDescriptor {
                    addr: rx_buf_phys + (i as u64) * RX_SLOT_STRIDE,
                    length: 0,
                    checksum: 0,
                    status: 0,
                    errors: 0,
                    special: 0,
                },
            );
        }
        let rx_ring_phys = rx_ring_frame.start_address().as_u64();
        write_reg(mmio_base, REG_RDBAL, (rx_ring_phys & 0xFFFF_FFFF) as u32);
        write_reg(mmio_base, REG_RDBAH, (rx_ring_phys >> 32) as u32);
        write_reg(mmio_base, REG_RDLEN, (NUM_RX_DESCRIPTORS * 16) as u32);
        write_reg(mmio_base, REG_RDH, 0);
        write_reg(mmio_base, REG_RDT, NUM_RX_SLOTS as u32);
        write_reg(mmio_base, REG_RCTL, RCTL_EN | RCTL_BAM | RCTL_SECRC);
    }

    unsafe {
        let empty_tx = TxDescriptor {
            addr: 0,
            length: 0,
            cso: 0,
            cmd: 0,
            status: 0,
            css: 0,
            special: 0,
        };
        for i in 0..NUM_TX_DESCRIPTORS {
            core::ptr::write_volatile(tx_ring_virt.add(i), empty_tx);
        }

        let ctrl = read_reg(mmio_base, REG_CTRL);
        write_reg(mmio_base, REG_CTRL, ctrl | CTRL_SLU);

        let tx_ring_phys = tx_ring_frame.start_address().as_u64();
        write_reg(mmio_base, REG_TDBAL, (tx_ring_phys & 0xFFFF_FFFF) as u32);
        write_reg(mmio_base, REG_TDBAH, (tx_ring_phys >> 32) as u32);
        write_reg(mmio_base, REG_TDLEN, (NUM_TX_DESCRIPTORS * 16) as u32);
        write_reg(mmio_base, REG_TDH, 0);
        write_reg(mmio_base, REG_TDT, 0);
        write_reg(mmio_base, REG_TIPG, TIPG_DEFAULT);
        write_reg(
            mmio_base,
            REG_TCTL,
            TCTL_EN | TCTL_PSP | TCTL_CT_DEFAULT | TCTL_COLD_FULL_DUPLEX,
        );
    }

    // Queues `frame` into TX descriptor `slot`, bumps TDT to `slot + 1`,
    // and waits for that descriptor's own DD bit - `tcp_syn_test`'s
    // existing TX-wait shape, parameterized over which slot/buffer
    // offset this particular send uses.
    let send_and_wait = move |slot: usize, frame: &[u8]| -> Result<(), &'static str> {
        unsafe {
            let buf_ptr = tx_buf_virt.add(slot * TX_SLOT_STRIDE as usize);
            core::ptr::copy_nonoverlapping(frame.as_ptr(), buf_ptr, frame.len());
            core::ptr::write_volatile(
                tx_ring_virt.add(slot),
                TxDescriptor {
                    addr: tx_buf_phys + (slot as u64) * TX_SLOT_STRIDE,
                    length: frame.len() as u16,
                    cso: 0,
                    cmd: TXD_CMD_EOP | TXD_CMD_IFCS | TXD_CMD_RS,
                    status: 0,
                    css: 0,
                    special: 0,
                },
            );
            write_reg(mmio_base, REG_TDT, (slot + 1) as u32);

            let start_tick = crate::interrupts::timer_ticks();
            loop {
                let status =
                    core::ptr::read_volatile(core::ptr::addr_of!((*tx_ring_virt.add(slot)).status));
                if status & TXD_STATUS_DD != 0 {
                    return Ok(());
                }
                if crate::interrupts::timer_ticks() - start_tick > 90 {
                    return Err("tcp_echo_test: TX timed out waiting for descriptor DD");
                }
                x86_64::instructions::hlt();
            }
        }
    };

    send_and_wait(0, &syn_frame)?;

    // Phase 1: poll RX slot 0 - the first (and, for this connection's
    // own fresh 4-tuple, only) frame this session will ever receive
    // should be the real SYN-ACK. Slot 0 has exactly one real buffer
    // behind it and is never reused, so once its DD bit is set there is
    // nothing further to wait for, whether or not the frame turns out
    // to be the expected reply.
    let mut peer_isn: Option<u32> = None;
    let rx_start_tick = crate::interrupts::timer_ticks();
    unsafe {
        loop {
            let status = core::ptr::read_volatile(core::ptr::addr_of!((*rx_ring_virt).status));
            if status & RXD_STAT_DD != 0 {
                let length =
                    core::ptr::read_volatile(core::ptr::addr_of!((*rx_ring_virt).length)) as usize;
                let data = core::slice::from_raw_parts(rx_buf_virt, length);
                if let Some((info, _)) = parse_reply_frame(data, target_ip, SRC_IP) {
                    let is_syn_ack = info.flags == (tcp::TCP_FLAG_SYN | tcp::TCP_FLAG_ACK)
                        && info.source_port == target_port
                        && info.dest_port == src_port
                        && info.ack_num == initial_seq.wrapping_add(1);
                    if is_syn_ack {
                        peer_isn = Some(info.seq_num);
                    }
                }
                break;
            }
            if crate::interrupts::timer_ticks() - rx_start_tick > 90 {
                break;
            }
            x86_64::instructions::hlt();
        }
    }
    let peer_isn =
        peer_isn.ok_or("tcp_echo_test: timed out waiting for a genuine SYN-ACK reply")?;

    kprintln!(
        "[E1000] tcp_echo_test to {:?}:{}: real SYN-ACK received (peer_isn={:#x}), completing handshake",
        target_ip,
        target_port,
        peer_isn
    );

    // Phase 2: the final ACK - completes the handshake. No payload, so
    // no reply is expected; SLIRP considers the connection ESTABLISHED
    // from this point on.
    let ack_header = tcp::build_tcp_header(
        SRC_IP,
        target_ip,
        src_port,
        target_port,
        initial_seq.wrapping_add(1),
        peer_isn.wrapping_add(1),
        tcp::TCP_FLAG_ACK,
        65535,
        &[],
    );
    let ack_frame = build_tcp_frame(dest_mac, src_mac, SRC_IP, target_ip, ack_header, &[]);
    send_and_wait(1, &ack_frame)?;

    // Phase 3: the real application-data segment.
    let data_header = tcp::build_tcp_header(
        SRC_IP,
        target_ip,
        src_port,
        target_port,
        initial_seq.wrapping_add(1),
        peer_isn.wrapping_add(1),
        tcp::TCP_FLAG_PSH | tcp::TCP_FLAG_ACK,
        65535,
        payload,
    );
    let data_frame = build_tcp_frame(dest_mac, src_mac, SRC_IP, target_ip, data_header, payload);
    send_and_wait(2, &data_frame)?;

    let expected_ack = initial_seq
        .wrapping_add(1)
        .wrapping_add(payload.len() as u32);

    // Phase 4: poll RX slots 1..NUM_RX_SLOTS in arrival order for the
    // real echo - typically slot 1 is the stack's own immediate bare ACK
    // for phase 3's data (empty payload, skipped) and slot 2 is the real
    // echo, but the loop just keeps advancing through whatever pre-armed
    // slots remain rather than assuming a fixed count of interim frames.
    // Each slot has its own real, never-reused buffer, so once a slot's
    // DD bit is set there is nothing more to learn from it - move on to
    // the next slot immediately (no `hlt()`) rather than waiting, since
    // a later slot may already be ready too.
    let echo_start_tick = crate::interrupts::timer_ticks();
    let mut slot = 1usize;
    let mut echo_result: Option<(alloc::vec::Vec<u8>, u32)> = None;
    unsafe {
        loop {
            if slot >= NUM_RX_SLOTS {
                break;
            }
            let status =
                core::ptr::read_volatile(core::ptr::addr_of!((*rx_ring_virt.add(slot)).status));
            if status & RXD_STAT_DD != 0 {
                let length =
                    core::ptr::read_volatile(core::ptr::addr_of!((*rx_ring_virt.add(slot)).length))
                        as usize;
                let buf_virt = rx_buf_virt.add(slot * RX_SLOT_STRIDE as usize);
                let data = core::slice::from_raw_parts(buf_virt, length);
                if let Some((info, echoed)) = parse_reply_frame(data, target_ip, SRC_IP) {
                    let is_echo = info.source_port == target_port
                        && info.dest_port == src_port
                        && info.ack_num == expected_ack
                        && !echoed.is_empty();
                    if is_echo {
                        kprintln!(
                            "[E1000] tcp_echo_test: real echo received ({} bytes)",
                            echoed.len()
                        );
                        serial_println!(
                            "[E1000] tcp_echo_test target={:?} port={} handshake_complete=true echo_len={} echo_matches={}",
                            target_ip,
                            target_port,
                            echoed.len(),
                            echoed == payload
                        );
                        // Fase 102: capture what the close sequence below
                        // needs (the echoed bytes to still return, and
                        // the peer's own sequence number advanced past
                        // this segment) instead of returning immediately -
                        // every TCP test before this Fase left the
                        // connection dangling from this kernel's own side
                        // the moment the echo arrived.
                        let peer_seq_after_echo = info.seq_num.wrapping_add(echoed.len() as u32);
                        echo_result = Some((echoed.to_vec(), peer_seq_after_echo));
                        slot += 1;
                        break;
                    }
                }
                slot += 1;
                continue;
            }
            if crate::interrupts::timer_ticks() - echo_start_tick > 90 {
                break;
            }
            x86_64::instructions::hlt();
        }
    }
    let (echoed_vec, peer_seq_after_echo) =
        echo_result.ok_or("tcp_echo_test: timed out waiting for the real echoed data")?;

    // Phase 5 (Fase 102): a graceful close. Sends our own FIN|ACK,
    // consuming one sequence number past the data we already sent (RFC
    // 793's own convention: FIN occupies a sequence number like a data
    // byte would). `our_seq_after_data` is `expected_ack` - the same
    // value phase 4's own `is_echo` check already used to recognize the
    // peer's ACK of our data segment, since nothing between then and now
    // sends anything new on our side.
    let our_seq_after_data = expected_ack;
    let fin_header = tcp::build_tcp_header(
        SRC_IP,
        target_ip,
        src_port,
        target_port,
        our_seq_after_data,
        peer_seq_after_echo,
        tcp::TCP_FLAG_FIN | tcp::TCP_FLAG_ACK,
        65535,
        &[],
    );
    let fin_frame = build_tcp_frame(dest_mac, src_mac, SRC_IP, target_ip, fin_header, &[]);
    send_and_wait(3, &fin_frame)?;

    // Phase 6: poll whatever RX slots remain (continuing from `slot`,
    // wherever phase 4 left it - each slot's buffer is real and never
    // reused, so nothing earlier needs re-checking) for the peer's own
    // FIN. SLIRP's `cmd:cat` process sees EOF on its own stdin once our
    // FIN arrives, finishes copying, and exits - its own socket close
    // should emit a FIN back, but genuinely uncertain going in whether
    // that arrives combined with the ACK of our own FIN in one segment
    // or as a separate one, so this checks the FIN bit on whatever
    // segment shows up rather than assuming a specific flag combination.
    // If it arrives within the same generous timeout the rest of this
    // function already uses, completes the close with a final ACK.
    let mut peer_fin_received = false;
    let close_start_tick = crate::interrupts::timer_ticks();
    unsafe {
        loop {
            if slot >= NUM_RX_SLOTS {
                break;
            }
            let status =
                core::ptr::read_volatile(core::ptr::addr_of!((*rx_ring_virt.add(slot)).status));
            if status & RXD_STAT_DD != 0 {
                let length =
                    core::ptr::read_volatile(core::ptr::addr_of!((*rx_ring_virt.add(slot)).length))
                        as usize;
                let buf_virt = rx_buf_virt.add(slot * RX_SLOT_STRIDE as usize);
                let data = core::slice::from_raw_parts(buf_virt, length);
                if let Some((info, _)) = parse_reply_frame(data, target_ip, SRC_IP) {
                    let is_peer_fin = info.source_port == target_port
                        && info.dest_port == src_port
                        && info.flags & tcp::TCP_FLAG_FIN != 0;
                    if is_peer_fin {
                        peer_fin_received = true;
                        let final_ack_header = tcp::build_tcp_header(
                            SRC_IP,
                            target_ip,
                            src_port,
                            target_port,
                            our_seq_after_data.wrapping_add(1),
                            info.seq_num.wrapping_add(1),
                            tcp::TCP_FLAG_ACK,
                            65535,
                            &[],
                        );
                        let final_ack_frame = build_tcp_frame(
                            dest_mac,
                            src_mac,
                            SRC_IP,
                            target_ip,
                            final_ack_header,
                            &[],
                        );
                        send_and_wait(4, &final_ack_frame)?;
                        break;
                    }
                }
                slot += 1;
                continue;
            }
            if crate::interrupts::timer_ticks() - close_start_tick > 90 {
                break;
            }
            x86_64::instructions::hlt();
        }
    }

    serial_println!(
        "[E1000] tcp_close_test target={:?} port={} fin_sent=true peer_fin_received={}",
        target_ip,
        target_port,
        peer_fin_received
    );

    Ok(echoed_vec)
}

/// Fase 106: the first real UDP round trip this kernel has ever
/// attempted - every prior network Fase (88-102) built TCP (connection-
/// oriented) or ICMP (its own dedicated protocol number). `net::udp`
/// provides the header/checksum primitives; this function is the first
/// thing that actually sends one over the wire.
///
/// **`guestfwd` was directly confirmed NOT to support UDP before this was
/// designed, not assumed**: `qemu-system-x86_64 -netdev user,...,guestfwd=
/// udp:...` was tried locally and rejected outright with `Invalid guest
/// forwarding rule` - `guestfwd`'s own `cmd:`-backed forwarding is TCP-only
/// in real QEMU/SLIRP, so the exact mechanism every TCP test in this
/// kernel (89, 90, 102) depends on for a controlled, always-on peer
/// simply isn't available for UDP. Instead, this targets SLIRP's own
/// BUILT-IN DNS proxy, always present at `10.0.2.3:53` for any `-netdev
/// user` instance with zero extra QEMU flags - the one real, always-on
/// UDP peer genuinely available here.
///
/// This does NOT implement a DNS resolver - `net::dns::build_query`
/// (generalized in Fase 110; originally one fixed, hand-built 29-byte
/// "A? example.com" query with transaction ID `0x1234`) builds a real
/// single-question A-record query for whatever `hostname` the caller
/// supplies, used purely as real payload content a real UDP peer will
/// genuinely reply to. Parsing the reply only reads the 12-byte DNS
/// header far enough to confirm it's genuinely OUR reply (matching
/// transaction ID) and genuinely A reply (the QR bit) - never any
/// resource-record data beyond the first answer (see `parsed_a_record`
/// below), which would be real, separate, substantially larger DNS-
/// client work this Fase deliberately does not attempt.
///
/// **Genuinely, deliberately uncertain going in, the same honest posture
/// Fase 89/90/102 themselves used**: SLIRP's DNS proxy forwards to
/// whatever real resolver the HOST machine has configured, so whether it
/// replies at all depends on real network conditions this kernel has no
/// control over - unlike `ping`'s own target (SLIRP's gateway, which
/// always answers its own ICMP echo locally, no external dependency at
/// all). Because of this, only the fully deterministic fact - the query
/// was built and transmitted successfully - is represented by `Ok` at
/// all; `reply_received`/`qr_bit_set`/`answer_count` are returned and
/// logged honestly rather than assumed, to be read from real output
/// rather than assumed correct.
///
/// Fase 107: when `answer_count > 0`, also attempts `net::dns::parse_
/// first_a_record` on the reply - closing the gap this doc used to
/// flag as future work (confirming a reply exists is not the same as
/// extracting a real value FROM it). This kernel's own local dev
/// environment observed `answer_count=0` for every real Fase 106 run
/// (only the real CI runner's own network path returned actual
/// records), so `parsed_a_record` is expected to stay `None` here on
/// this machine - genuinely environment-dependent, logged honestly
/// rather than assumed either way.
///
/// Fase 110: `hostname` replaces the old fixed "example.com" query
/// content - a caller-supplied name is now genuinely encoded onto the
/// wire via `dns::build_query`/`dns::encode_qname`, rather than only
/// the target DNS *server* being a parameter. An invalid hostname
/// (empty, an empty label, or a label over 63 bytes) is rejected by
/// `build_query` and propagated here as `Err` before anything is ever
/// sent - the same "fail before touching hardware" discipline this
/// function already used for `arp_resolve`'s own `?`.
/// `(reply_received, qr_bit_set, answer_count, parsed_a_record)`.
pub type DnsQueryResult = Result<(bool, bool, u16, Option<[u8; 4]>), &'static str>;

pub fn dns_query_test(target_ip: [u8; 4], hostname: &str) -> DnsQueryResult {
    use crate::net::{dns, icmp, udp};

    // Fase 110: the query content itself is now built for an arbitrary
    // hostname via dns::build_query rather than being one fixed 29-byte
    // array - see that function's own doc for the exact wire layout
    // (unchanged from Fase 106's original hand-built bytes when
    // hostname="example.com" and transaction_id=0x1234).
    const DNS_TRANSACTION_ID: u16 = 0x1234;
    let query = dns::build_query(DNS_TRANSACTION_ID, hostname)?;

    let dest_mac = arp_resolve(target_ip)?;
    let (dev, mmio_base) = find_mmio_base()?;
    dev.enable_bus_mastering();
    let src_mac = read_mac(mmio_base);
    const SRC_IP: [u8; 4] = [10, 0, 2, 15]; // SLIRP guest default
    const SRC_PORT: u16 = 45678;
    const DEST_PORT: u16 = 53;

    let udp_header = udp::build_udp_header(SRC_IP, target_ip, SRC_PORT, DEST_PORT, &query);
    let ipv4_header = icmp::build_ipv4_header(
        1,
        64,
        udp::IP_PROTOCOL_UDP,
        SRC_IP,
        target_ip,
        udp_header.len() + query.len(),
    );
    let eth_header = icmp::build_ethernet_header(dest_mac, src_mac, icmp::ETHERTYPE_IPV4);

    let mut frame: alloc::vec::Vec<u8> = alloc::vec::Vec::with_capacity(
        icmp::ETHERNET_HEADER_LEN + icmp::IPV4_HEADER_LEN + udp_header.len() + query.len(),
    );
    frame.extend_from_slice(&eth_header);
    frame.extend_from_slice(&ipv4_header);
    frame.extend_from_slice(&udp_header);
    frame.extend_from_slice(&query);

    let rx_ring_frame = crate::memory::frame_allocator::allocate_frame();
    let rx_buf_frame = crate::memory::frame_allocator::allocate_frame();
    let tx_ring_frame = crate::memory::frame_allocator::allocate_frame();
    let tx_buf_frame = crate::memory::frame_allocator::allocate_frame();
    let phys_offset = PHYS_MEM_OFFSET.load(Ordering::Relaxed);
    let rx_ring_virt = (phys_offset + rx_ring_frame.start_address().as_u64()) as *mut RxDescriptor;
    let tx_ring_virt = (phys_offset + tx_ring_frame.start_address().as_u64()) as *mut TxDescriptor;
    let tx_buf_virt = (phys_offset + tx_buf_frame.start_address().as_u64()) as *mut u8;

    // Arm RX first, same ordering reasoning as arp_resolve/ping/tcp_syn_test.
    unsafe {
        let empty_rx = RxDescriptor {
            addr: 0,
            length: 0,
            checksum: 0,
            status: 0,
            errors: 0,
            special: 0,
        };
        for i in 0..NUM_RX_DESCRIPTORS {
            core::ptr::write_volatile(rx_ring_virt.add(i), empty_rx);
        }
        core::ptr::write_volatile(
            rx_ring_virt,
            RxDescriptor {
                addr: rx_buf_frame.start_address().as_u64(),
                length: 0,
                checksum: 0,
                status: 0,
                errors: 0,
                special: 0,
            },
        );
        let rx_ring_phys = rx_ring_frame.start_address().as_u64();
        write_reg(mmio_base, REG_RDBAL, (rx_ring_phys & 0xFFFF_FFFF) as u32);
        write_reg(mmio_base, REG_RDBAH, (rx_ring_phys >> 32) as u32);
        write_reg(mmio_base, REG_RDLEN, (NUM_RX_DESCRIPTORS * 16) as u32);
        write_reg(mmio_base, REG_RDH, 0);
        write_reg(mmio_base, REG_RDT, 1);
        write_reg(mmio_base, REG_RCTL, RCTL_EN | RCTL_BAM | RCTL_SECRC);
    }

    unsafe {
        core::ptr::copy_nonoverlapping(frame.as_ptr(), tx_buf_virt, frame.len());

        let empty_tx = TxDescriptor {
            addr: 0,
            length: 0,
            cso: 0,
            cmd: 0,
            status: 0,
            css: 0,
            special: 0,
        };
        for i in 0..NUM_TX_DESCRIPTORS {
            core::ptr::write_volatile(tx_ring_virt.add(i), empty_tx);
        }
        core::ptr::write_volatile(
            tx_ring_virt,
            TxDescriptor {
                addr: tx_buf_frame.start_address().as_u64(),
                length: frame.len() as u16,
                cso: 0,
                cmd: TXD_CMD_EOP | TXD_CMD_IFCS | TXD_CMD_RS,
                status: 0,
                css: 0,
                special: 0,
            },
        );

        let ctrl = read_reg(mmio_base, REG_CTRL);
        write_reg(mmio_base, REG_CTRL, ctrl | CTRL_SLU);

        let tx_ring_phys = tx_ring_frame.start_address().as_u64();
        write_reg(mmio_base, REG_TDBAL, (tx_ring_phys & 0xFFFF_FFFF) as u32);
        write_reg(mmio_base, REG_TDBAH, (tx_ring_phys >> 32) as u32);
        write_reg(mmio_base, REG_TDLEN, (NUM_TX_DESCRIPTORS * 16) as u32);
        write_reg(mmio_base, REG_TDH, 0);
        write_reg(mmio_base, REG_TDT, 0);
        write_reg(mmio_base, REG_TIPG, TIPG_DEFAULT);
        write_reg(
            mmio_base,
            REG_TCTL,
            TCTL_EN | TCTL_PSP | TCTL_CT_DEFAULT | TCTL_COLD_FULL_DUPLEX,
        );
        write_reg(mmio_base, REG_TDT, 1);
    }

    let tx_start_tick = crate::interrupts::timer_ticks();
    let mut tx_ok = false;
    unsafe {
        loop {
            let status = core::ptr::read_volatile(core::ptr::addr_of!((*tx_ring_virt).status));
            if status & TXD_STATUS_DD != 0 {
                tx_ok = true;
                break;
            }
            if crate::interrupts::timer_ticks() - tx_start_tick > 90 {
                break;
            }
            x86_64::instructions::hlt();
        }
    }
    if !tx_ok {
        return Err("dns_query_test: TX timed out waiting for descriptor DD");
    }

    let mut reply_received = false;
    let mut qr_bit_set = false;
    let mut answer_count: u16 = 0;
    let mut parsed_a_record: Option<[u8; 4]> = None;
    let rx_start_tick = crate::interrupts::timer_ticks();
    unsafe {
        loop {
            let status = core::ptr::read_volatile(core::ptr::addr_of!((*rx_ring_virt).status));
            if status & RXD_STAT_DD != 0 {
                let length =
                    core::ptr::read_volatile(core::ptr::addr_of!((*rx_ring_virt).length)) as usize;
                let buf_virt = (phys_offset + rx_buf_frame.start_address().as_u64()) as *const u8;
                let data = core::slice::from_raw_parts(buf_virt, length);

                // Ethernet: EtherType IPv4. IPv4: protocol UDP, source
                // address is the DNS proxy we queried. Each check is
                // independent real evidence, the same reasoning ping's
                // own is_our_reply check already established.
                let ip_start = icmp::ETHERNET_HEADER_LEN;
                let udp_start = ip_start + icmp::IPV4_HEADER_LEN;
                let dns_start = udp_start + udp::UDP_HEADER_LEN;
                let is_our_reply = data.len() > dns_start
                    && data[12] == 0x08
                    && data[13] == 0x00
                    && data[ip_start + 9] == udp::IP_PROTOCOL_UDP
                    && icmp::checksum_is_valid(&data[ip_start..udp_start])
                    && data[ip_start + 12..ip_start + 16] == target_ip;

                // Fase 89's own real-world bug (tcp_syn_test) applies
                // identically here: the received buffer's own length is
                // NOT the same as the real UDP datagram's length - a
                // reply is very likely padded up to Ethernet's 60-byte
                // minimum frame size, extra bytes the sender's own UDP
                // length field never counted and that must NOT be fed
                // into the pseudo-header checksum sum. The IPv4 header's
                // own Total Length field is the one authoritative source
                // for how many bytes past the IP header genuinely belong
                // to this packet.
                let ip_total_len = if is_our_reply {
                    u16::from_be_bytes([data[ip_start + 2], data[ip_start + 3]]) as usize
                } else {
                    0
                };
                let real_udp_len = ip_total_len.saturating_sub(icmp::IPV4_HEADER_LEN);
                let udp_end = (udp_start + real_udp_len).min(data.len());

                if is_our_reply {
                    if let Ok(info) =
                        udp::parse_udp_datagram(target_ip, SRC_IP, &data[udp_start..udp_end])
                    {
                        if info.dest_port == SRC_PORT
                            && info.source_port == DEST_PORT
                            && dns_start + 8 <= udp_end
                            && data[dns_start..dns_start + 2] == DNS_TRANSACTION_ID.to_be_bytes()
                        {
                            reply_received = true;
                            qr_bit_set = data[dns_start + 2] & 0x80 != 0;
                            answer_count =
                                u16::from_be_bytes([data[dns_start + 6], data[dns_start + 7]]);
                            if answer_count > 0 {
                                let question_len = query.len() - dns::DNS_HEADER_LEN;
                                parsed_a_record = dns::parse_first_a_record(
                                    &data[dns_start..udp_end],
                                    question_len,
                                )
                                .ok();
                            }
                            break;
                        }
                    }
                }

                // Not our reply - rearm this same descriptor and keep
                // polling, same reasoning as arp_resolve/ping.
                core::ptr::write_volatile(
                    rx_ring_virt,
                    RxDescriptor {
                        addr: rx_buf_frame.start_address().as_u64(),
                        length: 0,
                        checksum: 0,
                        status: 0,
                        errors: 0,
                        special: 0,
                    },
                );
                write_reg(mmio_base, REG_RDT, 1);
            }
            if crate::interrupts::timer_ticks() - rx_start_tick > 90 {
                break;
            }
            x86_64::instructions::hlt();
        }
    }

    kprintln!(
        "[E1000] dns_query_test to {:?}:53 (hostname={}) - sent=true reply_received={} qr_bit_set={} answer_count={} parsed_a_record={:?}",
        target_ip,
        hostname,
        reply_received,
        qr_bit_set,
        answer_count,
        parsed_a_record
    );
    serial_println!(
        "[E1000] dns_query_test target={:?} port=53 sent=true reply_received={} qr_bit_set={} answer_count={} parsed_a_record={:?} hostname={}",
        target_ip,
        reply_received,
        qr_bit_set,
        answer_count,
        parsed_a_record,
        hostname
    );

    Ok((reply_received, qr_bit_set, answer_count, parsed_a_record))
}

/// Fase 113: wraps `dns_query_test` into a clean, reusable resolver API -
/// closes the "reusable hostname-lookup API" item the DNS-resolver
/// thread's own doc has flagged open since Fase 107. Queries
/// `dns_server` for `hostname`'s A record and returns just the resolved
/// IPv4 address, collapsing `dns_query_test`'s own full diagnostic
/// tuple (built for a boot self-test/interactive `dns` command that
/// wants to SEE every field) down to the one value an actual CALLER -
/// something that just wants an address to use, not to inspect the
/// protocol - needs. Still not a general resolver: single question, A
/// records only, the same standing scope this whole DNS arc has kept
/// since Fase 106 - this closes the "usable by other kernel code" gap
/// specifically, not the general-resolution gap (multiple answers, full
/// name-compression chains, other record types all remain separate,
/// larger follow-on work if ever needed).
pub fn resolve_hostname(hostname: &str, dns_server: [u8; 4]) -> Result<[u8; 4], &'static str> {
    let (reply_received, qr_bit_set, _answer_count, parsed_a_record) =
        dns_query_test(dns_server, hostname)?;
    if !reply_received || !qr_bit_set {
        return Err("resolve_hostname: no valid DNS reply received");
    }
    parsed_a_record.ok_or("resolve_hostname: reply contained no parseable A record")
}
