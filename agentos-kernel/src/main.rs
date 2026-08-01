#![no_std]
#![no_main]
#![allow(dead_code)]
#![feature(abi_x86_interrupt)]

extern crate alloc;

use core::panic::PanicInfo;

mod ata;
mod fat12;
mod fat32;
mod fat_common;
mod gdt;
mod gguf_loader;
mod interrupts;
mod keyboard;
mod memory;
mod net;
mod partition;
mod pci;
mod rtc;
mod scheduler;
mod serial;
mod shell;
mod syscall;
mod tensor_engine;
mod vga_buffer;

use alloc::boxed::Box;
use alloc::vec::Vec;
use gguf_loader::{GgufGtype, GgufModelLoader, GgufTensorInfo};
use memory::kv_allocator::KV_MANAGER;
use net::tcpip::NativeNetworkStack;
use scheduler::agent_scheduler::SCHEDULER;
use scheduler::process::Priority;

use bootloader_api::config::{BootloaderConfig, Mapping};
use bootloader_api::{entry_point, BootInfo};

/// Physical memory must be mapped so vga_buffer can reach the 0xb8000 text
/// buffer through `physical_memory_offset` (see BootInfo docs) instead of a
/// raw, unmapped physical address.
pub static BOOTLOADER_CONFIG: BootloaderConfig = {
    let mut config = BootloaderConfig::new_default();
    config.mappings.physical_memory = Some(Mapping::Dynamic);
    config
};

entry_point!(kernel_main, config = &BOOTLOADER_CONFIG);

