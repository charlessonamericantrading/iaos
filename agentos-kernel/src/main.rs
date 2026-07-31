#![no_std]
#![no_main]
#![allow(dead_code)]
#![feature(abi_x86_interrupt)]

extern crate alloc;

use core::panic::PanicInfo;

mod ata;
mod gdt;
mod gguf_loader;
mod interrupts;
mod keyboard;
mod memory;
mod net;
mod pci;
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
    shell::dispatch_command("lspci");
    shell::dispatch_command("disk");

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

    // 7d. Test a Real Cooperative Context Switch (stack + register swap)
    // Cooperative only - the worker yields back voluntarily. Not wired to
    // the timer interrupt yet (that's real preemption, separate work).
    scheduler::context_switch::run_demo();

    // 7e. Test a Real N-Way Priority Cooperative Scheduler
    // Generalizes 7d from one hardcoded worker to multiple tasks picked by
    // priority - still cooperative, not timer-driven.
    scheduler::context_switch::run_cooperative_demo();

    // 7f. Test Real Timer-Driven Preemption
    // Unlike 7d/7e, these two tasks never call yield_now at all - the only
    // reason either one ever stops running is the timer IRQ forcing it.
    scheduler::preemptive::run_preemptive_demo();

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
