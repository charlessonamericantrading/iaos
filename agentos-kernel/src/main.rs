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
use gguf_loader::GgufModelLoader;
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

    // 5. Test GGUF Quantized Model Parser & Tensor Matrix Execution
    kprintln!("[GGUF INFERENCE] Testing Native GGUF Header Parser & Tensor Weights...");
    let sample_gguf_bytes: [u8; 24] = [
        0x47, 0x47, 0x55, 0x46, // "GGUF"
        0x03, 0x00, 0x00, 0x00, // Version 3
        0x10, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // 16 Tensors
        0x08, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // 8 KV Pairs
    ];

    if let Ok(loader) = GgufModelLoader::parse_header(&sample_gguf_bytes) {
        let weights: [f32; 16] = [
            0.5, -0.2, 0.8, 0.1, 0.3, 0.9, -0.4, 0.6, -0.1, 0.4, 0.7, 0.2, 0.6, -0.5, 0.2, 0.8,
        ];
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

    // 7b-3. Real e1000 TX attempt: build a real transmit descriptor ring
    // (using fresh physical frames from the global allocator - Fase 21)
    // and hand a real ARP request to the hardware. Genuinely proven: the
    // ring's physical address and descriptor content are correct, and the
    // hardware really dequeues the descriptor (TDH advances). NOT yet
    // resolved: the descriptor's Descriptor-Done status bit never gets
    // written back in local testing, despite several independently ruled-
    // out hypotheses - see net/e1000.rs's module doc for the full
    // investigation. This currently always returns Err; kept and reported
    // honestly as real partial progress, not reverted.
    match net::e1000::send_test_frame() {
        Ok(()) => {}
        Err(e) => {
            kprintln!("[E1000] send_test_frame: {}", e);
            serial_println!("[E1000] tx_sent -> not yet confirmed: {}", e);
        }
    }

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
            Ok(fs) => match fs.read_file("BOOT-S~2") {
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
            Ok(fs) => {
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
