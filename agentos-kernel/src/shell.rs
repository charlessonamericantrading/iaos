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
            kprintln!(
                "Commands: help, ps, mem, uptime, date, lspci, disk, ls, cat, touch, rm, mkdir, rmdir, clear"
            );
            serial_println!(
                "[SHELL] help -> Commands: help, ps, mem, uptime, date, lspci, disk, ls, cat, touch, rm, mkdir, rmdir, clear"
            );
        }
        "ps" => {
            kprintln!("PID  PRIO   STATE    NAME              RSP");
            serial_println!("[SHELL] ps ->");
            SCHEDULER.lock().for_each_process(|p| {
                kprintln!(
                    "{:<4} {:<6} {:<8} {:<17} {:#x}",
                    p.pid,
                    priority_label(p.priority),
                    state_label(p.state),
                    p.name,
                    p.stack_pointer
                );
                serial_println!(
                    "  PID {} [{}] {} {} rsp={:#x}",
                    p.pid,
                    priority_label(p.priority),
                    state_label(p.state),
                    p.name,
                    p.stack_pointer
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
        "date" => {
            let t = crate::rtc::read_time();
            kprintln!(
                "{:04}-{:02}-{:02} {:02}:{:02}:{:02} UTC",
                t.year,
                t.month,
                t.day,
                t.hours,
                t.minutes,
                t.seconds
            );
            serial_println!(
                "[SHELL] date -> {:04}-{:02}-{:02} {:02}:{:02}:{:02}",
                t.year,
                t.month,
                t.day,
                t.hours,
                t.minutes,
                t.seconds
            );
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
        "ls" => {
            let dirname = parts.next().unwrap_or("");
            let result = if dirname.is_empty() {
                list_root_directory()
            } else {
                list_fat_directory(dirname)
            };
            if let Err(e) = result {
                kprintln!("ls: {}", e);
                serial_println!("[SHELL] ls -> FAILED: {}", e);
            }
        }
        "mkdir" => {
            let dirname = parts.next().unwrap_or("");
            if dirname.is_empty() {
                kprintln!("mkdir: usage: mkdir DIRNAME");
                serial_println!("[SHELL] mkdir -> no name given");
            } else {
                match create_fat_directory(dirname) {
                    Ok(()) => {
                        kprintln!("Created directory '{}'.", dirname);
                        serial_println!("[SHELL] mkdir {} -> created", dirname);
                    }
                    Err(e) => {
                        kprintln!("mkdir: {}", e);
                        serial_println!("[SHELL] mkdir {} -> FAILED: {}", dirname, e);
                    }
                }
            }
        }
        "rmdir" => {
            let dirname = parts.next().unwrap_or("");
            if dirname.is_empty() {
                kprintln!("rmdir: usage: rmdir DIRNAME");
                serial_println!("[SHELL] rmdir -> no name given");
            } else {
                match delete_fat_directory(dirname) {
                    Ok(()) => {
                        kprintln!("Deleted directory '{}'.", dirname);
                        serial_println!("[SHELL] rmdir {} -> deleted", dirname);
                    }
                    Err(e) => {
                        kprintln!("rmdir: {}", e);
                        serial_println!("[SHELL] rmdir {} -> FAILED: {}", dirname, e);
                    }
                }
            }
        }
        "cat" => {
            let filename = parts.next().unwrap_or("");
            if filename.is_empty() {
                kprintln!("cat: usage: cat FILENAME.EXT");
                serial_println!("[SHELL] cat -> no filename given");
            } else {
                match read_fat_file(filename) {
                    Ok(data) => {
                        kprintln!("Read {} bytes from '{}'.", data.len(), filename);
                        serial_println!("[SHELL] cat {} -> {} bytes", filename, data.len());
                        if data.len() >= 4 {
                            kprintln!(
                                "  first 4 bytes: {:02x} {:02x} {:02x} {:02x}",
                                data[0],
                                data[1],
                                data[2],
                                data[3]
                            );
                            serial_println!(
                                "  first4={:02x}{:02x}{:02x}{:02x}",
                                data[0],
                                data[1],
                                data[2],
                                data[3]
                            );
                        }
                    }
                    Err(e) => {
                        kprintln!("cat: {}", e);
                        serial_println!("[SHELL] cat {} -> FAILED: {}", filename, e);
                    }
                }
            }
        }
        "touch" => {
            let filename = parts.next().unwrap_or("");
            if filename.is_empty() {
                kprintln!("touch: usage: touch FILENAME.EXT [content...]");
                serial_println!("[SHELL] touch -> no filename given");
            } else {
                // Whatever's left on the line becomes the file's content -
                // words are re-joined with single spaces, since this shell
                // has no quoting, so original spacing/formatting between
                // words isn't preserved. Good enough for a real person to
                // create a real file at the prompt; not a general editor.
                let content = parts.collect::<alloc::vec::Vec<&str>>().join(" ");
                match create_fat_file(filename, content.as_bytes()) {
                    Ok(()) => {
                        kprintln!("Created '{}' ({} bytes).", filename, content.len());
                        serial_println!(
                            "[SHELL] touch {} -> created {} bytes",
                            filename,
                            content.len()
                        );
                    }
                    Err(e) => {
                        kprintln!("touch: {}", e);
                        serial_println!("[SHELL] touch {} -> FAILED: {}", filename, e);
                    }
                }
            }
        }
        "rm" => {
            let filename = parts.next().unwrap_or("");
            if filename.is_empty() {
                kprintln!("rm: usage: rm FILENAME.EXT");
                serial_println!("[SHELL] rm -> no filename given");
            } else {
                match delete_fat_file(filename) {
                    Ok(()) => {
                        kprintln!("Deleted '{}'.", filename);
                        serial_println!("[SHELL] rm {} -> deleted", filename);
                    }
                    Err(e) => {
                        kprintln!("rm: {}", e);
                        serial_println!("[SHELL] rm {} -> FAILED: {}", filename, e);
                    }
                }
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

/// Finds the first FAT-typed partition in the MBR. Returns it by value
/// (not a reference into the local `mbr` buffer) so callers aren't tied
/// to that buffer's short lifetime. `pub(crate)` so main.rs's self-tests
/// (e.g. the ATA write test) can find a safe LBA to test against without
/// duplicating this lookup.
pub(crate) fn find_fat_partition() -> Result<crate::partition::PartitionEntry, &'static str> {
    let mut mbr = [0u8; 512];
    crate::ata::read_sector(0, &mut mbr)?;
    crate::partition::parse_mbr(&mbr)
        .into_iter()
        .find(|p| matches!(p.partition_type, 0x01 | 0x04 | 0x06 | 0x0B | 0x0C | 0x0E))
        .ok_or("no FAT-typed partition found in the MBR")
}

fn print_dir_entries(fs_name: &str, entries: &[crate::fat_common::DirEntry]) {
    kprintln!("{} root directory ({} entries):", fs_name, entries.len());
    serial_println!("[SHELL] ls -> {} {} entries", fs_name, entries.len());
    for e in entries {
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
}

/// The `ls` command's implementation: tries FAT32 first, and falls back
/// to FAT12/16 if the partition doesn't parse as FAT32 - our own disk's
/// `0x0C`-typed partition is actually FAT12 (see `fat32::read_bpb`'s doc
/// comment), so this fallback is what makes `ls` actually work on it
/// instead of just reporting "not FAT32" and stopping.
fn list_root_directory() -> Result<(), &'static str> {
    let partition = find_fat_partition()?;
    match crate::fat32::read_bpb(&partition) {
        Ok(fs) => {
            let entries = fs.list_directory(fs.root_cluster)?;
            print_dir_entries("FAT32", &entries);
            Ok(())
        }
        Err(fat32_err) => {
            serial_println!("[SHELL] ls -> not FAT32 ({}), trying FAT12/16", fat32_err);
            let fs = crate::fat12::read_bpb(&partition)?;
            let entries = fs.list_root_directory()?;
            print_dir_entries("FAT12/16", &entries);
            Ok(())
        }
    }
}

/// The `cat` command's implementation - same FAT32-then-FAT12/16 fallback
/// as `list_root_directory`.
fn read_fat_file(name: &str) -> Result<alloc::vec::Vec<u8>, &'static str> {
    let partition = find_fat_partition()?;
    match crate::fat32::read_bpb(&partition) {
        Ok(fs) => fs.read_file(name),
        Err(_) => crate::fat12::read_bpb(&partition)?.read_file(name),
    }
}

/// The `touch` command's implementation. FAT12-only, unlike `read_fat_file`
/// above: `fat32.rs` has no write support at all yet, so a real FAT32 disk
/// would get a clear, honest error here rather than a silent no-op - a
/// currently-untested path in practice, since our own disk is FAT12 (see
/// the FAT32/FAT12 README row), not a gap papered over.
fn create_fat_file(name: &str, data: &[u8]) -> Result<(), &'static str> {
    let partition = find_fat_partition()?;
    if crate::fat32::read_bpb(&partition).is_ok() {
        return Err("FAT32 file creation isn't implemented yet (this disk is FAT12 in practice)");
    }
    let mut fs = crate::fat12::read_bpb(&partition)?;
    fs.create_file(name, data)
}

/// The `rm` command's implementation - same FAT12-only reasoning as
/// `create_fat_file` above.
fn delete_fat_file(name: &str) -> Result<(), &'static str> {
    let partition = find_fat_partition()?;
    if crate::fat32::read_bpb(&partition).is_ok() {
        return Err("FAT32 file deletion isn't implemented yet (this disk is FAT12 in practice)");
    }
    let mut fs = crate::fat12::read_bpb(&partition)?;
    fs.delete_file(name)
}

/// The `mkdir` command's implementation - same FAT12-only reasoning as
/// `create_fat_file` above.
fn create_fat_directory(name: &str) -> Result<(), &'static str> {
    let partition = find_fat_partition()?;
    if crate::fat32::read_bpb(&partition).is_ok() {
        return Err(
            "FAT32 directory creation isn't implemented yet (this disk is FAT12 in practice)",
        );
    }
    let mut fs = crate::fat12::read_bpb(&partition)?;
    fs.create_directory(name)
}

/// The `rmdir` command's implementation - same FAT12-only reasoning as
/// `create_fat_file` above.
fn delete_fat_directory(name: &str) -> Result<(), &'static str> {
    let partition = find_fat_partition()?;
    if crate::fat32::read_bpb(&partition).is_ok() {
        return Err(
            "FAT32 directory deletion isn't implemented yet (this disk is FAT12 in practice)",
        );
    }
    let mut fs = crate::fat12::read_bpb(&partition)?;
    fs.delete_directory(name)
}

/// The `ls DIRNAME` command's implementation - lists a named
/// SUBdirectory's contents (bare `ls` lists the root instead, via
/// `list_root_directory` above). FAT12-only, same reasoning as
/// `create_fat_file`/`delete_fat_file`.
fn list_fat_directory(name: &str) -> Result<(), &'static str> {
    let partition = find_fat_partition()?;
    if crate::fat32::read_bpb(&partition).is_ok() {
        return Err(
            "listing a FAT32 subdirectory by name isn't wired up yet (this disk is FAT12 in practice)",
        );
    }
    let fs = crate::fat12::read_bpb(&partition)?;
    let entry = fs
        .list_root_directory()?
        .into_iter()
        .find(|e| e.name.eq_ignore_ascii_case(name))
        .ok_or("no file or directory with that name")?;
    if !entry.is_dir {
        return Err("that's a file, not a directory - try 'cat' instead");
    }

    let sub_entries = fs.list_directory(entry.start_cluster)?;
    kprintln!("{} ({} entries):", name, sub_entries.len());
    serial_println!("[SHELL] ls {} -> {} entries", name, sub_entries.len());
    for e in &sub_entries {
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
