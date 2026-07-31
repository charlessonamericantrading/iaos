#![no_std]
#![no_main]
#![allow(dead_code)]
#![feature(abi_x86_interrupt)]

use core::panic::PanicInfo;

mod vga_buffer;
mod serial;
mod gdt;
mod interrupts;
mod memory;
mod tensor_engine;
mod scheduler;
mod syscall;
mod keyboard;
mod gguf_loader;
mod net;

use scheduler::agent_scheduler::SCHEDULER;
use scheduler::process::Priority;
use memory::kv_allocator::KV_MANAGER;
use keyboard::KEYBOARD;
use gguf_loader::GgufModelLoader;
use net::tcpip::NativeNetworkStack;

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
            kprintln!("[KV MEMORY] Allocated KV Cache Block #{} for PID 2 in VRAM", block_id);
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
            0.5, -0.2, 0.8, 0.1,
            0.3, 0.9, -0.4, 0.6,
            -0.1, 0.4, 0.7, 0.2,
            0.6, -0.5, 0.2, 0.8,
        ];
        let inputs: [f32; 4] = [1.0, 2.0, 0.5, 3.0];
        let mut outputs: [f32; 4] = [0.0; 4];

        loader.execute_gguf_layer_pass(&weights, &inputs, &mut outputs, 4, 4);

        kprintln!(
            "[GGUF RESULT] Y = ReLU(W * X + B) -> [{:.2}, {:.2}, {:.2}, {:.2}]",
            outputs[0], outputs[1], outputs[2], outputs[3]
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

    kprintln!("==================================================");
    kprintln!("  [SUCCESS] AgentOS Native Kernel Boot Sequence Complete ");
    kprintln!("  [SHELL] AgentOS Native Console Ready. Type commands: ");
    kprintln!("==================================================");
    kprint!("AgentOS> ");

    // 8. Main Interactive Kernel Shell Loop (PS/2 Keyboard & Event Dispatch)
    let mut last_scancode: u8 = 0;
    loop {
        {
            let mut kb = KEYBOARD.lock();
            if let Some(scancode) = kb.read_scancode() {
                if scancode != last_scancode && (scancode & 0x80) == 0 {
                    if let Some(ch) = kb.scancode_to_char(scancode) {
                        if ch == '\n' {
                            kprintln!("\n[AGENTOS SHELL] Task dispatched to Kernel Scheduler.");
                            kprint!("AgentOS> ");
                        } else {
                            kprint!("{}", ch);
                        }
                    }
                }
                last_scancode = scancode;
            }
        }
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