/// Kernel Entry Point, invoked by the `bootloader` crate after it has already
/// switched the CPU to 64-bit long mode and set up paging - the multiboot2
/// hand-rolled entry (`_start` + a 32-bit trampoline we never wrote) is gone.
fn kernel_main(boot_info: &'static mut BootInfo) -> ! {
    vga_buffer::PHYS_MEM_OFFSET.store(
        boot_info.physical_memory_offset.into_option().unwrap_or(0),
        core::sync::atomic::Ordering::Relaxed,
    );

    // 1. Initialize Hardware Displays & Debug Serial Output
    kprintln!("==================================================");
    kprintln!("   AgentOS Native Bare-Metal Kernel v1.0 (Rust)   ");
    kprintln!("==================================================");

    serial_println!("==================================================");
    serial_println!("   AgentOS Native Bare-Metal Kernel Initialized   ");
    serial_println!("==================================================");

    // 2. Initialize GDT & IDT
    kprintln!("[KERNEL INIT] Initializing GDT & Task State Segment...");
    gdt::init();

    kprintln!("[KERNEL INIT] Loading IDT Interrupt Handlers...");
    interrupts::init_idt();

    // 2b. Smoke-test the breakpoint handler: if this line is followed by
    // "[EXCEPTION] Breakpoint..." instead of a reset/hang, exception handling works.
    kprintln!("[KERNEL INIT] Testing breakpoint exception handler (int3)...");
    x86_64::instructions::interrupts::int3();
    kprintln!("[KERNEL INIT] Execution resumed after breakpoint - handler OK.");

    // 2c. Remap the 8259 PIC & Enable Hardware Interrupts
    // Must happen in this order: the IDT (loaded above) already has real
    // handlers at vectors 32/33, so it's now safe to let the PIC start
    // firing IRQs and to flip the CPU's IF flag. Doing this before the IDT
    // had those handlers, or before the PIC remap, would turn the first
    // timer tick into an unhandled-vector fault.
    interrupts::init_pics();
    x86_64::instructions::interrupts::enable();
    kprintln!("[KERNEL INIT] Hardware interrupts enabled (STI) - keyboard is now IRQ-driven.");

    // 2d. Map Real Physical Memory & Initialize the Heap Allocator
    // Everything below this point (KV cache, scheduler, GGUF loader) still
    // uses fixed-size static arrays, not `alloc` - but a real AI-native
    // kernel needs to load model files of arbitrary size, which fixed arrays
    // can't do. This makes `alloc` (Vec/Box/String) actually usable for the
    // first time; previously `heap::init_heap` initialized the allocator
    // over a virtual range that was never mapped to physical memory, so the
    // first real allocation would have page-faulted.
    kprintln!("[KERNEL INIT] Mapping physical memory & initializing heap allocator...");
    let phys_mem_offset =
        x86_64::VirtAddr::new(boot_info.physical_memory_offset.into_option().unwrap_or(0));
    let mut mapper = unsafe { memory::init(phys_mem_offset) };
    let mut frame_allocator =
        unsafe { memory::frame_allocator::BootInfoFrameAllocator::init(&boot_info.memory_regions) };
    memory::heap::init_heap(&mut mapper, &mut frame_allocator).expect("heap initialization failed");
    kprintln!(
        "[KERNEL INIT] Heap mapped at {:#x}, {} KiB - alloc (Vec/Box/String) now live.",
        memory::heap::HEAP_START,
        memory::heap::HEAP_SIZE / 1024
    );

    // Hands the SAME allocator instance (cursor already advanced past
    // whatever heap init just claimed) to a global slot so later code -
    // e.g. a future real NIC driver's TX/RX descriptor rings - can keep
    // allocating fresh physical frames after boot, without re-handing-out
    // frames the heap already owns.
    memory::frame_allocator::install_global(frame_allocator);

    // Proves the global allocator just installed hands out genuinely
    // usable, distinct physical frames - not just plausible-looking
    // numbers. Writes then reads back a small pattern through each
    // frame's PHYS_MEM_OFFSET-mapped virtual address (the same
    // identity-mapping mechanism vga_buffer.rs and net/e1000.rs already
    // rely on) - if that mapping or the frame itself were somehow wrong,
    // this panics here with a clear message instead of corrupting
    // something later, silently, whenever real DMA buffers eventually
    // use this same allocator.
    kprintln!("[KERNEL INIT] Testing global frame allocator...");
    {
        let frames = [
            memory::frame_allocator::allocate_frame(),
            memory::frame_allocator::allocate_frame(),
            memory::frame_allocator::allocate_frame(),
        ];
        for (i, frame) in frames.iter().enumerate() {
            let phys = frame.start_address().as_u64();
            let virt = (phys_mem_offset.as_u64() + phys) as *mut u8;
            let pattern = 0xA0 + i as u8;
            unsafe {
                core::ptr::write_volatile(virt, pattern);
                let read_back = core::ptr::read_volatile(virt);
                assert_eq!(
                    read_back, pattern,
                    "frame {} at phys {:#x} not genuinely writable/readable",
                    i, phys
                );
            }
        }
        kprintln!(
            "[FRAME ALLOC] 3 fresh frames: {:#x}, {:#x}, {:#x} (write/read-back verified)",
            frames[0].start_address().as_u64(),
            frames[1].start_address().as_u64(),
            frames[2].start_address().as_u64()
        );
        serial_println!(
            "[FRAME ALLOC] frames={:#x},{:#x},{:#x} verified=true",
            frames[0].start_address().as_u64(),
            frames[1].start_address().as_u64(),
            frames[2].start_address().as_u64()
        );
    }

    kprintln!("[KERNEL INIT] Testing heap allocator (Box + Vec)...");
    {
        let boxed = Box::new(41 + 1);
        let mut v: Vec<u32> = Vec::new();
        for i in 0..5 {
            v.push(i * i);
        }
        kprintln!(
            "[HEAP TEST] Box({}) at {:p}; Vec squares = {:?}",
            boxed,
            boxed,
            v
        );
    }

    // 3. Initialize Native Agent Multitask Scheduler
    kprintln!("[KERNEL INIT] Starting Agent Multitask Scheduler...");
    {
        let mut sched = SCHEDULER.lock();
        let _pid1 = sched.spawn("kernel-supervisord", Priority::KernelCritical, 50000);
        let _pid2 = sched.spawn("mal-native-engine", Priority::High, 25000);
        let _pid3 = sched.spawn("kv-cache-pager", Priority::High, 25000);

        kprintln!("[SCHEDULER] Spawned PID 1: kernel-supervisord");
        kprintln!("[SCHEDULER] Spawned PID 2: mal-native-engine");
        kprintln!("[SCHEDULER] Spawned PID 3: kv-cache-pager");

        if let Some(next_pid) = sched.schedule_next() {
            kprintln!("[SCHEDULER] Switched context to active PID: {}", next_pid);
        }
    }

    // 4. Test Native KV Cache Memory Allocation
    kprintln!("[KERNEL INIT] Initializing KV Cache Memory Manager...");
    {
        let mut kv = KV_MANAGER.lock();
        if let Some(block_id) = kv.allocate_kv_block(2, 2048) {
            kprintln!(
                "[KV MEMORY] Allocated KV Cache Block #{} for PID 2 in VRAM",
                block_id
            );
        }
    }

    // 5. Test GGUF Quantized Model Parser & Tensor Matrix Execution - the
    // header and tensor weight values have genuinely round-tripped
    // through the FAT12 disk since the prior two Fases (create -> read
    // back -> parse) instead of living only as compile-time arrays. This
    // Fase adds the piece both of those deliberately left out: a real
    // tensor-info entry (length-prefixed name, dimensions, quantization
    // type, and a byte OFFSET to that tensor's own data elsewhere in the
    // file - `GgufTensorInfo::parse`) sits between the header and the
    // weight bytes, and the weight bytes are now located by *following
    // that offset* rather than assumed to start immediately after a
    // fixed-size header. `MODEL.BIN` deliberately fits plain 8.3 (avoids
    // incidentally exercising VFAT again here - that's already its own
    // dedicated self-test). Still a deliberately simplified, honest
    // slice of real GGUF, not a compliance claim: `u32` fields instead
    // of GGUF's `u64` (more range than this kernel's own toy tensors
    // will ever need), exactly one tensor-info entry (not `tensor_count`
    // of them - `tensor_count` is set to 1 here specifically so the
    // header's own declared count matches what's actually present, no
    // longer just a cosmetic unused number), and no KV-metadata section
    // (`kv_count` set to 0, honestly reflecting that none is written or
    // parsed). Loading real *quantized* tensor formats is separate,
    // larger scope, not attempted here.
    kprintln!("[GGUF INFERENCE] Testing Native GGUF Header Parser & Tensor Weights...");
    const GGUF_HEADER: [u8; 24] = [
        0x47, 0x47, 0x55, 0x46, // "GGUF"
        0x03, 0x00, 0x00, 0x00, // Version 3
        0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // 1 Tensor (the real entry below)
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // 0 KV Pairs (none written/parsed)
    ];
    const TENSOR_NAME: &str = "weights";
    const GGUF_WEIGHTS: [f32; 16] = [
        0.5, -0.2, 0.8, 0.1, 0.3, 0.9, -0.4, 0.6, -0.1, 0.4, 0.7, 0.2, 0.6, -0.5, 0.2, 0.8,
    ];

    let mut model_file_bytes = alloc::vec::Vec::new();
    model_file_bytes.extend_from_slice(&GGUF_HEADER);
    model_file_bytes.extend_from_slice(&(TENSOR_NAME.len() as u32).to_le_bytes());
    model_file_bytes.extend_from_slice(TENSOR_NAME.as_bytes());
    model_file_bytes.extend_from_slice(&4u32.to_le_bytes()); // dim0
    model_file_bytes.extend_from_slice(&4u32.to_le_bytes()); // dim1
    model_file_bytes.extend_from_slice(&(GgufGtype::F32 as u32).to_le_bytes());
    // Where the tensor data will actually land, once the offset field
    // itself (4 more bytes) is appended right after this comment - NOT
    // assumed by the reader, computed here only to construct a valid
    // file and to check the *parsed* offset matches it later.
    let tensor_data_offset = model_file_bytes.len() + 4;
    model_file_bytes.extend_from_slice(&(tensor_data_offset as u32).to_le_bytes());
    for w in GGUF_WEIGHTS {
        model_file_bytes.extend_from_slice(&w.to_le_bytes());
    }

    match shell::find_fat_partition() {
        Ok(partition) => match fat12::read_bpb(&partition) {
            Ok(mut fs) => {
                let write_ok = fs.create_file("MODEL.BIN", &model_file_bytes).is_ok();
                let read_back = fs.read_file("MODEL.BIN");
                let round_trip_ok = read_back.as_deref() == Ok(model_file_bytes.as_slice());
                let cleanup_ok = fs.delete_file("MODEL.BIN").is_ok();

                let mut tensor_info_ok = false;
                let mut weights_decoded_ok = false;
                let mut tensor_output_ok = false;
                if let Ok(full_bytes) = &read_back {
                    if let Ok(loader) = GgufModelLoader::parse_header(&full_bytes[0..24]) {
                        if let Ok((info, _consumed)) = GgufTensorInfo::parse(&full_bytes[24..]) {
                            tensor_info_ok = info.name == TENSOR_NAME
                                && info.dimensions == [4, 4]
                                && info.gtype == GgufGtype::F32
                                && info.offset == tensor_data_offset;

                            // Located by following the parsed offset, not
                            // by assuming it sits right after the header -
                            // the real proof this Fase's offset-based
                            // indirection actually works, not just parses.
                            let tensor_bytes = &full_bytes[info.offset..info.offset + 64];
                            let weights = GgufModelLoader::decode_f32_le(tensor_bytes);
                            // Lossless byte round trip (to_le_bytes ->
                            // disk -> from_le_bytes), no arithmetic
                            // involved, so bit-for-bit equality is the
                            // correct check here, not a tolerance one.
                            weights_decoded_ok = weights == GGUF_WEIGHTS;

                            let inputs: [f32; 4] = [1.0, 2.0, 0.5, 3.0];
                            let mut outputs: [f32; 4] = [0.0; 4];
                            loader.execute_gguf_layer_pass(&weights, &inputs, &mut outputs, 4, 4);

                            kprintln!(
                                "[GGUF RESULT] Y = ReLU(W * X + B) -> [{:.2}, {:.2}, {:.2}, {:.2}]",
                                outputs[0],
                                outputs[1],
                                outputs[2],
                                outputs[3]
                            );
                            const EXPECTED_OUTPUTS: [f32; 4] = [0.8, 3.7, 1.65, 2.1];
                            tensor_output_ok = outputs
                                .iter()
                                .zip(EXPECTED_OUTPUTS.iter())
                                .all(|(a, b)| (a - b).abs() < 0.001);
                        }
                    }
                }

                kprintln!(
                    "[GGUF LOADER] disk round-trip: write_ok={} round_trip_ok={} tensor_info_ok={} weights_decoded_ok={} tensor_output_ok={} cleanup_ok={}",
                    write_ok, round_trip_ok, tensor_info_ok, weights_decoded_ok, tensor_output_ok, cleanup_ok
                );
                serial_println!(
                    "[GGUF] disk_load_test write_ok={} round_trip_ok={} tensor_info_ok={} weights_decoded_ok={} tensor_output_ok={} cleanup_ok={}",
                    write_ok, round_trip_ok, tensor_info_ok, weights_decoded_ok, tensor_output_ok, cleanup_ok
                );
            }
            Err(e) => {
                kprintln!("[GGUF LOADER] disk round-trip: not FAT12 ({})", e);
                serial_println!("[GGUF] disk_load_test -> not fat12: {}", e);
            }
        },
        Err(e) => {
            kprintln!(
                "[GGUF LOADER] disk round-trip: couldn't find FAT partition: {}",
                e
            );
            serial_println!("[GGUF] disk_load_test -> no partition: {}", e);
        }
    }

    // 6. Test Native VirtIO-Net & TCP/IPv4 Network Stack
    kprintln!("[NET INIT] Initializing VirtIO-Net Hardware Adapter & TCP/IP Stack...");
    NativeNetworkStack::send_ipv4_packet([192, 168, 1, 1], b"AgentOS Kernel Online");

    // 7. Invoke System Calls (Syscalls)
    kprintln!("[KERNEL SYSCALL] Testing Native Agent Syscall Dispatcher...");
    syscall::dispatch_syscall(syscall::SYS_SERIAL_PRINT, 0, 0, 0);
    let spawned_pid = syscall::dispatch_syscall(syscall::SYS_AGENT_SPAWN, 10000, 0, 0);
    syscall::dispatch_syscall(syscall::SYS_KV_ALLOC, spawned_pid, 1024, 0);

    // 7b. Test the Shell Command Dispatcher (same parser the IRQ1 keyboard
    // handler calls once a real key press ends a line - this exercises it
    // without needing one, so it's provable from a headless serial capture.
    kprintln!("[KERNEL INIT] Testing shell command dispatcher (help, ps, mem)...");
    shell::dispatch_command("help");
    shell::dispatch_command("ps");
    shell::dispatch_command("mem");
    shell::dispatch_command("uptime");
    shell::dispatch_command("date");
    shell::dispatch_command("lspci");

    // 7b-2. First real step toward a real e1000 NIC driver: find the real
    // device lspci above just confirmed is present, reach its
    // memory-mapped registers (reusing the same PHYS_MEM_OFFSET mechanism
    // vga_buffer.rs already relies on for the VGA text buffer - see
    // net/e1000.rs's module doc), and read its real STATUS/MAC registers.
    net::e1000::probe();

    // 7b-3/7b-4. Real e1000 TX+RX attempt: `receive_test_frame` arms a
    // real receive descriptor ring FIRST, then calls `send_test_frame`
    // internally (building a real transmit descriptor ring using fresh
    // physical frames from the global allocator - Fase 21 - and handing
    // a real ARP request to the hardware), then polls for a genuine
    // reply. TX is fully confirmed working as of Fase 44: the ring's
    // physical address and descriptor content are correct, the hardware
    // genuinely dequeues the descriptor (TDH advances), AND the
    // Descriptor-Done status bit now gets written back too - see
    // net/e1000.rs's module doc for the two-round investigation and its
    // actual resolution (`PciDevice::enable_bus_mastering` - PCI Bus
    // Mastering was never enabled, so the DMA write had nowhere real to
    // land). RX (Fase 45) is the first real attempt at receiving
    // anything - honestly uncertain going in whether QEMU's SLIRP
    // backend actually replies to the ARP request in a way this ring
    // picks up; see net/e1000.rs's own doc and this Fase's README/memory
    // notes for the real, observed outcome rather than an assumption.
    match net::e1000::receive_test_frame() {
        Ok(()) => {}
        Err(e) => {
            kprintln!("[E1000] receive_test_frame: {}", e);
            serial_println!("[E1000] rx_test -> not confirmed: {}", e);
        }
    }

    // 7b-5. Test the new `netcheck` shell command (Fase 46) - proves the
    // same TX+RX capability is reachable through the real
    // dispatch_command parsing path, not just the direct API call
    // above, matching this session's established "prove the API, then
    // prove the shell path separately" pattern.
    shell::dispatch_command("netcheck");

    shell::dispatch_command("disk");
    shell::dispatch_command("ls");
    shell::dispatch_command("cat KERNEL~1");

    // 7b-4. Real ATA PIO sector write - prerequisite for any future FAT
    // write support, same relationship Fase 21's frame allocator had to
    // e1000 TX. Writes a recognizable test pattern to a sector safely
    // within the FAT12 partition's likely-unused tail (well past where
    // real file data/metadata reaches - the files read above total well
    // under 1000 sectors out of this partition's 4096), reads it back,
    // and confirms it matches byte-for-byte - proof the write genuinely
    // reached disk, not just that the function returned without
    // erroring. Safe to run every boot: kernel-runner regenerates the
    // disk image fresh each time, so nothing persists across runs anyway.
    kprintln!("[KERNEL INIT] Testing real ATA sector write (write + read-back)...");
    match shell::find_fat_partition() {
        Ok(partition) => {
            let test_lba = partition.start_lba + 3000;
            let test_pattern = [0xA5u8; 512];
            match ata::write_sector(test_lba, &test_pattern) {
                Ok(()) => {
                    let mut read_back = [0u8; 512];
                    match ata::read_sector(test_lba, &mut read_back) {
                        Ok(()) => {
                            let matches = read_back == test_pattern;
                            kprintln!(
                                "[ATA] write+read-back test at LBA {}: {}",
                                test_lba,
                                if matches {
                                    "OK (bytes match)"
                                } else {
                                    "MISMATCH"
                                }
                            );
                            serial_println!("[ATA] write_test lba={} match={}", test_lba, matches);
                        }
                        Err(e) => {
                            kprintln!("[ATA] write test: read-back failed: {}", e);
                            serial_println!("[ATA] write_test lba={} read_failed: {}", test_lba, e);
                        }
                    }
                }
                Err(e) => {
                    kprintln!("[ATA] write test: write failed: {}", e);
                    serial_println!("[ATA] write_test lba={} write_failed: {}", test_lba, e);
                }
            }
        }
        Err(e) => {
            kprintln!("[ATA] write test: couldn't find FAT partition: {}", e);
            serial_println!("[ATA] write_test -> no partition: {}", e);
        }
    }

    // 7b-5. Real FAT12 file write, built on the ATA write primitive above
    // - the simplest possible case: overwrite an EXISTING file's content
    // with new data of the exact same length, reusing its existing
    // cluster chain as-is (no FAT/directory-entry changes). Creating a
    // new file or resizing one both need real cluster (re)allocation -
    // substantially more work, deliberately not attempted in this same
    // iteration.
    //
    // Targets BOOT-S~2 - but restores its ORIGINAL content afterward,
    // unconditionally. Found the hard way (a repeat-boot-test caught it
    // immediately): "BOOT-S~1"/"BOOT-S~2" are not inert data files -
    // they're the `bootloader` crate's own boot-stage executables that
    // the BIOS runs *before* this kernel ever starts. QEMU's `-drive`
    // writes straight through to the host .img file, so overwriting
    // BOOT-S~2 with test bytes and leaving it that way corrupts the boot
    // chain for every *subsequent* boot against that same image (the
    // first boot still looks fine, since the corruption happens *after*
    // that boot's own chain already handed off to this kernel) - the
    // symptom was a completely empty serial log on the next boot, no
    // panic, nothing, because the corrupted stage 2 never got far enough
    // to run any of this kernel's code at all. Restoring the original
    // content before returning is what makes this self-test safe to run
    // on every boot, unlike a persistent-effect assumption that turned
    // out to be wrong.
    kprintln!("[KERNEL INIT] Testing real FAT12 file write (overwrite + read-back)...");
    match shell::find_fat_partition() {
        Ok(partition) => match fat12::read_bpb(&partition) {
            Ok(mut fs) => match fs.read_file("BOOT-S~2") {
                Ok(original) => {
                    let test_pattern: alloc::vec::Vec<u8> =
                        (0..original.len()).map(|i| (i % 256) as u8).collect();
                    match fs.write_file("BOOT-S~2", &test_pattern) {
                        Ok(()) => match fs.read_file("BOOT-S~2") {
                            Ok(read_back) => {
                                let matches = read_back == test_pattern;
                                kprintln!(
                                    "[FAT12] write+read-back test on BOOT-S~2 ({} bytes): {}",
                                    test_pattern.len(),
                                    if matches {
                                        "OK (bytes match)"
                                    } else {
                                        "MISMATCH"
                                    }
                                );
                                serial_println!(
                                    "[FAT12] write_test file=BOOT-S~2 len={} match={}",
                                    test_pattern.len(),
                                    matches
                                );
                            }
                            Err(e) => {
                                kprintln!("[FAT12] write test: read-back failed: {}", e);
                                serial_println!("[FAT12] write_test read_failed: {}", e);
                            }
                        },
                        Err(e) => {
                            kprintln!("[FAT12] write test: write failed: {}", e);
                            serial_println!("[FAT12] write_test write_failed: {}", e);
                        }
                    }
                    // Unconditional: runs regardless of the match/error
                    // outcome above - this file must never end this boot
                    // in anything but its genuine, original state.
                    match fs.write_file("BOOT-S~2", &original) {
                        Ok(()) => {
                            serial_println!("[FAT12] write_test restored=true");
                        }
                        Err(e) => {
                            kprintln!("[FAT12] CRITICAL: failed to restore BOOT-S~2: {}", e);
                            serial_println!("[FAT12] write_test restore_failed: {}", e);
                        }
                    }
                }
                Err(e) => {
                    kprintln!("[FAT12] write test: couldn't read original BOOT-S~2: {}", e);
                    serial_println!("[FAT12] write_test -> no original: {}", e);
                }
            },
            Err(e) => {
                kprintln!("[FAT12] write test: not FAT12 ({})", e);
                serial_println!("[FAT12] write_test -> not fat12: {}", e);
            }
        },
        Err(e) => {
            kprintln!("[FAT12] write test: couldn't find FAT partition: {}", e);
            serial_println!("[FAT12] write_test -> no partition: {}", e);
        }
    }

    // 7b-6. Real FAT12 file CREATION - genuinely new, not an overwrite of
    // something that already existed (Fase 24 covered that case). Built
    // on new free-cluster/free-directory-entry search logic, scoped to
    // files that fit in one cluster with a short (8.3) name. Creates
    // "AGENTOS.TXT" if it doesn't already exist - a repeat boot against
    // the same unregenerated disk image correctly reports "already
    // exists" instead of re-creating it (there's no file-deletion
    // support to reset between repeat boots, and there doesn't need to
    // be: either way, the read-back below should show the same real
    // content). Also lists the root directory again afterward to show
    // the new file genuinely appearing alongside the original 3.
    kprintln!("[KERNEL INIT] Testing real FAT12 file creation...");
    match shell::find_fat_partition() {
        Ok(partition) => match fat12::read_bpb(&partition) {
            Ok(mut fs) => {
                let test_content = b"AgentOS created this file for real.";
                match fs.create_file("AGENTOS.TXT", test_content) {
                    Ok(()) => {
                        kprintln!("[FAT12] created AGENTOS.TXT ({} bytes)", test_content.len());
                        serial_println!("[FAT12] create_test created=true");
                    }
                    Err(e) => {
                        kprintln!("[FAT12] create_file: {}", e);
                        serial_println!("[FAT12] create_test created=false reason={}", e);
                    }
                }
                match fs.read_file("AGENTOS.TXT") {
                    Ok(read_back) => {
                        let matches = read_back == test_content;
                        kprintln!(
                            "[FAT12] AGENTOS.TXT read-back ({} bytes): {}",
                            read_back.len(),
                            if matches {
                                "OK (matches expected content)"
                            } else {
                                "MISMATCH"
                            }
                        );
                        serial_println!(
                            "[FAT12] create_test read_back_len={} match={}",
                            read_back.len(),
                            matches
                        );
                    }
                    Err(e) => {
                        kprintln!("[FAT12] create test: read-back failed: {}", e);
                        serial_println!("[FAT12] create_test read_failed: {}", e);
                    }
                }
            }
            Err(e) => {
                kprintln!("[FAT12] create test: not FAT12 ({})", e);
                serial_println!("[FAT12] create_test -> not fat12: {}", e);
            }
        },
        Err(e) => {
            kprintln!("[FAT12] create test: couldn't find FAT partition: {}", e);
            serial_println!("[FAT12] create_test -> no partition: {}", e);
        }
    }
    shell::dispatch_command("ls");

    // 7b-7. Real FAT12 file DELETION - frees a file's whole cluster chain
    // (each FAT entry set back to 0x000) and marks its directory entry
    // deleted (0xE5). Uses its own dedicated test file ("DELETEME.TXT")
    // rather than touching AGENTOS.TXT above, so this test's cleanup
    // can't interact with that one's already-verified "already exists on
    // repeat boots" behavior. Creates it fresh (ignoring "already
    // exists" - a prior interrupted run may have left it behind),
    // confirms it exists, deletes it, then confirms it's genuinely gone
    // (read_file fails, ls count drops back down) - fully self-cleaning,
    // so this test behaves identically whether the disk was just
    // regenerated or this is a repeat boot against the same image.
    kprintln!("[KERNEL INIT] Testing real FAT12 file deletion...");
    match shell::find_fat_partition() {
        Ok(partition) => match fat12::read_bpb(&partition) {
            Ok(mut fs) => {
                let _ = fs.create_file("DELETEME.TXT", b"temporary file for delete_file test");
                let existed_before = fs.read_file("DELETEME.TXT").is_ok();
                match fs.delete_file("DELETEME.TXT") {
                    Ok(()) => {
                        let gone_after = fs.read_file("DELETEME.TXT").is_err();
                        kprintln!(
                            "[FAT12] delete test: existed_before={} gone_after={}",
                            existed_before,
                            gone_after
                        );
                        serial_println!(
                            "[FAT12] delete_test existed_before={} gone_after={}",
                            existed_before,
                            gone_after
                        );
                    }
                    Err(e) => {
                        kprintln!("[FAT12] delete_file: {}", e);
                        serial_println!("[FAT12] delete_test failed: {}", e);
                    }
                }
            }
            Err(e) => {
                kprintln!("[FAT12] delete test: not FAT12 ({})", e);
                serial_println!("[FAT12] delete_test -> not fat12: {}", e);
            }
        },
        Err(e) => {
            kprintln!("[FAT12] delete test: couldn't find FAT partition: {}", e);
            serial_println!("[FAT12] delete_test -> no partition: {}", e);
        }
    }
    shell::dispatch_command("ls");

    // 7b-8. Real shell-command-level file create/read/delete - proves the
    // *interactive* path (dispatch_command parsing an actual typed line),
    // not just the underlying Fat12Info methods the tests above already
    // exercise directly. Before this, a real person at the AgentOS>
    // prompt had no way to create or delete a file themselves, even
    // though the kernel could do both internally - the same class of gap
    // Shift-key support closed for typing at all. Uses its own dedicated
    // filename, independent of AGENTOS.TXT/DELETEME.TXT above.
    kprintln!("[KERNEL INIT] Testing touch/rm shell commands...");
    shell::dispatch_command("touch SHELLNEW.TXT hello from the real shell");
    shell::dispatch_command("cat SHELLNEW.TXT");
    shell::dispatch_command("rm SHELLNEW.TXT");
    shell::dispatch_command("cat SHELLNEW.TXT");
    shell::dispatch_command("ls");

    // 7b-9. Real FAT12 MULTI-CLUSTER file creation - create_file previously
    // only handled files that fit in a single cluster (1024 bytes on this
    // disk's 2-sector clusters); this proves it now allocates and chains
    // several. Content is a deterministic byte pattern (not a giant string
    // literal) so it can be checked byte-for-byte on read-back, sized to
    // comfortably need multiple clusters on any plausible small-FAT12-
    // volume geometry, not just this exact disk's measured 1024-byte
    // cluster size. Also exercises delete_file's existing cluster-chain
    // walk against a genuine multi-cluster chain for the first time -
    // Fase 27 only ever proved it against single-cluster files.
    kprintln!("[KERNEL INIT] Testing FAT12 multi-cluster file creation...");
    match shell::find_fat_partition() {
        Ok(partition) => match fat12::read_bpb(&partition) {
            Ok(mut fs) => {
                let big_content: alloc::vec::Vec<u8> =
                    (0..3100u32).map(|i| (i % 256) as u8).collect();
                match fs.create_file("BIGFILE.TXT", &big_content) {
                    Ok(()) => match fs.read_file("BIGFILE.TXT") {
                        Ok(read_back) => {
                            let matches = read_back == big_content;
                            kprintln!(
                                "[FAT12] multi-cluster test: read_back_len={} match={}",
                                read_back.len(),
                                matches
                            );
                            serial_println!(
                                "[FAT12] multi_cluster_test created=true read_back_len={} match={}",
                                read_back.len(),
                                matches
                            );
                        }
                        Err(e) => {
                            kprintln!("[FAT12] multi-cluster read_file: {}", e);
                            serial_println!("[FAT12] multi_cluster_test read_failed: {}", e);
                        }
                    },
                    Err(e) => {
                        kprintln!("[FAT12] multi-cluster create_file: {}", e);
                        serial_println!("[FAT12] multi_cluster_test create_failed: {}", e);
                    }
                }
                // Clean up regardless of the outcome above, so this test
                // is self-cleaning across repeat boots against the same
                // unregenerated image - same discipline as Fase 27's
                // delete_file test.
                match fs.delete_file("BIGFILE.TXT") {
                    Ok(()) => {
                        let gone = fs.read_file("BIGFILE.TXT").is_err();
                        kprintln!("[FAT12] multi-cluster cleanup: deleted, gone={}", gone);
                        serial_println!("[FAT12] multi_cluster_test deleted=true gone={}", gone);
                    }
                    Err(e) => {
                        kprintln!("[FAT12] multi-cluster cleanup delete_file: {}", e);
                        serial_println!("[FAT12] multi_cluster_test delete_failed: {}", e);
                    }
                }
            }
            Err(e) => {
                kprintln!("[FAT12] multi-cluster test: not FAT12 ({})", e);
                serial_println!("[FAT12] multi_cluster_test -> not fat12: {}", e);
            }
        },
        Err(e) => {
            kprintln!(
                "[FAT12] multi-cluster test: couldn't find FAT partition: {}",
                e
            );
            serial_println!("[FAT12] multi_cluster_test -> no partition: {}", e);
        }
    }
    shell::dispatch_command("ls");

    // 7b-10. Real FAT12 file RESIZE (grow AND shrink) - write_file
    // previously only supported overwriting a file with exactly the same
    // amount of data; this proves it can now also grow (append new
    // clusters to the chain) and shrink (free trailing clusters) an
    // EXISTING file. Uses its own dedicated file ("RESIZE.TXT"), fully
    // self-cleaning (deletes it at the end) so this test behaves the
    // same whether the disk was just regenerated or this is a repeat
    // boot. Each phase uses a DIFFERENT deterministic pattern (different
    // modulus, different length) specifically so a bug that left stale
    // bytes from a previous phase in place would very likely be caught
    // as a mismatch, not accidentally still look correct.
    kprintln!("[KERNEL INIT] Testing FAT12 file resize (grow + shrink)...");
    match shell::find_fat_partition() {
        Ok(partition) => match fat12::read_bpb(&partition) {
            Ok(mut fs) => {
                let initial: alloc::vec::Vec<u8> = (0..500u32).map(|i| (i % 200) as u8).collect();
                let grown: alloc::vec::Vec<u8> = (0..2500u32).map(|i| (i % 231) as u8).collect();
                let shrunk: alloc::vec::Vec<u8> = (0..300u32).map(|i| (i % 199) as u8).collect();

                let _ = fs.create_file("RESIZE.TXT", &initial);
                let created_ok = fs
                    .read_file("RESIZE.TXT")
                    .map(|d| d == initial)
                    .unwrap_or(false);

                let grow_ok = match fs.write_file("RESIZE.TXT", &grown) {
                    Ok(()) => fs
                        .read_file("RESIZE.TXT")
                        .map(|d| d == grown)
                        .unwrap_or(false),
                    Err(_) => false,
                };

                let shrink_ok = match fs.write_file("RESIZE.TXT", &shrunk) {
                    Ok(()) => fs
                        .read_file("RESIZE.TXT")
                        .map(|d| d == shrunk)
                        .unwrap_or(false),
                    Err(_) => false,
                };

                kprintln!(
                    "[FAT12] resize test: created_ok={} grow_ok={} shrink_ok={}",
                    created_ok,
                    grow_ok,
                    shrink_ok
                );
                serial_println!(
                    "[FAT12] resize_test created_ok={} grow_ok={} shrink_ok={}",
                    created_ok,
                    grow_ok,
                    shrink_ok
                );

                match fs.delete_file("RESIZE.TXT") {
                    Ok(()) => {
                        let gone = fs.read_file("RESIZE.TXT").is_err();
                        kprintln!("[FAT12] resize cleanup: deleted, gone={}", gone);
                        serial_println!("[FAT12] resize_test cleanup deleted=true gone={}", gone);
                    }
                    Err(e) => {
                        kprintln!("[FAT12] resize cleanup delete_file: {}", e);
                        serial_println!("[FAT12] resize_test cleanup delete_failed: {}", e);
                    }
                }
            }
            Err(e) => {
                kprintln!("[FAT12] resize test: not FAT12 ({})", e);
                serial_println!("[FAT12] resize_test -> not fat12: {}", e);
            }
        },
        Err(e) => {
            kprintln!("[FAT12] resize test: couldn't find FAT partition: {}", e);
            serial_println!("[FAT12] resize_test -> no partition: {}", e);
        }
    }
    shell::dispatch_command("ls");

    // 7b-11. Real FAT12 subdirectory creation and listing - mkdir
    // allocates a real cluster holding "." (self) and ".." (parent,
    // cluster 0 meaning "the root") entries, the real FAT convention for
    // a directory - distinct from a plain file's cluster, which just
    // holds opaque data. Deliberately NOT self-cleaning (no rmdir/
    // delete_directory yet - out of scope for this Fase, matching how
    // create_file/delete_file were built as separate steps): TESTDIR
    // persists across repeat boots the same way AGENTOS.TXT does, and
    // mkdir on an existing name fails gracefully ("already exists"),
    // same tolerance create_file already established. The one thing
    // that must ALWAYS hold, fresh boot or repeat, is that TESTDIR's
    // own listing shows EXACTLY "." and ".." and nothing else - proving
    // the rest of its cluster was genuinely zeroed, not left with
    // leftover bytes from some other file this kernel's own self-tests
    // created and deleted earlier in the same boot.
    kprintln!("[KERNEL INIT] Testing FAT12 subdirectory creation...");
    shell::dispatch_command("mkdir TESTDIR");
    shell::dispatch_command("ls");
    shell::dispatch_command("ls TESTDIR");

    // 7b-12. Real FAT12 subdirectory DELETION - frees a directory's
    // cluster and marks its root entry deleted, refusing to delete a
    // non-empty one (checked via its own "."/".." -only content). Uses
    // its own dedicated directory ("RMDTEST"), fully self-cleaning -
    // unlike the TESTDIR test above, which deliberately stays around to
    // keep testing mkdir's "already exists" tolerance across repeat
    // boots (no rmdir existed yet when that test was written).
    kprintln!("[KERNEL INIT] Testing FAT12 subdirectory deletion...");
    shell::dispatch_command("mkdir RMDTEST");
    shell::dispatch_command("rmdir RMDTEST");
    shell::dispatch_command("ls");
    shell::dispatch_command("ls RMDTEST");

    // 7b-14. Real FAT12 file I/O INSIDE a subdirectory - create_file/
    // read_file/write_file/delete_file previously only ever operated on
    // the root directory; the new _in variants (create_file_in/
    // read_file_in/write_file_in/delete_file_in) target an arbitrary
    // subdirectory's cluster instead, sharing the exact same internal
    // logic via a small DirLocation abstraction so this needed zero new
    // low-level disk-I/O code - multi-cluster allocation, resize, and
    // cluster-chain entry-scanning all already existed (Fase 29/30/31).
    // Deliberately NOT exposed as new shell commands yet (that needs
    // path syntax like "SUBTEST/FILE.TXT", a separate concern) - proven
    // here at the Fat12Info API level first, same order Fase 26/28
    // proved create_file before exposing it via touch. Fully self-
    // cleaning: creates its own dedicated directory and removes it at
    // the end, same discipline as Fase 33's RMDTEST.
    kprintln!("[KERNEL INIT] Testing FAT12 file I/O inside a subdirectory...");
    match shell::find_fat_partition() {
        Ok(partition) => match fat12::read_bpb(&partition) {
            Ok(mut fs) => {
                let _ = fs.create_directory("SUBTEST");
                let dir_cluster = fs
                    .list_root_directory()
                    .ok()
                    .and_then(|entries| entries.into_iter().find(|e| e.name == "SUBTEST"))
                    .map(|e| e.start_cluster);

                match dir_cluster {
                    Some(cluster) => {
                        let initial = b"hello from inside a subdirectory";
                        let grown: alloc::vec::Vec<u8> =
                            (0..1500u32).map(|i| (i % 211) as u8).collect();

                        let _ = fs.create_file_in(cluster, "INSIDE.TXT", initial);
                        let created_ok = fs
                            .read_file_in(cluster, "INSIDE.TXT")
                            .map(|d| d == initial)
                            .unwrap_or(false);

                        let write_ok = match fs.write_file_in(cluster, "INSIDE.TXT", &grown) {
                            Ok(()) => fs
                                .read_file_in(cluster, "INSIDE.TXT")
                                .map(|d| d == grown)
                                .unwrap_or(false),
                            Err(_) => false,
                        };

                        let delete_ok = match fs.delete_file_in(cluster, "INSIDE.TXT") {
                            Ok(()) => fs.read_file_in(cluster, "INSIDE.TXT").is_err(),
                            Err(_) => false,
                        };

                        let cleanup_ok = fs.delete_directory("SUBTEST").is_ok();

                        kprintln!(
                            "[FAT12] subdir file I/O test: created_ok={} write_ok={} delete_ok={} cleanup_ok={}",
                            created_ok,
                            write_ok,
                            delete_ok,
                            cleanup_ok
                        );
                        serial_println!(
                            "[FAT12] subdir_file_test created_ok={} write_ok={} delete_ok={} cleanup_ok={}",
                            created_ok,
                            write_ok,
                            delete_ok,
                            cleanup_ok
                        );
                    }
                    None => {
                        kprintln!("[FAT12] subdir file I/O test: couldn't find SUBTEST's cluster");
                        serial_println!("[FAT12] subdir_file_test -> no dir cluster");
                    }
                }
            }
            Err(e) => {
                kprintln!("[FAT12] subdir file I/O test: not FAT12 ({})", e);
                serial_println!("[FAT12] subdir_file_test -> not fat12: {}", e);
            }
        },
        Err(e) => {
            kprintln!(
                "[FAT12] subdir file I/O test: couldn't find FAT partition: {}",
                e
            );
            serial_println!("[FAT12] subdir_file_test -> no partition: {}", e);
        }
    }
    shell::dispatch_command("ls");

    // 7b-15. Real shell-level file I/O via a DIR/FILE path - touch/cat/rm
    // now understand "DIRNAME/FILENAME.EXT" (one level deep), reaching a
    // subdirectory's file through the REAL dispatch_command parsing path
    // (split_path + resolve_dir_cluster in shell.rs), not by calling
    // Fat12Info's *_in methods directly the way the test above does.
    // Fully self-cleaning: creates its own dedicated directory and
    // removes it afterward.
    kprintln!("[KERNEL INIT] Testing shell file I/O via a DIR/FILE path...");
    shell::dispatch_command("mkdir PATHTEST");
    shell::dispatch_command("touch PATHTEST/INSIDE.TXT hello via a real path");
    shell::dispatch_command("cat PATHTEST/INSIDE.TXT");
    shell::dispatch_command("rm PATHTEST/INSIDE.TXT");
    shell::dispatch_command("cat PATHTEST/INSIDE.TXT");
    shell::dispatch_command("rmdir PATHTEST");
    shell::dispatch_command("ls");

    // 7b-16. Real NESTED FAT12 subdirectories - create_directory/
    // delete_directory now accept a parent directory location (root OR
    // another subdirectory's cluster), the exact same DirLocation
    // pattern Fase 34 used for file I/O. The one thing genuinely
    // different for a nested directory vs. a root-level one: its ".."
    // entry must point to the PARENT's real cluster, not always 0 (0 is
    // specifically the "parent is root" convention) - this test's real
    // correctness check is confirming that value, not just that
    // creation "succeeded". Not yet wired to shell path syntax (that
    // needs multi-segment path parsing, a separate concern) - proven at
    // the Fat12Info API level first, same order as Fase 34.
    kprintln!("[KERNEL INIT] Testing nested FAT12 subdirectories...");
    match shell::find_fat_partition() {
        Ok(partition) => match fat12::read_bpb(&partition) {
            Ok(mut fs) => {
                let _ = fs.create_directory("NESTTEST");
                let outer_cluster = fs
                    .list_root_directory()
                    .ok()
                    .and_then(|entries| entries.into_iter().find(|e| e.name == "NESTTEST"))
                    .map(|e| e.start_cluster);

                match outer_cluster {
                    Some(outer) => {
                        let _ = fs.create_directory_in(outer, "INNER");
                        let outer_entries = fs.list_directory(outer).unwrap_or_default();
                        let inner_cluster = outer_entries
                            .iter()
                            .find(|e| e.name == "INNER")
                            .map(|e| e.start_cluster);
                        // "." + ".." + "INNER" - proves creation didn't
                        // just succeed but also didn't leave stale or
                        // duplicate entries in the parent.
                        let outer_count_ok = outer_entries.len() == 3;

                        let dotdot_ok = match inner_cluster {
                            Some(inner) => fs
                                .list_directory(inner)
                                .ok()
                                .and_then(|entries| entries.into_iter().find(|e| e.name == ".."))
                                .map(|e| e.start_cluster == outer)
                                .unwrap_or(false),
                            None => false,
                        };

                        let delete_inner_ok = fs.delete_directory_in(outer, "INNER").is_ok();
                        let cleanup_ok = fs.delete_directory("NESTTEST").is_ok();

                        kprintln!(
                            "[FAT12] nested dir test: outer_count_ok={} dotdot_ok={} delete_inner_ok={} cleanup_ok={}",
                            outer_count_ok,
                            dotdot_ok,
                            delete_inner_ok,
                            cleanup_ok
                        );
                        serial_println!(
                            "[FAT12] nested_dir_test outer_count_ok={} dotdot_ok={} delete_inner_ok={} cleanup_ok={}",
                            outer_count_ok,
                            dotdot_ok,
                            delete_inner_ok,
                            cleanup_ok
                        );
                    }
                    None => {
                        kprintln!("[FAT12] nested dir test: couldn't find NESTTEST's cluster");
                        serial_println!("[FAT12] nested_dir_test -> no outer cluster");
                    }
                }
            }
            Err(e) => {
                kprintln!("[FAT12] nested dir test: not FAT12 ({})", e);
                serial_println!("[FAT12] nested_dir_test -> not fat12: {}", e);
            }
        },
        Err(e) => {
            kprintln!(
                "[FAT12] nested dir test: couldn't find FAT partition: {}",
                e
            );
            serial_println!("[FAT12] nested_dir_test -> no partition: {}", e);
        }
    }
    shell::dispatch_command("ls");

    // 7b-17. Real FAT12 subdirectory GROWTH - create_file_in/
    // create_directory_in previously failed once a subdirectory's single
    // cluster ran out of free entry slots; find_free_entry_in now
    // allocates and chains a fresh cluster onto the directory
    // automatically when that happens, reusing the exact same
    // allocate-then-chain primitives create_file/write_file already use
    // for a file's own chain. This disk's clusters hold 32 entries each
    // (1024 bytes / 32), 2 of which are always "."/"..", so the 31st
    // file created inside a fresh subdirectory is genuinely the first
    // one that requires a second cluster to exist at all.
    kprintln!("[KERNEL INIT] Testing FAT12 subdirectory growth (multi-cluster directories)...");
    match shell::find_fat_partition() {
        Ok(partition) => match fat12::read_bpb(&partition) {
            Ok(mut fs) => {
                let _ = fs.create_directory("GROWTEST");
                let dir_cluster = fs
                    .list_root_directory()
                    .ok()
                    .and_then(|entries| entries.into_iter().find(|e| e.name == "GROWTEST"))
                    .map(|e| e.start_cluster);

                match dir_cluster {
                    Some(dir) => {
                        let mut all_created = true;
                        for i in 0..31u32 {
                            let name = alloc::format!("F{}.TXT", i);
                            if fs.create_file_in(dir, &name, &[i as u8]).is_err() {
                                all_created = false;
                            }
                        }

                        let entries = fs.list_directory(dir).unwrap_or_default();
                        // "." + ".." + 31 files = 33 - proves every
                        // create genuinely landed, none silently lost
                        // when the chain grew mid-loop.
                        let count_ok = entries.len() == 33;

                        // The 31st file (index 30) is the one that could
                        // only exist if a second cluster was genuinely
                        // allocated - verified by actually reading its
                        // content back, not just checking it "exists".
                        let last_name = alloc::format!("F{}.TXT", 30u32);
                        let last_ok = fs
                            .read_file_in(dir, &last_name)
                            .map(|d| d == [30u8])
                            .unwrap_or(false);

                        let mut all_deleted = true;
                        for i in 0..31u32 {
                            let name = alloc::format!("F{}.TXT", i);
                            if fs.delete_file_in(dir, &name).is_err() {
                                all_deleted = false;
                            }
                        }
                        let cleanup_ok = fs.delete_directory("GROWTEST").is_ok();

                        kprintln!(
                            "[FAT12] dir growth test: all_created={} count_ok={} last_ok={} all_deleted={} cleanup_ok={}",
                            all_created,
                            count_ok,
                            last_ok,
                            all_deleted,
                            cleanup_ok
                        );
                        serial_println!(
                            "[FAT12] dir_growth_test all_created={} count_ok={} last_ok={} all_deleted={} cleanup_ok={}",
                            all_created,
                            count_ok,
                            last_ok,
                            all_deleted,
                            cleanup_ok
                        );
                    }
                    None => {
                        kprintln!("[FAT12] dir growth test: couldn't find GROWTEST's cluster");
                        serial_println!("[FAT12] dir_growth_test -> no dir cluster");
                    }
                }
            }
            Err(e) => {
                kprintln!("[FAT12] dir growth test: not FAT12 ({})", e);
                serial_println!("[FAT12] dir_growth_test -> not fat12: {}", e);
            }
        },
        Err(e) => {
            kprintln!(
                "[FAT12] dir growth test: couldn't find FAT partition: {}",
                e
            );
            serial_println!("[FAT12] dir_growth_test -> no partition: {}", e);
        }
    }

    // 7b-2. Test VFAT Long File Names (create, display, lookup by EITHER
    // name, delete) "long name.txt" is exactly 13 characters - this
    // Fase's one-chunk limit, deliberately tested right at the boundary
    // rather than comfortably under it. Doesn't fit 8.3 (a space, and 9
    // base characters before the extension), so create_file falls back
    // to build_name_entries' VFAT path: a generated short alias
    // ("LONGN~1.TXT") plus one real long-name entry. long_name_shown
    // proves parse_dir_sector reconstructs it correctly (checksum
    // verified) for `ls`; long_name_read_ok and alias_read_ok both being
    // true proves find_entry_location_in genuinely matches by either
    // name, not just whichever one `ls` happens to display -
    // read_file/delete_file/write_file/create_file's own "already
    // exists" check all share that one matching path, so this exercises
    // all of them at once. alias_collision_rejected is a direct
    // regression guard for a real bug this test caught on its first
    // run: a plain create using the exact bytes of an already-taken
    // alias used to succeed instead of being rejected, silently leaving
    // two entries sharing one short name (fixed in build_name_entries -
    // see its doc for the full story). Repeated for a directory name
    // ("My Documents" -> "MYDOC~1", deleted by its *long* name this time
    // rather than the alias, covering both lookup directions across the
    // two sub-tests) to prove create_directory_impl/delete_directory_impl
    // share the identical wiring, not just the file functions.
    kprintln!("[KERNEL INIT] Testing VFAT long file names (create/display/lookup/delete)...");
    match shell::find_fat_partition() {
        Ok(partition) => match fat12::read_bpb(&partition) {
            Ok(mut fs) => {
                const LFN_DATA: &[u8] = b"Real VFAT long name support!";
                let file_created = fs.create_file("long name.txt", LFN_DATA).is_ok();
                let entries = fs.list_root_directory().unwrap_or_default();
                let long_name_shown = entries.iter().any(|e| e.name == "long name.txt");
                let long_name_read_ok = fs
                    .read_file("long name.txt")
                    .map(|d| d == LFN_DATA)
                    .unwrap_or(false);
                let alias_read_ok = fs
                    .read_file("LONGN~1.TXT")
                    .map(|d| d == LFN_DATA)
                    .unwrap_or(false);
                let alias_collision_rejected = fs.create_file("LONGN~1.TXT", b"other").is_err();
                let file_delete_ok = fs.delete_file("LONGN~1.TXT").is_ok();
                let file_gone_ok =
                    fs.read_file("long name.txt").is_err() && fs.read_file("LONGN~1.TXT").is_err();

                let dir_created = fs.create_directory("My Documents").is_ok();
                let entries = fs.list_root_directory().unwrap_or_default();
                let dir_long_name_shown =
                    entries.iter().any(|e| e.name == "My Documents" && e.is_dir);
                let dir_delete_ok = fs.delete_directory("My Documents").is_ok();
                let dir_gone_ok = !fs
                    .list_root_directory()
                    .unwrap_or_default()
                    .iter()
                    .any(|e| e.name == "My Documents" || e.name == "MYDOC~1");

                kprintln!(
                    "[FAT12] vfat test: file_created={} long_name_shown={} long_name_read_ok={} alias_read_ok={} alias_collision_rejected={} file_delete_ok={} file_gone_ok={} dir_created={} dir_long_name_shown={} dir_delete_ok={} dir_gone_ok={}",
                    file_created, long_name_shown, long_name_read_ok, alias_read_ok, alias_collision_rejected,
                    file_delete_ok, file_gone_ok, dir_created, dir_long_name_shown, dir_delete_ok, dir_gone_ok
                );
                serial_println!(
                    "[FAT12] vfat_test file_created={} long_name_shown={} long_name_read_ok={} alias_read_ok={} alias_collision_rejected={} file_delete_ok={} file_gone_ok={} dir_created={} dir_long_name_shown={} dir_delete_ok={} dir_gone_ok={}",
                    file_created, long_name_shown, long_name_read_ok, alias_read_ok, alias_collision_rejected,
                    file_delete_ok, file_gone_ok, dir_created, dir_long_name_shown, dir_delete_ok, dir_gone_ok
                );
            }
            Err(e) => {
                kprintln!("[FAT12] vfat test: not FAT12 ({})", e);
                serial_println!("[FAT12] vfat_test -> not fat12: {}", e);
            }
        },
        Err(e) => {
            kprintln!("[FAT12] vfat test: couldn't find FAT partition: {}", e);
            serial_println!("[FAT12] vfat_test -> no partition: {}", e);
        }
    }
    shell::dispatch_command("ls");

    // 7b-3. Test Multi-Segment Shell Paths ("A/B/FILE", any number of
    // levels deep) - through the REAL dispatch_command parsing path, not
    // by calling Fat12Info's own already-nesting-capable _in methods
    // directly (Fase 36 proved those work; this proves the *shell* can
    // finally reach them). Before this Fase, shell.rs's own split_path
    // hard-rejected a second '/' and mkdir/rmdir/ls never split a path at
    // all, so a person at the prompt had no way to create, populate, or
    // even list a nested directory - only Fase 36's internal self-test
    // could. mkdir MULTIOUT/MULTIIN is the key new capability: creating
    // a directory *inside* another one by name, from the shell, for the
    // first time. The final `cat` correctly asserts the subdirectory-
    // specific "not found in directory" message (not "in root
    // directory") - proof the path genuinely routed to the nested
    // location two levels down, not a coincidental pass.
    kprintln!("[KERNEL INIT] Testing multi-segment shell paths (A/B/FILE)...");
    shell::dispatch_command("mkdir MULTIOUT");
    shell::dispatch_command("mkdir MULTIOUT/MULTIIN");
    shell::dispatch_command("touch MULTIOUT/MULTIIN/DEEP.TXT nested two levels deep");
    shell::dispatch_command("cat MULTIOUT/MULTIIN/DEEP.TXT");
    shell::dispatch_command("ls MULTIOUT/MULTIIN");
    shell::dispatch_command("rm MULTIOUT/MULTIIN/DEEP.TXT");
    shell::dispatch_command("cat MULTIOUT/MULTIIN/DEEP.TXT");
    shell::dispatch_command("rmdir MULTIOUT/MULTIIN");
    shell::dispatch_command("rmdir MULTIOUT");
    shell::dispatch_command("ls");

    // 7b-4. Test Multi-Chunk VFAT Long File Names (>13 characters, needing
    // several chained long-name entries instead of the one Fase 38 could
    // build). "a very long descriptive name.txt" is 32 characters - past
    // the old 13-character cap, needing ceil(32/13)=3 long entries
    // (13+13+6 characters). Exercises the actual new logic this Fase adds
    // (build_lfn_entries building N entries in correct reverse-sequence
    // order, find_free_entry_run_in reserving N+1 consecutive slots)
    // while relying on machinery already proven in Fase 38: the read
    // side's LfnState was always general over chunk count, so reading
    // this 3-chunk name back correctly is also the first real proof that
    // generality was ever exercised, not just designed for.
    // multi_chunk_long_read_ok proves reading by the real long name
    // reconstructs all 3 chunks in the right order (a scrambled order
    // would produce a readable-but-wrong string, not an error - so this
    // compares full content, not just success/failure).
    // multi_chunk_alias_read_ok proves the generated short alias
    // ("AVERY~1.TXT") is independently valid too, same as the single-
    // chunk case. Deleted by the long name specifically (Fase 38's file
    // sub-test used the alias) to exercise find_entry_location_in's own
    // multi-chunk reconstruction, not just parse_dir_sector's.
    kprintln!("[KERNEL INIT] Testing multi-chunk VFAT long file names (>13 chars)...");
    match shell::find_fat_partition() {
        Ok(partition) => match fat12::read_bpb(&partition) {
            Ok(mut fs) => {
                const LONG_NAME: &str = "a very long descriptive name.txt";
                const MULTI_LFN_DATA: &[u8] = b"multi-chunk VFAT works!";
                let multi_chunk_created = fs.create_file(LONG_NAME, MULTI_LFN_DATA).is_ok();
                let entries = fs.list_root_directory().unwrap_or_default();
                let multi_chunk_shown = entries.iter().any(|e| e.name == LONG_NAME);
                let multi_chunk_long_read_ok = fs
                    .read_file(LONG_NAME)
                    .map(|d| d == MULTI_LFN_DATA)
                    .unwrap_or(false);
                let multi_chunk_alias_read_ok = fs
                    .read_file("AVERY~1.TXT")
                    .map(|d| d == MULTI_LFN_DATA)
                    .unwrap_or(false);
                let multi_chunk_delete_ok = fs.delete_file(LONG_NAME).is_ok();
                let multi_chunk_gone_ok =
                    fs.read_file(LONG_NAME).is_err() && fs.read_file("AVERY~1.TXT").is_err();

                kprintln!(
                    "[FAT12] multi-chunk vfat test: created={} shown={} long_read_ok={} alias_read_ok={} delete_ok={} gone_ok={}",
                    multi_chunk_created, multi_chunk_shown, multi_chunk_long_read_ok,
                    multi_chunk_alias_read_ok, multi_chunk_delete_ok, multi_chunk_gone_ok
                );
                serial_println!(
                    "[FAT12] multi_chunk_vfat_test created={} shown={} long_read_ok={} alias_read_ok={} delete_ok={} gone_ok={}",
                    multi_chunk_created, multi_chunk_shown, multi_chunk_long_read_ok,
                    multi_chunk_alias_read_ok, multi_chunk_delete_ok, multi_chunk_gone_ok
                );
            }
            Err(e) => {
                kprintln!("[FAT12] multi-chunk vfat test: not FAT12 ({})", e);
                serial_println!("[FAT12] multi_chunk_vfat_test -> not fat12: {}", e);
            }
        },
        Err(e) => {
            kprintln!(
                "[FAT12] multi-chunk vfat test: couldn't find FAT partition: {}",
                e
            );
            serial_println!("[FAT12] multi_chunk_vfat_test -> no partition: {}", e);
        }
    }
    shell::dispatch_command("ls");

    // 7c. Test Backspace/Line-Editing by feeding a realistic PS/2 make+break
    // byte sequence through the real handle_scancode() - typing "pss" then
    // one backspace should leave "ps" (verified: this dispatches the real
    // `ps` process listing below, not an "Unknown command: 'pss'").
    kprintln!("[KERNEL INIT] Testing keyboard backspace (typing 'pss' + backspace -> 'ps')...");
    const BACKSPACE_TEST_SEQUENCE: &[u8] = &[
        0x19, 0x99, // p down, up
        0x1F, 0x9F, // s down, up
        0x1F, 0x9F, // s down, up (repeated key - needs the break code above first)
        0x0E, 0x8E, // backspace down, up
        0x1C, 0x9C, // enter down, up
    ];
    for &code in BACKSPACE_TEST_SEQUENCE {
        keyboard::handle_scancode(code);
    }

    // 7c-2. Test Shell Command History (Up/Down) via the real handle_scancode
    // sequence: "xx"+Enter, "yy"+Enter, Up, Up, Enter. Two Up presses from a
    // fresh line should land on the OLDER of the two ("xx"), not "yy" - so
    // the final Enter should dispatch "xx" again, meaning "Unknown command:
    // 'xx'" appears twice in the log total and "...'yy'" only once.
    kprintln!("[KERNEL INIT] Testing shell history (xx, yy, Up, Up -> recall 'xx')...");
    const HISTORY_TEST_SEQUENCE: &[u8] = &[
        0x2D, 0xAD, 0x2D, 0xAD, // "xx"
        0x1C, 0x9C, // enter -> dispatch "xx"
        0x15, 0x95, 0x15, 0x95, // "yy"
        0x1C, 0x9C, // enter -> dispatch "yy"
        0xE0, 0x48, 0xE0, 0xC8, // Up (press+release) -> recall "yy"
        0xE0, 0x48, 0xE0, 0xC8, // Up (press+release) -> recall "xx"
        0x1C, 0x9C, // enter -> dispatch recalled "xx"
    ];
    for &code in HISTORY_TEST_SEQUENCE {
        keyboard::handle_scancode(code);
    }

    // 7c-3. Test Left/Right Cursor Movement within a line via the real
    // handle_scancode sequence: type "hep" (a typo missing the 'l'), press
    // Left once (cursor moves from after 'p' to between 'e' and 'p'), type
    // 'l', Enter. If the cursor genuinely moved and the insert landed at
    // that position rather than at the end, the buffer is "help" - a real
    // shell command, so success shows up as the actual help text below,
    // not "Unknown command: 'hep'" or "...'hepl'".
    kprintln!("[KERNEL INIT] Testing cursor movement (type 'hep', Left, 'l' -> 'help')...");
    const CURSOR_TEST_SEQUENCE: &[u8] = &[
        0x23, 0xA3, // h down, up
        0x12, 0x92, // e down, up
        0x19, 0x99, // p down, up
        0xE0, 0x4B, 0xE0, 0xCB, // Left (press+release)
        0x26, 0xA6, // l down, up -> inserted between 'e' and 'p'
        0x1C, 0x9C, // enter -> dispatch "help"
    ];
    for &code in CURSOR_TEST_SEQUENCE {
        keyboard::handle_scancode(code);
    }

    // 7c-4. Test Shift key support (uppercase letters + `~`/`-`) - a real
    // practical gap this closes: before this, a real user at the keyboard
    // could never actually type `cat KERNEL~1` themselves (no way to
    // produce an uppercase letter or `~` at all), even though the
    // self-test above dispatches it directly. Feeds a realistic sequence
    // holding Left Shift across "KERNEL~" (six letters plus the backtick
    // key, which shifts to `~`), releasing it before the unshifted "1",
    // spelling the exact real filename found on disk. If Shift genuinely
    // works, this dispatches the real `cat` command a *second* time (the
    // first was the direct self-test above) - "cat KERNEL~1 -> ... bytes"
    // should appear twice in the log, not once plus an "Unknown command".
    kprintln!("[KERNEL INIT] Testing Shift key (typing 'cat KERNEL~1' via keyboard)...");
    const SHIFT_TEST_SEQUENCE: &[u8] = &[
        0x2E, 0xAE, // c
        0x1E, 0x9E, // a
        0x14, 0x94, // t
        0x39, 0xB9, // space
        0x2A, // Left Shift down (held through "KERNEL~")
        0x25, 0xA5, // K
        0x12, 0x92, // E
        0x13, 0x93, // R
        0x31, 0xB1, // N
        0x12, 0x92, // E
        0x26, 0xA6, // L
        0x29, 0xA9, // ~ (backtick key, shifted)
        0xAA, // Left Shift up
        0x02, 0x82, // 1 (unshifted)
        0x1C, 0x9C, // enter -> dispatch "cat KERNEL~1"
    ];
    for &code in SHIFT_TEST_SEQUENCE {
        keyboard::handle_scancode(code);
    }

    // 7d. Test a Real Cooperative Context Switch (stack + register swap)
    // Cooperative only - the worker yields back voluntarily. Not wired to
    // the timer interrupt yet (that's real preemption, separate work).
    scheduler::context_switch::run_demo();

    // 7e. Test a Real N-Way Priority Cooperative Scheduler
    // Generalizes 7d from one hardcoded worker to multiple tasks picked by
    // priority - still cooperative, not timer-driven. Also proves the
    // ps-visible PCB table is now really unified with this scheduler: the
    // demo itself calls `ps` from inside the "alpha" task to show live
    // RUNNING/READY state, and this second call afterward should show
    // task-alpha/bravo/charlie as real PCB entries in TERMINATED state
    // with distinct, real (nonzero) stack pointers - not the fixed
    // boot-time-only table `ps` used to be limited to.
    scheduler::context_switch::run_cooperative_demo();
    shell::dispatch_command("ps");

    // 7f. Test Real Timer-Driven Preemption
    // Unlike 7d/7e, these two tasks never call yield_now at all - the only
    // reason either one ever stops running is the timer IRQ forcing it.
    // Also proves ps-visible PCB unification reaches the preemptive
    // scheduler too now (via try_lock from tick(), see preemptive.rs's
    // module doc): the demo calls `ps` from inside task-preempt-0's first
    // entry to show live RUNNING/READY state, and this call afterward
    // should show both task-preempt-0/1 as real PCB entries in TERMINATED
    // state with distinct, real stack pointers.
    scheduler::preemptive::run_preemptive_demo();
    shell::dispatch_command("ps");

    kprintln!("==================================================");
    kprintln!("  [SUCCESS] AgentOS Native Kernel Boot Sequence Complete ");
    kprintln!("  [SHELL] AgentOS Native Console Ready. Type commands: ");
    kprintln!("==================================================");
    kprint!("AgentOS> ");

    // 8. Idle Loop - the shell prompt above is now serviced entirely by the
    // IRQ1 keyboard handler (see interrupts.rs -> keyboard::handle_scancode).
    // `hlt` parks the CPU until the next interrupt (keyboard, timer, ...)
    // instead of burning cycles polling port 0x60 every iteration.
    loop {
        x86_64::instructions::hlt();
    }
}

/// Panic Handler for `#![no_std]` Bare-Metal
#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    kprintln!("\n[KERNEL PANIC] {}", info);
    serial_println!("\n[KERNEL PANIC] {}", info);
    loop {
        x86_64::instructions::hlt();
    }
}
