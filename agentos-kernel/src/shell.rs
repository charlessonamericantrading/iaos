use crate::memory::heap;
use crate::memory::kv_allocator::KV_MANAGER;
use crate::scheduler::agent_scheduler::SCHEDULER;
use crate::scheduler::process::{Priority, ProcessState};
use crate::{kprintln, serial_println};

/// Parses and runs one line typed at the `AgentOS>` prompt.
///
/// Kept separate from `keyboard.rs` so it can also be called directly from
/// a boot self-test (see main.rs) without needing a real keypress - the
/// IRQ1 handler and the self-test both go through this same function.
pub fn dispatch_command(line: &str) {
    let line = line.trim();
    if line.is_empty() {
        return;
    }

    let mut parts = line.split_whitespace();
    let cmd = parts.next().unwrap_or("");
    match cmd {
        "help" => {
            kprintln!("Commands: help, ps, mem, uptime, lspci, clear");
            serial_println!("[SHELL] help -> Commands: help, ps, mem, uptime, lspci, clear");
        }
        "ps" => {
            kprintln!("PID  PRIO   STATE    NAME");
            serial_println!("[SHELL] ps ->");
            SCHEDULER.lock().for_each_process(|p| {
                kprintln!(
                    "{:<4} {:<6} {:<8} {}",
                    p.pid,
                    priority_label(p.priority),
                    state_label(p.state),
                    p.name
                );
                serial_println!(
                    "  PID {} [{}] {} {}",
                    p.pid,
                    priority_label(p.priority),
                    state_label(p.state),
                    p.name
                );
            });
        }
        "mem" => {
            let manager = KV_MANAGER.lock();
            kprintln!(
                "Heap: {:#x}..{:#x} ({} KiB) | KV cache blocks allocated: {}",
                heap::HEAP_START,
                heap::HEAP_START + heap::HEAP_SIZE,
                heap::HEAP_SIZE / 1024,
                manager.get_allocated_count()
            );
            serial_println!(
                "[SHELL] mem -> heap {:#x}..{:#x} ({} KiB), kv_blocks={}",
                heap::HEAP_START,
                heap::HEAP_START + heap::HEAP_SIZE,
                heap::HEAP_SIZE / 1024,
                manager.get_allocated_count()
            );
            manager.for_each_block(|b| {
                kprintln!(
                    "  KV block #{} pid={} {:?} {}B @ {:#x}",
                    b.block_id,
                    b.pid,
                    b.location,
                    b.size_bytes(),
                    b.addr()
                );
                serial_println!(
                    "  KV block #{} pid={} {:?} {}B @ {:#x}",
                    b.block_id,
                    b.pid,
                    b.location,
                    b.size_bytes(),
                    b.addr()
                );
            });
        }
        "uptime" => {
            let ticks = crate::interrupts::timer_ticks();
            kprintln!(
                "Timer ticks since boot: {} (~{:.1}s at ~18.2Hz)",
                ticks,
                ticks as f64 / 18.2
            );
            serial_println!("[SHELL] uptime -> {} ticks", ticks);
        }
        "lspci" => {
            let devices = crate::pci::scan_bus0();
            kprintln!("Bus Dev Fn  Vendor Device Class");
            serial_println!("[SHELL] lspci -> {} device(s) on bus 0", devices.len());
            for d in &devices {
                kprintln!(
                    "{:3} {:3} {:2}  {:#06x} {:#06x} {:#04x}:{:#04x} ({})",
                    d.bus,
                    d.device,
                    d.function,
                    d.vendor_id,
                    d.device_id,
                    d.class,
                    d.subclass,
                    crate::pci::class_name(d.class)
                );
                serial_println!(
                    "  {:02x}:{:02x}.{} vendor={:#06x} device={:#06x} class={:#04x}:{:#04x} prog_if={:#04x} ({})",
                    d.bus,
                    d.device,
                    d.function,
                    d.vendor_id,
                    d.device_id,
                    d.class,
                    d.subclass,
                    d.prog_if,
                    crate::pci::class_name(d.class)
                );
            }
        }
        "clear" => {
            crate::vga_buffer::clear_screen();
            serial_println!("[SHELL] clear -> VGA screen cleared");
        }
        other => {
            kprintln!("Unknown command: '{}' (try 'help')", other);
            serial_println!("[SHELL] unknown command: '{}'", other);
        }
    }
}

fn priority_label(p: Priority) -> &'static str {
    match p {
        Priority::KernelCritical => "KCRIT",
        Priority::High => "HIGH",
        Priority::Normal => "NORM",
        Priority::Background => "BG",
    }
}

fn state_label(s: ProcessState) -> &'static str {
    match s {
        ProcessState::Ready => "READY",
        ProcessState::Running => "RUNNING",
        ProcessState::Blocked => "BLOCKED",
        ProcessState::Terminated => "DEAD",
    }
}
