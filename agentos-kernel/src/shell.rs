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
            kprintln!("Commands: help, ps, mem, uptime, lspci, disk, ls, clear");
            serial_println!(
                "[SHELL] help -> Commands: help, ps, mem, uptime, lspci, disk, ls, clear"
            );
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
        "disk" => {
            let mut buf = [0u8; 512];
            match crate::ata::read_sector(0, &mut buf) {
                Ok(()) => {
                    let sig_ok = buf[510] == 0x55 && buf[511] == 0xAA;
                    kprintln!(
                        "ATA read OK: LBA 0, 512 bytes. Boot signature (bytes 510-511): {:#04x}{:02x} ({})",
                        buf[510],
                        buf[511],
                        if sig_ok { "valid MBR" } else { "unexpected" }
                    );
                    serial_println!(
                        "[SHELL] disk -> LBA0 read OK, signature={:#04x}{:02x} ({})",
                        buf[510],
                        buf[511],
                        if sig_ok { "valid" } else { "UNEXPECTED" }
                    );
                    if sig_ok {
                        for (i, p) in crate::partition::parse_mbr(&buf).iter().enumerate() {
                            if p.partition_type == 0 {
                                continue;
                            }
                            kprintln!(
                                "  Partition {}: {}type={:#04x} ({}) start_lba={} sectors={}",
                                i,
                                if p.bootable { "* " } else { "  " },
                                p.partition_type,
                                crate::partition::partition_type_name(p.partition_type),
                                p.start_lba,
                                p.sector_count
                            );
                            serial_println!(
                                "  partition{} type={:#04x} ({}) start_lba={} sectors={} bootable={}",
                                i,
                                p.partition_type,
                                crate::partition::partition_type_name(p.partition_type),
                                p.start_lba,
                                p.sector_count,
                                p.bootable
                            );
                        }
                    }
                }
                Err(e) => {
                    kprintln!("ATA read failed: {}", e);
                    serial_println!("[SHELL] disk -> FAILED: {}", e);
                }
            }
        }
        "ls" => match list_fat32_root() {
            Ok(()) => {}
            Err(e) => {
                kprintln!("ls: {}", e);
                serial_println!("[SHELL] ls -> FAILED: {}", e);
            }
        },
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

/// Finds the first FAT32 partition in the MBR and lists its root
/// directory - the `ls` command's implementation, pulled out of the
/// dispatch match arm so it can use `?` for the multi-step read/parse
/// chain (MBR -> partition table -> BPB -> cluster-chain walk).
fn list_fat32_root() -> Result<(), &'static str> {
    let mut mbr = [0u8; 512];
    crate::ata::read_sector(0, &mut mbr)?;

    let partitions = crate::partition::parse_mbr(&mbr);
    let fat32 = partitions
        .iter()
        .find(|p| p.partition_type == 0x0B || p.partition_type == 0x0C)
        .ok_or("no FAT32 partition found in the MBR")?;

    let fs = crate::fat32::read_bpb(fat32)?;
    let entries = fs.list_directory(fs.root_cluster)?;

    kprintln!("FAT32 root directory ({} entries):", entries.len());
    serial_println!("[SHELL] ls -> {} entries", entries.len());
    for e in &entries {
        kprintln!(
            "  {}{:<12} {} bytes",
            if e.is_dir { "[DIR] " } else { "      " },
            e.name,
            e.size
        );
        serial_println!(
            "  {} {} bytes cluster={} dir={}",
            e.name,
            e.size,
            e.start_cluster,
            e.is_dir
        );
    }
    Ok(())
}
