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
mod ring3;
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

    // 2a-2. Test the ring-3 GDT/TSS foundation (Fase 68) - the first
    // step of a real usermode transition arc, deliberately scoped to
    // just the infrastructure: real ring-3 code/data segments exist in
    // the GDT now, and the TSS has a real kernel stack (RSP0) ready for
    // when a ring-3->ring-0 control transfer needs one. Does NOT yet
    // attempt an actual transition (no IDT gate has DPL=3, no page is
    // marked user-accessible, nothing executes iretq/sysret to ring 3
    // yet) - that's real, substantial follow-on work, matching this
    // project's own established multi-Fase pattern for large features
    // (e.g. net::e1000/net::virtio's own probe -> setup -> send -> ...
    // progressions).
    //
    // user_code_rpl/user_data_rpl=3 is genuine proof these selectors'
    // Requested Privilege Level bits are really Ring 3, not just "some
    // nonzero selector happened to get appended" - a wrong descriptor
    // (e.g. accidentally appending another kernel segment) would show
    // rpl=0 here instead. rsp0_nonzero=true is genuine proof a real
    // stack was allocated and installed, not the TSS's own zeroed
    // default (which would triple-fault the instant it was ever used).
    kprintln!("[KERNEL INIT] Testing ring-3 GDT/TSS foundation...");
    {
        let info = gdt::ring3_info();
        let user_code_rpl_ok = info.user_code_rpl == 3;
        let user_data_rpl_ok = info.user_data_rpl == 3;
        let rsp0_nonzero = info.rsp0 != 0;
        kprintln!(
            "[GDT] user_code_selector={:#06x} user_data_selector={:#06x} user_code_rpl_ok={} user_data_rpl_ok={} rsp0_nonzero={}",
            info.user_code_selector,
            info.user_data_selector,
            user_code_rpl_ok,
            user_data_rpl_ok,
            rsp0_nonzero
        );
        serial_println!(
            "[GDT] ring3_test user_code_selector={:#06x} user_data_selector={:#06x} user_code_rpl_ok={} user_data_rpl_ok={} rsp0_nonzero={}",
            info.user_code_selector,
            info.user_data_selector,
            user_code_rpl_ok,
            user_data_rpl_ok,
            rsp0_nonzero
        );
    }

    kprintln!("[KERNEL INIT] Loading IDT Interrupt Handlers...");
    interrupts::init_idt();

    // 2b. Smoke-test the breakpoint handler: if this line is followed by
    // "[EXCEPTION] Breakpoint..." instead of a reset/hang, exception handling works.
    kprintln!("[KERNEL INIT] Testing breakpoint exception handler (int3)...");
    x86_64::instructions::interrupts::int3();
    kprintln!("[KERNEL INIT] Execution resumed after breakpoint - handler OK.");

    // 2b-2. Test the real DPL=3 syscall gate (Fase 69, the second step of
    // the ring-3 arc after Fase 68's own GDT/TSS foundation) - fires
    // `int 0x80` for real right now, from ring-0 (no ring-3 code exists
    // yet to actually test the privilege-lowering path itself, but
    // CPL=0 <= DPL=3 is always a permitted invocation, so this already
    // proves the gate is real and correctly wired, not just that its
    // DPL bits read back as 3 in isolation). count_delta==1 is genuine
    // proof the handler ran exactly once, not a coincidental read of an
    // already-nonzero counter or a silently-swallowed fault.
    //
    // Fase 72 strengthens this same call site: rax/rdi/rsi/rdx are now
    // pinned to known values (SYS_SERIAL_PRINT, 0, 0, 0) instead of
    // whatever happened to be left over from earlier code, and the real
    // return value comes back in rax - proving the new naked
    // `syscall_entry_asm` genuinely reads real registers and dispatches
    // to the real `syscall::dispatch_syscall`, not just that the gate
    // fires. Pinning the inputs explicitly (rather than leaving them as
    // whatever garbage preceded this point) keeps this test
    // deterministic across builds, and avoids ever hitting
    // dispatch_syscall's "Unknown System Call Number" arm by accident.
    kprintln!("[KERNEL INIT] Testing DPL=3 syscall gate (int 0x80, real args)...");
    let syscall_int_count_before = interrupts::syscall_int_count();
    let mut syscall_rax: u64 = syscall::SYS_SERIAL_PRINT;
    unsafe {
        core::arch::asm!(
            "int 0x80",
            inout("rax") syscall_rax,
            in("rdi") 0u64,
            in("rsi") 0u64,
            in("rdx") 0u64,
            options(nostack)
        );
    }
    let syscall_int_count_after = interrupts::syscall_int_count();
    let syscall_int_count_delta = syscall_int_count_after - syscall_int_count_before;
    kprintln!(
        "[KERNEL INIT] Resumed after int 0x80 - count_delta={} returned={}",
        syscall_int_count_delta,
        syscall_rax
    );
    serial_println!(
        "[IDT] syscall_int_test count_delta={} dispatch_ret={}",
        syscall_int_count_delta,
        syscall_rax
    );

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
    // Fase 74: installed globally right away, not kept as a local - only
    // one `OffsetPageTable` can safely exist at a time (it wraps a unique
    // `&'static mut PageTable`), so every use from here on, including the
    // two right below, goes through `memory::paging::with_mapper` instead
    // of a plain `&mut mapper` - see that module's own doc for why.
    memory::paging::install_global(unsafe { memory::init(phys_mem_offset) });
    let mut frame_allocator =
        unsafe { memory::frame_allocator::BootInfoFrameAllocator::init(&boot_info.memory_regions) };
    memory::paging::with_mapper(|mapper| memory::heap::init_heap(mapper, &mut frame_allocator))
        .expect("heap initialization failed");
    kprintln!(
        "[KERNEL INIT] Heap mapped at {:#x}, {} KiB - alloc (Vec/Box/String) now live.",
        memory::heap::HEAP_START,
        memory::heap::HEAP_SIZE / 1024
    );

    // 2d-2. Map a real user-accessible page (Fase 70, the third step of
    // the ring-3 arc after Fase 68's GDT/TSS and Fase 69's DPL=3 int
    // 0x80 gate) - deliberately a NEW, separate mapping, not reusing the
    // heap (which was, and stays, kernel-only). Still entirely a ring-0
    // operation: nothing runs in ring-3 yet, so this only proves the
    // MAPPING itself is genuinely correct (both structurally, via the
    // real PTE flags read back through the page tables, and
    // functionally, via a real write+read-back through the mapping) -
    // a future Fase still needs the actual ring-3 entry to prove the
    // USER_ACCESSIBLE bit is what makes the real difference.
    kprintln!("[KERNEL INIT] Mapping a real user-accessible test page...");
    memory::paging::with_mapper(|mapper| {
        memory::user_page::map_user_test_page(mapper, &mut frame_allocator)
    })
    .expect("user test page mapping failed");
    {
        let info =
            memory::paging::with_mapper(|mapper| memory::user_page::inspect_user_test_page(mapper));
        kprintln!(
            "[MEMORY] user_page_test present={} writable={} user_accessible={} write_read_back_ok={}",
            info.present,
            info.writable,
            info.user_accessible,
            info.write_read_back_ok
        );
        serial_println!(
            "[MEMORY] user_page_test present={} writable={} user_accessible={} write_read_back_ok={}",
            info.present,
            info.writable,
            info.user_accessible,
            info.write_read_back_ok
        );
    }

    // Fase 99: a SECOND, independent user page - reserved for ring-3
    // programs that need their own memory instead of sharing the one
    // page above with every other ring-3 test (see
    // ring3::run_ring3_disk_loaded_test, repointed at this address
    // below). Verified the same two independent ways as the page above:
    // structurally (real PTE flags) and functionally (write+read-back).
    kprintln!("[KERNEL INIT] Mapping a second, independent user page for disk-loaded programs...");
    memory::paging::with_mapper(|mapper| {
        memory::user_page::map_user_page_at(
            mapper,
            &mut frame_allocator,
            memory::user_page::USER_DISK_PROGRAM_PAGE_ADDR,
        )
    })
    .expect("disk program page mapping failed");
    {
        let info = memory::paging::with_mapper(|mapper| {
            memory::user_page::inspect_user_page_at(
                mapper,
                memory::user_page::USER_DISK_PROGRAM_PAGE_ADDR,
            )
        });
        kprintln!(
            "[MEMORY] disk_program_page present={} writable={} user_accessible={} write_read_back_ok={}",
            info.present,
            info.writable,
            info.user_accessible,
            info.write_read_back_ok
        );
        serial_println!(
            "[MEMORY] disk_program_page present={} writable={} user_accessible={} write_read_back_ok={}",
            info.present,
            info.writable,
            info.user_accessible,
            info.write_read_back_ok
        );
    }

    // Fase 100: a THIRD, independent user page - a dedicated stack for
    // the same disk-loaded program, separate from its code page above.
    // See ring3::run_ring3_disk_loaded_test, now using this address for
    // stack_top instead of sharing the tail of its own code page.
    kprintln!("[KERNEL INIT] Mapping a third, independent user page as a dedicated stack...");
    memory::paging::with_mapper(|mapper| {
        memory::user_page::map_user_page_at(
            mapper,
            &mut frame_allocator,
            memory::user_page::USER_DISK_PROGRAM_STACK_ADDR,
        )
    })
    .expect("disk program stack page mapping failed");
    {
        let info = memory::paging::with_mapper(|mapper| {
            memory::user_page::inspect_user_page_at(
                mapper,
                memory::user_page::USER_DISK_PROGRAM_STACK_ADDR,
            )
        });
        kprintln!(
            "[MEMORY] disk_program_stack present={} writable={} user_accessible={} write_read_back_ok={}",
            info.present,
            info.writable,
            info.user_accessible,
            info.write_read_back_ok
        );
        serial_println!(
            "[MEMORY] disk_program_stack present={} writable={} user_accessible={} write_read_back_ok={}",
            info.present,
            info.writable,
            info.user_accessible,
            info.write_read_back_ok
        );
    }

    // Fase 103: a FOURTH, independent user page - a real DATA segment
    // for the same disk-loaded program, distinct from its code and
    // stack pages above. See ring3::run_ring3_disk_loaded_test, whose
    // program now writes a real signature byte here instead of only
    // ever touching its own code/stack.
    kprintln!("[KERNEL INIT] Mapping a fourth, independent user page as a real data segment...");
    memory::paging::with_mapper(|mapper| {
        memory::user_page::map_user_page_at(
            mapper,
            &mut frame_allocator,
            memory::user_page::USER_DISK_PROGRAM_DATA_ADDR,
        )
    })
    .expect("disk program data page mapping failed");
    {
        let info = memory::paging::with_mapper(|mapper| {
            memory::user_page::inspect_user_page_at(
                mapper,
                memory::user_page::USER_DISK_PROGRAM_DATA_ADDR,
            )
        });
        kprintln!(
            "[MEMORY] disk_program_data present={} writable={} user_accessible={} write_read_back_ok={}",
            info.present,
            info.writable,
            info.user_accessible,
            info.write_read_back_ok
        );
        serial_println!(
            "[MEMORY] disk_program_data present={} writable={} user_accessible={} write_read_back_ok={}",
            info.present,
            info.writable,
            info.user_accessible,
            info.write_read_back_ok
        );
    }

    // Fase 73: closes the ring-3 arc's own last remaining gap - a SAFE
    // ring-3 -> ring-0 return, unlike every earlier ring-3 test (Fase 71/
    // 72's ring3test/ring3syscall, both deliberately-opt-in shell
    // commands that end in a permanent halt). This one genuinely
    // resumes normal kernel execution afterward, so - unlike those two -
    // it runs right here, unconditionally, as a normal self-test.
    kprintln!("[KERNEL INIT] Testing a real ring-3 program that exits voluntarily...");
    let ring3_exit_code = ring3::run_ring3_exit_test();
    kprintln!(
        "[KERNEL INIT] Back from ring-3 for good - exit_code={}",
        ring3_exit_code
    );

    // Fase 76: proves SYS_TENSOR_EVAL's new USER_ACCESSIBLE enforcement
    // is real - a genuine ring-3 call pointing at kernel-only memory
    // (the heap) must now be rejected, unlike a mere unmapped pointer
    // (Fase 74's own test) or a valid, user-accessible one (this same
    // syscall's own passing case, proven from ring-0 further below).
    // Reuses the SAME safe-return mechanism as the test right above, so
    // it also runs unconditionally rather than needing an opt-in
    // command.
    kprintln!(
        "[KERNEL INIT] Testing SYS_TENSOR_EVAL rejects a ring-3 pointer to kernel-only memory..."
    );
    let ring3_reject_code = ring3::run_ring3_pointer_reject_test();
    kprintln!(
        "[KERNEL INIT] Back from ring-3 - kernel-only pointer test returned {:#x}",
        ring3_reject_code
    );

    // Fase 77: proves the pointer check above now covers a slice's WHOLE
    // length, not just where it starts - a real ring-3 call whose
    // "weights" pointer sits on a genuinely valid, user-accessible page
    // but whose stated length runs off the end into an unmapped one must
    // also be rejected. Same safe-return mechanism, same "runs
    // unconditionally" reasoning as the two tests above.
    kprintln!(
        "[KERNEL INIT] Testing SYS_TENSOR_EVAL rejects a ring-3 slice that overruns its starting page..."
    );
    let ring3_overrun_code = ring3::run_ring3_slice_overrun_test();
    kprintln!(
        "[KERNEL INIT] Back from ring-3 - slice-overrun test returned {:#x}",
        ring3_overrun_code
    );

    // Fase 79: first step toward eventual multi-ring3-process scheduling
    // - proves the timer interrupt can tell a tick landed while ring-3
    // code was running, not just that a tick happened. Same safe-return
    // mechanism, same "runs unconditionally" reasoning as the three
    // tests above.
    kprintln!(
        "[KERNEL INIT] Testing whether a real timer tick can be detected while ring-3 code is running..."
    );
    let ring3_timer_tick_code = ring3::run_ring3_timer_tick_test();
    kprintln!(
        "[KERNEL INIT] Back from ring-3 - timer-tick-detection test returned exit_code={}",
        ring3_timer_tick_code
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

    // 5b. Test real GGUF Q8_0 quantized tensor decoding (Fase 50) - pure
    // byte-transformation logic, deliberately disk-independent (unlike
    // the disk-load test above): GgufTensorInfo has parsed a tensor's
    // real gtype since Fase 43, but until this Fase nothing ever
    // branched on it to decide *how* to decode - every caller assumed
    // F32. decode_q8_0/decode_tensor and the f16_to_f32 helper they need
    // (GGML's block scale is a real half-precision float, and this
    // no_std build has no f16 type of its own) were verified against
    // GGML's actual block_q8_0 struct and IEEE 754 binary16's defined
    // cases respectively, the same discipline Fase 47 used for RFC 1071.
    //
    // f16_vectors_ok is hand-computed, independent of decode_q8_0's own
    // round trip - the same reasoning Fase 47 applied to its checksum
    // vectors: a round-trip-only test would still pass even if
    // f16_to_f32 consistently disagreed with the real IEEE 754 standard,
    // since decode_q8_0 would just consistently agree with itself. Covers
    // zero, the smallest subnormal (2^-24, the trickiest branch - a
    // normalizing left-shift loop), and a normal negative value.
    // q8_0_decode_ok builds a real 34-byte block (scale=0.5, values -16
    // through 15) and checks the decoded values against directly
    // computing `value * 0.5` on the original i8s - independent of
    // decode_q8_0's own byte-parsing, so a block-size or byte-order bug
    // would still be caught even though both sides agree on what 0.5
    // means. dispatch_f32_ok/dispatch_q8_0_ok/dispatch_unimplemented_ok
    // prove decode_tensor actually branches on gtype rather than
    // silently ignoring it, including the honest Err for a gtype with no
    // decoder yet (F16 here) rather than silently misreading it as F32.
    kprintln!("[GGUF INFERENCE] Testing Q8_0 quantized tensor decoding...");
    {
        use gguf_loader::{f16_to_f32, GgufModelLoader};

        // Smallest binary16 subnormal (bits=0x0001) is exactly 2^-24 -
        // built directly from its own bit pattern (biased exponent
        // 127-24=103, zero mantissa) rather than a runtime `powi` (not
        // available without libm in this #![no_std] build anyway),
        // independently of f16_to_f32's own bit-construction logic.
        let smallest_subnormal_f32 = f32::from_bits(103u32 << 23);
        let f16_vectors_ok = [
            (0x0000u16, 0.0f32),
            (0xC000u16, -2.0f32),
            (0x3C00u16, 1.0f32),
            (0x0001u16, smallest_subnormal_f32),
        ]
        .iter()
        .all(|&(bits, expected)| f16_to_f32(bits) == expected);

        let qs: [i8; 32] = core::array::from_fn(|i| (i as i8) - 16);
        let mut q8_block: alloc::vec::Vec<u8> = alloc::vec::Vec::with_capacity(34);
        q8_block.extend_from_slice(&0x3800u16.to_le_bytes()); // f16 0.5
        for &v in &qs {
            q8_block.push(v as u8);
        }
        let decoded = GgufModelLoader::decode_q8_0(&q8_block);
        let expected: alloc::vec::Vec<f32> = qs.iter().map(|&v| v as f32 * 0.5).collect();
        let q8_0_decode_ok = decoded == expected;

        let f32_bytes = 2.5f32.to_le_bytes();
        let dispatch_f32_ok =
            GgufModelLoader::decode_tensor(&f32_bytes, GgufGtype::F32) == Ok(alloc::vec![2.5f32]);
        let dispatch_q8_0_ok =
            GgufModelLoader::decode_tensor(&q8_block, GgufGtype::Q8_0) == Ok(expected);
        // No "dispatch_unimplemented_ok" check anymore: this test was
        // written (Fase 50) when F16 was decode_tensor's one remaining
        // unimplemented gtype, specifically to prove an honest Err came
        // back for it rather than a silent F32 misread. Fase 54 gave F16
        // a real decoder too, so every GgufGtype this kernel recognizes
        // now dispatches successfully - there's no longer a gtype value
        // left to exercise that Err path with, so the check (and its
        // premise) is retired rather than left in place testing nothing
        // real. Confirmed by the CI check for THIS line actually
        // regressing (dispatch_unimplemented_ok flipping to false) when
        // this Fase's own new capability landed, before this fix.

        kprintln!(
            "[GGUF Q8_0] f16_vectors_ok={} q8_0_decode_ok={} dispatch_f32_ok={} dispatch_q8_0_ok={}",
            f16_vectors_ok, q8_0_decode_ok, dispatch_f32_ok, dispatch_q8_0_ok
        );
        serial_println!(
            "[GGUF] q8_0_test f16_vectors_ok={} q8_0_decode_ok={} dispatch_f32_ok={} dispatch_q8_0_ok={}",
            f16_vectors_ok, q8_0_decode_ok, dispatch_f32_ok, dispatch_q8_0_ok
        );
    }

    // 5b-2. Test real GGUF Q4_0 quantized tensor decoding (Fase 52) - the
    // one remaining quantized format deliberately deferred alongside Q4_1
    // in Fase 50 (Q8_0). Same disk-independent, pure byte-transformation
    // scope as that Fase. The block layout (18 bytes: 2-byte f16 scale +
    // 16 bytes of packed 4-bit nibbles) was verified directly against
    // GGML's actual dequantize_row_q4_0 source, specifically because its
    // packing is split-half (byte j's low nibble is value[j], high
    // nibble is value[j+16]) rather than the equally-plausible-looking
    // interleaved guess (value[2j]/value[2j+1]) - getting this wrong
    // would silently scramble every decoded tensor, not error.
    //
    // q4_0_decode_ok builds one real 18-byte block with a DELIBERATELY
    // ASYMMETRIC nibble pattern per byte (low nibble = j ascending,
    // high nibble = 15-j descending, for j in 0..16) specifically so a
    // low/high-nibble swap OR an interleaved-vs-split-half packing bug
    // would produce a completely different, easily-distinguishable wrong
    // sequence rather than accidentally passing. dispatch_q4_0_ok proves
    // decode_tensor (Fase 50) now routes Q4_0 to this new decoder too.
    kprintln!("[GGUF INFERENCE] Testing Q4_0 quantized tensor decoding...");
    {
        use gguf_loader::GgufModelLoader;

        const SCALE_BITS: u16 = 0x3800; // f16 0.5
        let mut q4_block: alloc::vec::Vec<u8> = alloc::vec::Vec::with_capacity(18);
        q4_block.extend_from_slice(&SCALE_BITS.to_le_bytes());
        let mut expected_q4: alloc::vec::Vec<f32> = alloc::vec![0.0f32; 32];
        for j in 0..16usize {
            let low_nibble = j as u8; // 0..15, ascending
            let high_nibble = (15 - j) as u8; // 15..0, descending
            q4_block.push(low_nibble | (high_nibble << 4));
            expected_q4[j] = (low_nibble as i32 - 8) as f32 * 0.5;
            expected_q4[j + 16] = (high_nibble as i32 - 8) as f32 * 0.5;
        }
        let decoded_q4 = GgufModelLoader::decode_q4_0(&q4_block);
        let q4_0_decode_ok = decoded_q4 == expected_q4;
        let dispatch_q4_0_ok =
            GgufModelLoader::decode_tensor(&q4_block, GgufGtype::Q4_0) == Ok(expected_q4);

        kprintln!(
            "[GGUF Q4_0] q4_0_decode_ok={} dispatch_q4_0_ok={}",
            q4_0_decode_ok,
            dispatch_q4_0_ok
        );
        serial_println!(
            "[GGUF] q4_0_test q4_0_decode_ok={} dispatch_q4_0_ok={}",
            q4_0_decode_ok,
            dispatch_q4_0_ok
        );
    }

    // 5b-3. Test real GGUF Q4_1 quantized tensor decoding (Fase 53) - the
    // last legacy quantization format after Q8_0 (Fase 50) and Q4_0
    // (Fase 52), completing that thread. Same 4-bit split-half nibble
    // packing as Q4_0 (verified to genuinely carry over unchanged, not
    // just assumed, via GGML's actual dequantize_row_q4_1 source), but a
    // full affine dequantization (nibble*scale+min) instead of Q4_0's
    // symmetric (nibble-8)*scale - a second f16 "min" field lets Q4_1
    // represent asymmetric value ranges Q4_0's fixed symmetric range
    // can't.
    //
    // q4_1_decode_ok builds one real 20-byte block with scale=0.5,
    // min=1.0 (deliberately nonzero and distinct from each other, so a
    // bug conflating scale and min, or dropping either one, produces
    // visibly wrong values) and the SAME asymmetric nibble pattern
    // (low ascending, high descending) as Q4_0's self-test, for the same
    // "catch a swap/interleave bug" reasoning. dispatch_q4_1_ok proves
    // decode_tensor (Fase 50) now routes Q4_1 too - meaning every
    // GgufGtype except F16 now has a real decoder.
    kprintln!("[GGUF INFERENCE] Testing Q4_1 quantized tensor decoding...");
    {
        use gguf_loader::GgufModelLoader;

        const SCALE_BITS: u16 = 0x3800; // f16 0.5
        const MIN_BITS: u16 = 0x3C00; // f16 1.0
        let mut q4_1_block: alloc::vec::Vec<u8> = alloc::vec::Vec::with_capacity(20);
        q4_1_block.extend_from_slice(&SCALE_BITS.to_le_bytes());
        q4_1_block.extend_from_slice(&MIN_BITS.to_le_bytes());
        let mut expected_q4_1: alloc::vec::Vec<f32> = alloc::vec![0.0f32; 32];
        for j in 0..16usize {
            let low_nibble = j as u8; // 0..15, ascending
            let high_nibble = (15 - j) as u8; // 15..0, descending
            q4_1_block.push(low_nibble | (high_nibble << 4));
            expected_q4_1[j] = low_nibble as f32 * 0.5 + 1.0;
            expected_q4_1[j + 16] = high_nibble as f32 * 0.5 + 1.0;
        }
        let decoded_q4_1 = GgufModelLoader::decode_q4_1(&q4_1_block);
        let q4_1_decode_ok = decoded_q4_1 == expected_q4_1;
        let dispatch_q4_1_ok =
            GgufModelLoader::decode_tensor(&q4_1_block, GgufGtype::Q4_1) == Ok(expected_q4_1);

        kprintln!(
            "[GGUF Q4_1] q4_1_decode_ok={} dispatch_q4_1_ok={}",
            q4_1_decode_ok,
            dispatch_q4_1_ok
        );
        serial_println!(
            "[GGUF] q4_1_test q4_1_decode_ok={} dispatch_q4_1_ok={}",
            q4_1_decode_ok,
            dispatch_q4_1_ok
        );
    }

    // 5b-4. Test real GGUF F16 tensor decoding (Fase 54) - the last
    // remaining GgufGtype, completing full decode_tensor coverage.
    // Unlike every quantized format above, F16 has no block/scale
    // structure at all - each value is simply its own raw 2 bytes,
    // decoded via the already-verified f16_to_f32 (built for the
    // quantized formats' block scales, reused here directly). Uses the
    // same hand-verified bit patterns (0x3C00/0xC000/0x3800 -> 1.0/-2.0/
    // 0.5) already proven correct in Fase 50's own f16_to_f32 vectors.
    kprintln!("[GGUF INFERENCE] Testing F16 tensor decoding...");
    {
        use gguf_loader::GgufModelLoader;

        let mut f16_bytes: alloc::vec::Vec<u8> = alloc::vec::Vec::new();
        f16_bytes.extend_from_slice(&0x3C00u16.to_le_bytes()); // 1.0
        f16_bytes.extend_from_slice(&0xC000u16.to_le_bytes()); // -2.0
        f16_bytes.extend_from_slice(&0x3800u16.to_le_bytes()); // 0.5
        let expected_f16: alloc::vec::Vec<f32> = alloc::vec![1.0, -2.0, 0.5];

        let decoded_f16 = GgufModelLoader::decode_f16_le(&f16_bytes);
        let f16_decode_ok = decoded_f16 == expected_f16;
        let dispatch_f16_ok =
            GgufModelLoader::decode_tensor(&f16_bytes, GgufGtype::F16) == Ok(expected_f16);

        kprintln!(
            "[GGUF F16] f16_decode_ok={} dispatch_f16_ok={}",
            f16_decode_ok,
            dispatch_f16_ok
        );
        serial_println!(
            "[GGUF] f16_test f16_decode_ok={} dispatch_f16_ok={}",
            f16_decode_ok,
            dispatch_f16_ok
        );
    }

    // 5b-5. Test real GGUF Q4_K super-block quantized tensor decoding
    // (Fase 57) - GGML's "K-quant" family, structurally different from
    // (and meaningfully more complex than) the legacy Q8_0/Q4_0/Q4_1
    // formats above: 256 values per 144-byte block, split into 8 sub-
    // blocks of 32 with their OWN 6-bit-packed scale and min each, not
    // one scale for the whole block. Getting this right needed real,
    // deliberate care beyond the usual "fetch the source, verify a test
    // vector" pattern: an early research fetch of this exact struct's
    // field sizes came back wrong (`scales[8]`/`qs[64]` instead of the
    // real `scales[12]`/`qs[128]`, caught by cross-checking against
    // GGML's own K_SCALE_SIZE/QK_K constants) - so get_scale_min_k4's
    // own intricate bit-unpacking (see its own doc) was independently
    // hand-derived and verified with a complete round trip covering all
    // 8 sub-blocks before any of this was trusted enough to implement.
    //
    // get_scale_min_k4_ok re-runs that exact same hand-verified round
    // trip as a real, compiled self-test (not just scratch-paper math):
    // a 12-byte scales array encoding sc=[7,14,21,28,35,42,49,56],
    // m=[56,49,42,35,28,21,14,7] - chosen to be small, distinct, and
    // exercise every sub-block index and both branches of get_scale_
    // min_k4's own `j < 4` split. decode_q4_k_ok builds one full real
    // 144-byte block (d=dmin=1.0, using that same scales array) with
    // FOUR distinct 32-byte qs chunks (0x00/0xFF/0x73/0x21 - deliberately
    // different low/high nibble values per chunk) and checks all 256
    // decoded values against 8 hand-computed constants (one low/high
    // pair per chunk) - proving the outer sub-block loop, the q_offset
    // chunk advancement, and the low/high nibble split are all correct
    // together, not just get_scale_min_k4 in isolation.
    kprintln!("[GGUF INFERENCE] Testing Q4_K super-block quantized tensor decoding...");
    {
        use gguf_loader::{get_scale_min_k4, GgufModelLoader};

        const SCALES: [u8; 12] = [135, 142, 213, 220, 120, 113, 42, 35, 195, 90, 225, 120];
        const EXPECTED_SC: [u8; 8] = [7, 14, 21, 28, 35, 42, 49, 56];
        const EXPECTED_M: [u8; 8] = [56, 49, 42, 35, 28, 21, 14, 7];
        let get_scale_min_k4_ok = (0..8).all(|j| {
            let (sc, m) = get_scale_min_k4(j, &SCALES);
            sc == EXPECTED_SC[j] && m == EXPECTED_M[j]
        });

        let mut q4k_block: alloc::vec::Vec<u8> = alloc::vec::Vec::with_capacity(144);
        q4k_block.extend_from_slice(&0x3C00u16.to_le_bytes()); // d = 1.0
        q4k_block.extend_from_slice(&0x3C00u16.to_le_bytes()); // dmin = 1.0
        q4k_block.extend_from_slice(&SCALES);
        q4k_block.extend(core::iter::repeat_n(0x00u8, 32)); // chunk 0
        q4k_block.extend(core::iter::repeat_n(0xFFu8, 32)); // chunk 1
        q4k_block.extend(core::iter::repeat_n(0x73u8, 32)); // chunk 2
        q4k_block.extend(core::iter::repeat_n(0x21u8, 32)); // chunk 3

        let decoded = GgufModelLoader::decode_q4_k(&q4k_block);
        let mut expected_q4k: alloc::vec::Vec<f32> = alloc::vec::Vec::with_capacity(256);
        expected_q4k.extend(core::iter::repeat_n(-56.0f32, 32)); // chunk0 low: 7*0-56
        expected_q4k.extend(core::iter::repeat_n(-49.0f32, 32)); // chunk0 high: 14*0-49
        expected_q4k.extend(core::iter::repeat_n(273.0f32, 32)); // chunk1 low: 21*15-42
        expected_q4k.extend(core::iter::repeat_n(385.0f32, 32)); // chunk1 high: 28*15-35
        expected_q4k.extend(core::iter::repeat_n(77.0f32, 32)); // chunk2 low: 35*3-28
        expected_q4k.extend(core::iter::repeat_n(273.0f32, 32)); // chunk2 high: 42*7-21
        expected_q4k.extend(core::iter::repeat_n(35.0f32, 32)); // chunk3 low: 49*1-14
        expected_q4k.extend(core::iter::repeat_n(105.0f32, 32)); // chunk3 high: 56*2-7
        let decode_q4_k_ok = decoded == expected_q4k;

        let dispatch_q4_k_ok =
            GgufModelLoader::decode_tensor(&q4k_block, GgufGtype::Q4_K) == Ok(expected_q4k);

        kprintln!(
            "[GGUF Q4_K] get_scale_min_k4_ok={} decode_q4_k_ok={} dispatch_q4_k_ok={}",
            get_scale_min_k4_ok,
            decode_q4_k_ok,
            dispatch_q4_k_ok
        );
        serial_println!(
            "[GGUF] q4_k_test get_scale_min_k4_ok={} decode_q4_k_ok={} dispatch_q4_k_ok={}",
            get_scale_min_k4_ok,
            decode_q4_k_ok,
            dispatch_q4_k_ok
        );
    }

    // 5b-6. Test real GGUF Q6_K super-block quantized tensor decoding
    // (Fase 58) - another K-quant format, but structurally different from
    // Q4_K in three ways checked rather than assumed: `d` (the single
    // super-block scale) comes LAST in the struct, not first; the 6-bit
    // value is split across two separate arrays (`ql` low 4 bits, `qh`
    // high 2 bits) rather than packed into one nibble-pair array; and
    // `scales[16]` are plain signed i8 bytes, not 6-bit-packed like
    // Q4_K's scales[12] - no bit-extraction helper needed here at all.
    //
    // The struct's fetched size (ql[128]+qh[64]+scales[16]+d(2)=210
    // bytes) was cross-checked against GGML's own dequantize_row_q6_K
    // pointer arithmetic (ql+=64, qh+=32, sc+=8 per 128-value half-block,
    // over 2 half-blocks) before being trusted - this Fase's version of
    // the same internal-consistency check that caught Q4_K's wrong
    // fetched field sizes last time. The four-quadrant bit-extraction
    // (q1..q4 sharing one qh[l] byte, each taking a different
    // non-overlapping 2-bit field) was hand-traced with concrete bytes
    // (ql[0]=0xAB, ql[32]=0xCD, qh[0]=0xE4) before any Rust was written:
    // decomposing 0xE4's four 2-bit fields (0,1,2,3) and combining with
    // ql's nibbles gives q1=-21, q2=-3, q3=10, q4=28.
    //
    // decode_q6_k_ok builds one full real 210-byte block using a
    // UNIFORM ql/qh byte pattern (so q1..q4 stay constant across every
    // `l`) paired with eight DISTINCT prime scales [2,3,5,7,11,13,17,19]
    // - chosen specifically to exercise both branches of `is=l/16` (0
    // and 1) for all four quadrants, proving the sc[is]/sc[is+2]/
    // sc[is+4]/sc[is+6] indexing is correct, not just the bit
    // extraction in isolation.
    kprintln!("[GGUF INFERENCE] Testing Q6_K super-block quantized tensor decoding...");
    {
        const SCALES8: [i8; 8] = [2, 3, 5, 7, 11, 13, 17, 19];

        let mut q6k_block: Vec<u8> = Vec::with_capacity(210);
        // ql[128]: half 0 then half 1, each 64 bytes = 32x0xAB + 32x0xCD
        for _ in 0..2 {
            q6k_block.extend(core::iter::repeat_n(0xABu8, 32));
            q6k_block.extend(core::iter::repeat_n(0xCDu8, 32));
        }
        // qh[64]: half 0 then half 1, each 32 bytes of 0xE4
        for _ in 0..2 {
            q6k_block.extend(core::iter::repeat_n(0xE4u8, 32));
        }
        // scales[16]: same 8 distinct primes repeated for both halves
        for _ in 0..2 {
            for &s in &SCALES8 {
                q6k_block.push(s as u8);
            }
        }
        // d = 1.0, LAST
        q6k_block.extend_from_slice(&0x3C00u16.to_le_bytes());

        let decoded = GgufModelLoader::decode_q6_k(&q6k_block);
        let mut expected_q6k: Vec<f32> = Vec::with_capacity(256);
        for _ in 0..2 {
            expected_q6k.extend(core::iter::repeat_n(-42.0f32, 16)); // is=0, sc[0]=2, q1=-21
            expected_q6k.extend(core::iter::repeat_n(-63.0f32, 16)); // is=1, sc[1]=3, q1=-21
            expected_q6k.extend(core::iter::repeat_n(-15.0f32, 16)); // is=0, sc[2]=5, q2=-3
            expected_q6k.extend(core::iter::repeat_n(-21.0f32, 16)); // is=1, sc[3]=7, q2=-3
            expected_q6k.extend(core::iter::repeat_n(110.0f32, 16)); // is=0, sc[4]=11, q3=10
            expected_q6k.extend(core::iter::repeat_n(130.0f32, 16)); // is=1, sc[5]=13, q3=10
            expected_q6k.extend(core::iter::repeat_n(476.0f32, 16)); // is=0, sc[6]=17, q4=28
            expected_q6k.extend(core::iter::repeat_n(532.0f32, 16)); // is=1, sc[7]=19, q4=28
        }
        let decode_q6_k_ok = decoded == expected_q6k;

        let dispatch_q6_k_ok =
            GgufModelLoader::decode_tensor(&q6k_block, GgufGtype::Q6_K) == Ok(expected_q6k);

        kprintln!(
            "[GGUF Q6_K] decode_q6_k_ok={} dispatch_q6_k_ok={}",
            decode_q6_k_ok,
            dispatch_q6_k_ok
        );
        serial_println!(
            "[GGUF] q6_k_test decode_q6_k_ok={} dispatch_q6_k_ok={}",
            decode_q6_k_ok,
            dispatch_q6_k_ok
        );
    }

    // 5b-7. Test real GGUF Q5_K super-block quantized tensor decoding
    // (Fase 59) - a third K-quant format, structurally much closer to
    // Q4_K than to Q6_K: d/dmin come FIRST (like Q4_K), and scales[12]
    // are 6-bit-packed via the EXACT SAME get_scale_min_k4 Q4_K already
    // uses - reused completely unchanged, not re-derived, since GGML's
    // own dequantize_row_q5_K calls it identically to dequantize_row_
    // q4_K. The only new piece: a 5th bit per value from qh[32], added
    // as +16 to a 4-bit qs nibble (unsigned 0..31, no recentering) -
    // and qh itself never advances across the block's 4 sub-iterations,
    // instead using shifting bit-masks (u1/u2) to pick a different
    // single bit position out of the SAME 32 bytes each time (u1: bits
    // 0,2,4,6; u2: bits 1,3,5,7 - together all 8 bits of each byte).
    // Hand-traced with concrete bytes (qh byte 0xAA, four distinct qs
    // nibble-pair bytes) before writing any Rust - see decode_q5_k's
    // own doc for the full derivation.
    //
    // decode_q5_k_ok reuses Fase 57's own already-verified SCALES array
    // unchanged (get_scale_min_k4 itself didn't change, so its already-
    // proven round trip doesn't need re-deriving) paired with a UNIFORM
    // qh=0xAA (constant across all 32 bytes) and four distinct uniform
    // qs chunks (0xAB/0xCD/0xEF/0x12) - giving 8 predictable sub-block
    // values (11, 26, 13, 28, 15, 30, 2, 17, from low/high nibble +
    // conditional +16) checked against 8 hand-computed dequantized
    // constants.
    kprintln!("[GGUF INFERENCE] Testing Q5_K super-block quantized tensor decoding...");
    {
        const SCALES: [u8; 12] = [135, 142, 213, 220, 120, 113, 42, 35, 195, 90, 225, 120];

        let mut q5k_block: Vec<u8> = Vec::with_capacity(176);
        q5k_block.extend_from_slice(&0x3C00u16.to_le_bytes()); // d = 1.0
        q5k_block.extend_from_slice(&0x3C00u16.to_le_bytes()); // dmin = 1.0
        q5k_block.extend_from_slice(&SCALES);
        q5k_block.extend(core::iter::repeat_n(0xAAu8, 32)); // qh: bits 0,2,4,6=0; 1,3,5,7=1
        q5k_block.extend(core::iter::repeat_n(0xABu8, 32)); // qs chunk 0: low=11 high=10
        q5k_block.extend(core::iter::repeat_n(0xCDu8, 32)); // qs chunk 1: low=13 high=12
        q5k_block.extend(core::iter::repeat_n(0xEFu8, 32)); // qs chunk 2: low=15 high=14
        q5k_block.extend(core::iter::repeat_n(0x12u8, 32)); // qs chunk 3: low=2 high=1

        let decoded = GgufModelLoader::decode_q5_k(&q5k_block);
        let mut expected_q5k: Vec<f32> = Vec::with_capacity(256);
        expected_q5k.extend(core::iter::repeat_n(21.0f32, 32)); // sub0: 7*11-56
        expected_q5k.extend(core::iter::repeat_n(315.0f32, 32)); // sub1: 14*26-49
        expected_q5k.extend(core::iter::repeat_n(231.0f32, 32)); // sub2: 21*13-42
        expected_q5k.extend(core::iter::repeat_n(749.0f32, 32)); // sub3: 28*28-35
        expected_q5k.extend(core::iter::repeat_n(497.0f32, 32)); // sub4: 35*15-28
        expected_q5k.extend(core::iter::repeat_n(1239.0f32, 32)); // sub5: 42*30-21
        expected_q5k.extend(core::iter::repeat_n(84.0f32, 32)); // sub6: 49*2-14
        expected_q5k.extend(core::iter::repeat_n(945.0f32, 32)); // sub7: 56*17-7
        let decode_q5_k_ok = decoded == expected_q5k;

        let dispatch_q5_k_ok =
            GgufModelLoader::decode_tensor(&q5k_block, GgufGtype::Q5_K) == Ok(expected_q5k);

        kprintln!(
            "[GGUF Q5_K] decode_q5_k_ok={} dispatch_q5_k_ok={}",
            decode_q5_k_ok,
            dispatch_q5_k_ok
        );
        serial_println!(
            "[GGUF] q5_k_test decode_q5_k_ok={} dispatch_q5_k_ok={}",
            decode_q5_k_ok,
            dispatch_q5_k_ok
        );
    }

    // 5b-8. Test real GGUF Q3_K super-block quantized tensor decoding
    // (Fase 60) - the fourth K-quant format, and structurally the most
    // intricate yet: 16 sub-blocks of 16 elements each (not 8 of 32),
    // a signed 3-bit value (2 low bits from qs + 1 high bit from
    // hmask, recentered -4), and - genuinely new - a scale-
    // reconstruction scheme that is NOT get_scale_min_k4 (Q3_K has no
    // separate min field, just one signed 6-bit scale per sub-block).
    // GGML's real dequantize_row_q3_K treats scales[12] as three u32
    // words and reconstructs a fourth via a SWAR (SIMD-within-a-
    // register) bit trick - reduced here to a plain per-index formula,
    // NOT trusted from derivation alone: cross-checked by re-
    // implementing GGML's exact packed-word logic in a scratch script
    // and comparing outputs across 2000 random byte inputs before any
    // of this Rust was written. See decode_q3_k's own doc for the full
    // formula and derivation story.
    //
    // decode_q3_k_ok builds one full real 110-byte block: scales
    // chosen (via the verified formula's own inverse) to decode to a
    // full-range arithmetic sequence (3,7,11,...,63, step 4) covering
    // all 16 sub-blocks; hmask alternating 0xAA (first 16 bytes) /
    // 0x55 (second 16, the exact inverse bit pattern) so every one of
    // the 8 total shifting-mask positions differs between the two
    // halves; qs uniform 0xE4 (whose four 2-bit fields are exactly
    // 0,1,2,3, the same convenient byte Q6_K's own test already used) -
    // giving 16 predictable sub-block values, independently confirmed
    // via the same scratch-script cross-check as the scale formula
    // itself, not just hand arithmetic.
    kprintln!("[GGUF INFERENCE] Testing Q3_K super-block quantized tensor decoding...");
    {
        const RAW_SCALES: [u8; 12] = [
            0x33, 0x77, 0xBB, 0xFF, 0x33, 0x77, 0xBB, 0xFF, 0xE4, 0xE4, 0xE4, 0xE4,
        ];

        let mut q3k_block: Vec<u8> = Vec::with_capacity(110);
        q3k_block.extend(core::iter::repeat_n(0xAAu8, 16)); // hmask[0..16)
        q3k_block.extend(core::iter::repeat_n(0x55u8, 16)); // hmask[16..32)
        q3k_block.extend(core::iter::repeat_n(0xE4u8, 64)); // qs[64]
        q3k_block.extend_from_slice(&RAW_SCALES);
        q3k_block.extend_from_slice(&0x3C00u16.to_le_bytes()); // d = 1.0, LAST

        let decoded = GgufModelLoader::decode_q3_k(&q3k_block);
        let mut expected_q3k: Vec<f32> = Vec::with_capacity(256);
        expected_q3k.extend(core::iter::repeat_n(116.0f32, 16)); // is=0
        expected_q3k.extend(core::iter::repeat_n(0.0f32, 16)); // is=1
        expected_q3k.extend(core::iter::repeat_n(-21.0f32, 16)); // is=2
        expected_q3k.extend(core::iter::repeat_n(51.0f32, 16)); // is=3
        expected_q3k.extend(core::iter::repeat_n(26.0f32, 16)); // is=4
        expected_q3k.extend(core::iter::repeat_n(-18.0f32, 16)); // is=5
        expected_q3k.extend(core::iter::repeat_n(-15.0f32, 16)); // is=6
        expected_q3k.extend(core::iter::repeat_n(1.0f32, 16)); // is=7
        expected_q3k.extend(core::iter::repeat_n(-12.0f32, 16)); // is=8
        expected_q3k.extend(core::iter::repeat_n(0.0f32, 16)); // is=9
        expected_q3k.extend(core::iter::repeat_n(11.0f32, 16)); // is=10
        expected_q3k.extend(core::iter::repeat_n(-45.0f32, 16)); // is=11
        expected_q3k.extend(core::iter::repeat_n(-38.0f32, 16)); // is=12
        expected_q3k.extend(core::iter::repeat_n(46.0f32, 16)); // is=13
        expected_q3k.extend(core::iter::repeat_n(81.0f32, 16)); // is=14
        expected_q3k.extend(core::iter::repeat_n(-31.0f32, 16)); // is=15
        let decode_q3_k_ok = decoded == expected_q3k;

        let dispatch_q3_k_ok =
            GgufModelLoader::decode_tensor(&q3k_block, GgufGtype::Q3_K) == Ok(expected_q3k);

        kprintln!(
            "[GGUF Q3_K] decode_q3_k_ok={} dispatch_q3_k_ok={}",
            decode_q3_k_ok,
            dispatch_q3_k_ok
        );
        serial_println!(
            "[GGUF] q3_k_test decode_q3_k_ok={} dispatch_q3_k_ok={}",
            decode_q3_k_ok,
            dispatch_q3_k_ok
        );
    }

    // 5b-9. Test real GGUF Q2_K super-block quantized tensor decoding
    // (Fase 61) - the fifth K-quant format, and (after Q3_K's SWAR
    // scale trick) a welcome return to simplicity: no bit-packing
    // scheme at all. Each scales[16] byte holds BOTH a sub-block's
    // scale (low nibble) AND its min (high nibble) directly, unpacked
    // - confirmed against GGML's real dequantize_row_q2_K rather than
    // assumed from the format's name. The quantized value is a plain
    // 2 bits, used directly unsigned (no recentering, no companion
    // high-bit array), dequantized affinely: value = q_2bits*(d*scale)
    // - (dmin*min). The outer n/j/is/shift loop structure is identical
    // to Q3_K's own, just without a second hmask-style array.
    //
    // decode_q2_k_ok uses a scratch-script-verified test (continuing
    // Q3_K's discipline): 16 distinct scale/min pairs (scale=i, min=
    // 15-i for i=0..16, using the full 0-15 nibble range) paired with
    // a uniform qs byte (0xE4, the same convenient byte Q6_K's own
    // test uses) - the 16 resulting values were confirmed by the same
    // scratch script before any Rust was written.
    kprintln!("[GGUF INFERENCE] Testing Q2_K super-block quantized tensor decoding...");
    {
        let mut scales = [0u8; 16];
        for (i, s) in scales.iter_mut().enumerate() {
            *s = (i as u8) | (((15 - i as u8) & 0x0F) << 4);
        }

        let mut q2k_block: Vec<u8> = Vec::with_capacity(84);
        q2k_block.extend_from_slice(&scales);
        q2k_block.extend(core::iter::repeat_n(0xE4u8, 64)); // qs[64]
        q2k_block.extend_from_slice(&0x3C00u16.to_le_bytes()); // d = 1.0
        q2k_block.extend_from_slice(&0x3C00u16.to_le_bytes()); // dmin = 1.0, LAST

        let decoded = GgufModelLoader::decode_q2_k(&q2k_block);
        let expected_blocks: [f32; 16] = [
            -15.0, -14.0, -11.0, -9.0, -3.0, 0.0, 9.0, 13.0, -7.0, -6.0, 5.0, 7.0, 21.0, 24.0,
            41.0, 45.0,
        ];
        let mut expected_q2k: Vec<f32> = Vec::with_capacity(256);
        for v in expected_blocks {
            expected_q2k.extend(core::iter::repeat_n(v, 16));
        }
        let decode_q2_k_ok = decoded == expected_q2k;

        let dispatch_q2_k_ok =
            GgufModelLoader::decode_tensor(&q2k_block, GgufGtype::Q2_K) == Ok(expected_q2k);

        kprintln!(
            "[GGUF Q2_K] decode_q2_k_ok={} dispatch_q2_k_ok={}",
            decode_q2_k_ok,
            dispatch_q2_k_ok
        );
        serial_println!(
            "[GGUF] q2_k_test decode_q2_k_ok={} dispatch_q2_k_ok={}",
            decode_q2_k_ok,
            dispatch_q2_k_ok
        );
    }

    // 5b-8. Test real GGUF Q8_K super-block quantized tensor decoding
    // (Fase 67) - the LAST defined K-quant, and, despite being numbered
    // last, structurally the simplest of the whole family: no bit-
    // packing at all, a plain f32 delta (not f16, unlike every other
    // K-quant), and a full signed byte per value. qs[i] = i as u8 for
    // all 256 values deliberately exercises the ENTIRE i8 range exactly
    // once (0..127 stay positive, 128..255 wrap to -128..-1 when
    // reinterpreted as i8) - a stronger, exhaustive check than any
    // other decoder's test here, since it covers every possible input
    // byte rather than a handful of hand-picked samples. d=2.0 (not 1.0)
    // so a bug that forgot to multiply by d, or that misread d as a
    // 2-byte f16 instead of the real 4-byte f32, would produce visibly
    // wrong output rather than an accidental pass.
    kprintln!("[GGUF INFERENCE] Testing Q8_K super-block quantized tensor decoding...");
    {
        let mut q8k_block: Vec<u8> = Vec::with_capacity(292);
        q8k_block.extend_from_slice(&2.0f32.to_le_bytes()); // d = 2.0, FIRST
        let mut qs = [0u8; 256];
        for (i, q) in qs.iter_mut().enumerate() {
            *q = i as u8;
        }
        q8k_block.extend_from_slice(&qs);
        q8k_block.extend(core::iter::repeat_n(0u8, 32)); // bsums[16], unused

        let decoded = GgufModelLoader::decode_q8_k(&q8k_block);
        let expected_q8k: Vec<f32> = (0..256u32).map(|i| 2.0 * (i as u8 as i8) as f32).collect();
        let decode_q8_k_ok = decoded == expected_q8k;

        let dispatch_q8_k_ok =
            GgufModelLoader::decode_tensor(&q8k_block, GgufGtype::Q8_K) == Ok(expected_q8k);

        kprintln!(
            "[GGUF Q8_K] decode_q8_k_ok={} dispatch_q8_k_ok={}",
            decode_q8_k_ok,
            dispatch_q8_k_ok
        );
        serial_println!(
            "[GGUF] q8_k_test decode_q8_k_ok={} dispatch_q8_k_ok={}",
            decode_q8_k_ok,
            dispatch_q8_k_ok
        );
    }

    // 5c. Test real GGUF multi-tensor-info support (Fase 51) -
    // GgufTensorInfo::parse has returned "how many bytes this one entry
    // consumed" since Fase 43 specifically so several could be parsed in
    // sequence, but nothing before this Fase ever actually looped over
    // more than one - every prior test constructed exactly one tensor.
    // parse_many builds on parse's existing return value without
    // changing it at all. Deliberately disk-independent, same reasoning
    // as the Q8_0 test above - this Fase is scoped to the parsing logic
    // itself, not disk I/O (already proven in Fase 41-43).
    //
    // Constructs a buffer with TWO tensor-info entries back-to-back
    // ("weights" F32, "scale_bias" Q8_0 - deliberately different gtypes,
    // not just a duplicate) followed by both tensors' real data, offsets
    // computed the same way the disk-load test's own MODEL.BIN
    // construction does. parse_many_ok checks both entries' name/
    // dimensions/gtype/offset are exactly right AND that the total
    // consumed byte count lands exactly at the tensor-info section's own
    // end - proof this isn't just "found 2 somethings" but that each
    // entry's own length was tracked correctly across the whole
    // sequence, not just the first one. tensor1/2_decode_ok then feed
    // each parsed entry's OWN gtype into decode_tensor (Fase 50) - a
    // genuinely different claim than Fase 50's own dispatch test (which
    // used hand-built structs): this proves parse_many's real output is
    // directly usable by decode_tensor, not just structurally similar to
    // what it expects.
    kprintln!("[GGUF INFERENCE] Testing GGUF multi-tensor-info support...");
    {
        use gguf_loader::{GgufModelLoader, GgufTensorInfo};

        const NAME1: &str = "weights";
        const NAME2: &str = "scale_bias";
        const TENSOR1_DATA: [f32; 4] = [1.0, -2.0, 3.5, 0.0];

        let mut buf: alloc::vec::Vec<u8> = alloc::vec::Vec::new();
        buf.extend_from_slice(&(NAME1.len() as u32).to_le_bytes());
        buf.extend_from_slice(NAME1.as_bytes());
        buf.extend_from_slice(&2u32.to_le_bytes()); // dim0
        buf.extend_from_slice(&2u32.to_le_bytes()); // dim1
        buf.extend_from_slice(&(GgufGtype::F32 as u32).to_le_bytes());
        let offset1_pos = buf.len();
        buf.extend_from_slice(&0u32.to_le_bytes()); // offset placeholder

        buf.extend_from_slice(&(NAME2.len() as u32).to_le_bytes());
        buf.extend_from_slice(NAME2.as_bytes());
        buf.extend_from_slice(&32u32.to_le_bytes()); // dim0
        buf.extend_from_slice(&1u32.to_le_bytes()); // dim1
        buf.extend_from_slice(&(GgufGtype::Q8_0 as u32).to_le_bytes());
        let offset2_pos = buf.len();
        buf.extend_from_slice(&0u32.to_le_bytes()); // offset placeholder

        let tensor_info_section_len = buf.len();
        buf[offset1_pos..offset1_pos + 4]
            .copy_from_slice(&(tensor_info_section_len as u32).to_le_bytes());

        for v in TENSOR1_DATA {
            buf.extend_from_slice(&v.to_le_bytes());
        }

        let tensor2_data_offset = buf.len();
        buf[offset2_pos..offset2_pos + 4]
            .copy_from_slice(&(tensor2_data_offset as u32).to_le_bytes());

        buf.extend_from_slice(&0x3800u16.to_le_bytes()); // f16 scale = 0.5
        let qs2: [i8; 32] = core::array::from_fn(|i| (i as i8) - 16);
        for &v in &qs2 {
            buf.push(v as u8);
        }

        let parse_many_result = GgufTensorInfo::parse_many(&buf, 2);
        let parse_many_ok = matches!(&parse_many_result, Ok((infos, consumed))
            if infos.len() == 2
            && infos[0].name == NAME1 && infos[0].dimensions == [2, 2]
            && infos[0].gtype == GgufGtype::F32 && infos[0].offset == tensor_info_section_len
            && infos[1].name == NAME2 && infos[1].dimensions == [32, 1]
            && infos[1].gtype == GgufGtype::Q8_0 && infos[1].offset == tensor2_data_offset
            && *consumed == tensor_info_section_len);
        let infos = parse_many_result.map(|(i, _)| i).unwrap_or_default();

        let tensor1_decode_ok = infos
            .first()
            .map(|info| {
                GgufModelLoader::decode_tensor(&buf[info.offset..info.offset + 16], info.gtype)
                    == Ok(TENSOR1_DATA.to_vec())
            })
            .unwrap_or(false);
        let expected2: alloc::vec::Vec<f32> = qs2.iter().map(|&v| v as f32 * 0.5).collect();
        let tensor2_decode_ok = infos
            .get(1)
            .map(|info| {
                GgufModelLoader::decode_tensor(&buf[info.offset..info.offset + 34], info.gtype)
                    == Ok(expected2)
            })
            .unwrap_or(false);

        kprintln!(
            "[GGUF MULTI-TENSOR] parse_many_ok={} tensor1_decode_ok={} tensor2_decode_ok={}",
            parse_many_ok,
            tensor1_decode_ok,
            tensor2_decode_ok
        );
        serial_println!(
            "[GGUF] multi_tensor_test parse_many_ok={} tensor1_decode_ok={} tensor2_decode_ok={}",
            parse_many_ok,
            tensor1_decode_ok,
            tensor2_decode_ok
        );
    }

    // 5c-2. Test real GGUF multi-tensor DISK loading (Fase 56) - the
    // previous test proved parse_many correct against a synthetic
    // in-memory buffer; this proves the same capability through real
    // FAT12 disk I/O for the first time, the exact gap Fase 51's own
    // memory notes flagged as not yet closed. Builds a genuine 2-tensor
    // GGUF file (24-byte header + 2 tensor-info entries, parsed via
    // parse_many, + both tensors' real data - "weights" F32 and
    // "scale_bias" Q8_0, deliberately different gtypes again), writes it
    // to a dedicated real file (distinct from the original single-tensor
    // disk-load test's own MODEL.BIN, so the two tests can never
    // interact), reads it back, then parses and decodes BOTH tensors
    // from the DISK-READ bytes - not the in-memory buffer used to
    // construct them. Each tensor's offset field is relative to the FULL
    // FILE (matching the convention the original single-tensor disk-load
    // test already established), not just the tensor-info section, since
    // parse_many is called on a slice starting right after the header.
    kprintln!("[GGUF INFERENCE] Testing GGUF multi-tensor DISK loading...");
    match shell::find_fat_partition() {
        Ok(partition) => match fat12::read_bpb(&partition) {
            Ok(mut fs) => {
                use gguf_loader::{GgufModelLoader, GgufTensorInfo};

                const T1_NAME: &str = "weights";
                const T2_NAME: &str = "scale_bias";
                const T1_DATA: [f32; 4] = [1.0, -2.0, 3.5, 0.0];

                let mut model_bytes: alloc::vec::Vec<u8> = alloc::vec::Vec::new();
                model_bytes.extend_from_slice(&gguf_loader::GGUF_MAGIC.to_le_bytes());
                model_bytes.extend_from_slice(&3u32.to_le_bytes()); // version
                model_bytes.extend_from_slice(&2u64.to_le_bytes()); // tensor_count = 2
                model_bytes.extend_from_slice(&0u64.to_le_bytes()); // kv_count = 0

                model_bytes.extend_from_slice(&(T1_NAME.len() as u32).to_le_bytes());
                model_bytes.extend_from_slice(T1_NAME.as_bytes());
                model_bytes.extend_from_slice(&2u32.to_le_bytes()); // dim0
                model_bytes.extend_from_slice(&2u32.to_le_bytes()); // dim1
                model_bytes.extend_from_slice(&(GgufGtype::F32 as u32).to_le_bytes());
                let offset1_pos = model_bytes.len();
                model_bytes.extend_from_slice(&0u32.to_le_bytes()); // offset placeholder

                model_bytes.extend_from_slice(&(T2_NAME.len() as u32).to_le_bytes());
                model_bytes.extend_from_slice(T2_NAME.as_bytes());
                model_bytes.extend_from_slice(&32u32.to_le_bytes()); // dim0
                model_bytes.extend_from_slice(&1u32.to_le_bytes()); // dim1
                model_bytes.extend_from_slice(&(GgufGtype::Q8_0 as u32).to_le_bytes());
                let offset2_pos = model_bytes.len();
                model_bytes.extend_from_slice(&0u32.to_le_bytes()); // offset placeholder

                let tensor1_data_offset = model_bytes.len();
                model_bytes[offset1_pos..offset1_pos + 4]
                    .copy_from_slice(&(tensor1_data_offset as u32).to_le_bytes());
                for v in T1_DATA {
                    model_bytes.extend_from_slice(&v.to_le_bytes());
                }

                let tensor2_data_offset = model_bytes.len();
                model_bytes[offset2_pos..offset2_pos + 4]
                    .copy_from_slice(&(tensor2_data_offset as u32).to_le_bytes());
                model_bytes.extend_from_slice(&0x3800u16.to_le_bytes()); // f16 scale = 0.5
                let qs2: [i8; 32] = core::array::from_fn(|i| (i as i8) - 16);
                for &v in &qs2 {
                    model_bytes.push(v as u8);
                }

                let write_ok = fs.create_file("MULTIGF.BIN", &model_bytes).is_ok();
                let read_back = fs.read_file("MULTIGF.BIN");
                let round_trip_ok = read_back.as_deref() == Ok(model_bytes.as_slice());

                let mut header_ok = false;
                let mut parse_many_disk_ok = false;
                let mut tensor1_disk_decode_ok = false;
                let mut tensor2_disk_decode_ok = false;

                if let Ok(full_bytes) = &read_back {
                    header_ok = GgufModelLoader::parse_header(&full_bytes[0..24])
                        .map(|h| h.tensor_count == 2 && h.kv_count == 0)
                        .unwrap_or(false);

                    if let Ok((infos, consumed)) = GgufTensorInfo::parse_many(&full_bytes[24..], 2)
                    {
                        parse_many_disk_ok = infos.len() == 2
                            && infos[0].name == T1_NAME
                            && infos[0].dimensions == [2, 2]
                            && infos[0].gtype == GgufGtype::F32
                            && infos[0].offset == tensor1_data_offset
                            && infos[1].name == T2_NAME
                            && infos[1].dimensions == [32, 1]
                            && infos[1].gtype == GgufGtype::Q8_0
                            && infos[1].offset == tensor2_data_offset
                            && consumed == tensor1_data_offset - 24;

                        tensor1_disk_decode_ok = infos
                            .first()
                            .map(|info| {
                                GgufModelLoader::decode_tensor(
                                    &full_bytes[info.offset..info.offset + 16],
                                    info.gtype,
                                ) == Ok(T1_DATA.to_vec())
                            })
                            .unwrap_or(false);

                        let expected2: alloc::vec::Vec<f32> =
                            qs2.iter().map(|&v| v as f32 * 0.5).collect();
                        tensor2_disk_decode_ok = infos
                            .get(1)
                            .map(|info| {
                                GgufModelLoader::decode_tensor(
                                    &full_bytes[info.offset..info.offset + 34],
                                    info.gtype,
                                ) == Ok(expected2)
                            })
                            .unwrap_or(false);
                    }
                }

                let cleanup_ok = fs.delete_file("MULTIGF.BIN").is_ok();

                kprintln!(
                    "[GGUF MULTI-TENSOR DISK] write_ok={} round_trip_ok={} header_ok={} parse_many_disk_ok={} tensor1_disk_decode_ok={} tensor2_disk_decode_ok={} cleanup_ok={}",
                    write_ok, round_trip_ok, header_ok, parse_many_disk_ok,
                    tensor1_disk_decode_ok, tensor2_disk_decode_ok, cleanup_ok
                );
                serial_println!(
                    "[GGUF] multi_tensor_disk_test write_ok={} round_trip_ok={} header_ok={} parse_many_disk_ok={} tensor1_disk_decode_ok={} tensor2_disk_decode_ok={} cleanup_ok={}",
                    write_ok, round_trip_ok, header_ok, parse_many_disk_ok,
                    tensor1_disk_decode_ok, tensor2_disk_decode_ok, cleanup_ok
                );
            }
            Err(e) => {
                kprintln!("[GGUF] multi-tensor disk test: not FAT12 ({})", e);
                serial_println!("[GGUF] multi_tensor_disk_test -> not fat12: {}", e);
            }
        },
        Err(e) => {
            kprintln!(
                "[GGUF] multi-tensor disk test: couldn't find FAT partition: {}",
                e
            );
            serial_println!("[GGUF] multi_tensor_disk_test -> no partition: {}", e);
        }
    }

    // 6. Test Native VirtIO-Net & TCP/IPv4 Network Stack
    kprintln!("[NET INIT] Initializing VirtIO-Net Hardware Adapter & TCP/IP Stack...");
    NativeNetworkStack::send_ipv4_packet([192, 168, 1, 1], b"AgentOS Kernel Online");

    // 7. Invoke System Calls (Syscalls)
    kprintln!("[KERNEL SYSCALL] Testing Native Agent Syscall Dispatcher...");
    syscall::dispatch_syscall(syscall::SYS_SERIAL_PRINT, 0, 0, 0, false);
    let spawned_pid = syscall::dispatch_syscall(syscall::SYS_AGENT_SPAWN, 10000, 0, 0, false);
    syscall::dispatch_syscall(syscall::SYS_KV_ALLOC, spawned_pid, 1024, 0, false);

    // 7a-2. Test SYS_TENSOR_EVAL (Fase 55) - this syscall number has been
    // defined since before this session's own start, but dispatch_syscall
    // never had a match arm for it at all: calling it always fell through
    // to "Unknown System Call Number", a genuine dead constant, the same
    // class of gap Fase 50 found in GgufTensorInfo's own long-parsed-but-
    // unused gtype field. Deliberately picked as this Fase's own area
    // switch after five straight GGUF Fases (50-54): a small, ring-0-only
    // slice of "make syscalls real" - wiring the dispatcher to the actual
    // tensor engine - distinct from what was then the much larger,
    // still-deferred real ring-3/ring-0 transition, since closed for
    // real by Fase 68-73 (see syscall.rs's own TensorEvalArgs arm, which
    // now validates this exact pointer argument before dereferencing it,
    // per Fase 74).
    //
    // weights/inputs/bias/in_dim/out_dim are a small, fully hand-computable
    // 2x2 layer: row0 = ReLU(1*1 + 2*1 + 1) = ReLU(4) = 4.0; row1 =
    // ReLU(3*1 + 4*1 - 10) = ReLU(-3) = 0.0 - the negative bias on row1 is
    // deliberate, so this test also exercises ReLU's clipping branch (a
    // real negative pre-activation clamped to zero), not just plain
    // addition. syscall_ok proves dispatch_syscall itself returned the
    // real success code (0) for this syscall number, not the "Unknown
    // System Call Number" fallback (u64::MAX) it would have returned
    // before this Fase. tensor_eval_correct proves the syscall path
    // genuinely drove TensorEngine::matmul_layer for real (through the
    // TensorEvalArgs pointer, not a stub) - the actual computed outputs
    // match the hand-computed expected values, not just "some 0 came
    // back".
    kprintln!("[KERNEL SYSCALL] Testing SYS_TENSOR_EVAL (real tensor engine via syscall)...");
    {
        let weights: [f32; 4] = [1.0, 2.0, 3.0, 4.0];
        let inputs: [f32; 2] = [1.0, 1.0];
        let bias: [f32; 2] = [1.0, -10.0];
        let mut outputs: [f32; 2] = [0.0; 2];

        let args = syscall::TensorEvalArgs {
            weights: weights.as_ptr(),
            weights_len: weights.len(),
            inputs: inputs.as_ptr(),
            inputs_len: inputs.len(),
            bias: bias.as_ptr(),
            bias_len: bias.len(),
            outputs: outputs.as_mut_ptr(),
            outputs_len: outputs.len(),
            in_dim: 2,
            out_dim: 2,
        };
        let result = syscall::dispatch_syscall(
            syscall::SYS_TENSOR_EVAL,
            &args as *const _ as u64,
            0,
            0,
            false,
        );
        let syscall_ok = result == 0;

        const EXPECTED_OUTPUTS: [f32; 2] = [4.0, 0.0];
        let tensor_eval_correct = outputs
            .iter()
            .zip(EXPECTED_OUTPUTS.iter())
            .all(|(a, b)| (a - b).abs() < 0.001);

        kprintln!(
            "[SYSCALL] tensor_eval_test: syscall_ok={} tensor_eval_correct={} outputs=[{:.2}, {:.2}]",
            syscall_ok,
            tensor_eval_correct,
            outputs[0],
            outputs[1]
        );
        serial_println!(
            "[SYSCALL] tensor_eval_test syscall_ok={} tensor_eval_correct={} outputs=[{:.2}, {:.2}]",
            syscall_ok,
            tensor_eval_correct,
            outputs[0],
            outputs[1]
        );
    }

    // 7a-3. Test that SYS_TENSOR_EVAL rejects an invalid pointer cleanly
    // (Fase 74), instead of trusting it blindly the way the test right
    // above's own real, valid pointer implicitly could not have caught.
    // Before this Fase, dereferencing this exact pointer would have
    // triggered a real #PF from INSIDE the syscall handler, hitting the
    // existing page_fault_handler, which halts the kernel forever - so
    // this test's own success criterion is as much "the kernel is still
    // running to print this line at all" as it is the printed value
    // itself. 0x1000 is a low, canonical, page-aligned address this
    // kernel never maps (this kernel's own real mappings - the kernel
    // image, the heap at 0x4444..., Fase 70's user page at 0x5555... -
    // are all far above it), chosen the same way `run_ring3_test`'s own
    // `cli` byte was: a deliberately clear-cut case, not a boundary one.
    kprintln!("[KERNEL SYSCALL] Testing SYS_TENSOR_EVAL rejects an unmapped pointer...");
    {
        const DEFINITELY_UNMAPPED: u64 = 0x1000;
        let result =
            syscall::dispatch_syscall(syscall::SYS_TENSOR_EVAL, DEFINITELY_UNMAPPED, 0, 0, false);
        let rejected_cleanly = result == u64::MAX;
        kprintln!(
            "[SYSCALL] tensor_eval_invalid_ptr_test: rejected_cleanly={}",
            rejected_cleanly
        );
        serial_println!(
            "[SYSCALL] tensor_eval_invalid_ptr_test rejected_cleanly={}",
            rejected_cleanly
        );
    }

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

    // 7b-2-2. First real step toward a real VirtIO-net driver (Fase
    // 62) - a second, structurally different real NIC alongside e1000,
    // now reachable because this kernel's boot command adds `-device
    // virtio-net-pci` (confirmed empirically to land at a new PCI slot,
    // 00:04.0, leaving e1000's 00:03.0 completely unaffected). Unlike
    // e1000's memory-mapped BAR0, VirtIO's legacy PCI transport uses an
    // I/O-port BAR0 - real `in`/`out` port reads, not read_volatile
    // through a mapped address. See net/virtio.rs's own module doc for
    // the full register-offset verification story.
    net::virtio::probe();

    // 7b-2-3. Completes the real VirtIO device-init handshake
    // (ACKNOWLEDGE -> DRIVER -> DRIVER_OK) and sets up one real
    // virtqueue (TX, index 1) - Fase 63, the necessary next step
    // before any actual frame can be sent. queue_num=256 (read back,
    // not assumed) needs a 10246-byte vring - 3 physical frames, not
    // 1, the first time this kernel's frame allocator has needed
    // contiguous multi-frame memory (verified explicitly, not
    // assumed - see net/virtio.rs's own doc). pfn_readback matching
    // pfn is the real proof the device accepted the queue's physical
    // address, not just that the write didn't crash.
    kprintln!("[VIRTIO INIT] Setting up TX virtqueue...");
    match net::virtio::init_tx_queue() {
        Ok(info) => {
            let pfn_ok = info.pfn_readback == info.pfn;
            kprintln!(
                "[VIRTIO TXQ] queue_num={} frames_needed={} pfn_ok={} final_status={:#04x}",
                info.queue_num,
                info.frames_needed,
                pfn_ok,
                info.final_status
            );
            serial_println!(
                "[VIRTIO] txq_test queue_num={} frames_needed={} pfn_ok={} final_status={:#04x}",
                info.queue_num,
                info.frames_needed,
                pfn_ok,
                info.final_status
            );

            // 7b-2-4. Builds a real virtio_net_hdr + minimal broadcast
            // frame, places it in the TX queue's avail ring, notifies
            // the device, and polls the used ring for completion - the
            // VirtIO equivalent of net::e1000::send_test_frame's own
            // TDH-advancing proof (Fase 22), though a genuinely
            // different protocol shape (a completion-ring index, not a
            // single hardware register). First real attempt at
            // actually sending data through this second NIC - honestly
            // uncertain going in whether this hits its own version of
            // e1000's own DD-bit mystery (Fase 22->44).
            kprintln!("[VIRTIO INIT] Sending a test frame through the TX queue...");
            match net::virtio::send_test_frame(&info) {
                Ok(send) => {
                    let used_advanced = send.used_idx_after != send.used_idx_before;
                    kprintln!(
                        "[VIRTIO TX] used_idx_before={} used_idx_after={} advanced={} elem_id={} elem_len={}",
                        send.used_idx_before,
                        send.used_idx_after,
                        used_advanced,
                        send.used_elem_id,
                        send.used_elem_len
                    );
                    serial_println!(
                        "[VIRTIO] tx_test used_idx_before={} used_idx_after={} advanced={} elem_id={} elem_len={}",
                        send.used_idx_before,
                        send.used_idx_after,
                        used_advanced,
                        send.used_elem_id,
                        send.used_elem_len
                    );
                }
                Err(e) => {
                    kprintln!("[VIRTIO TX] {}", e);
                    serial_println!("[VIRTIO] tx_test error={}", e);
                }
            }

            // 7b-2-5. Sets up the RX virtqueue (index 0) and arms one
            // real, write-only receive descriptor - the RX equivalent
            // of 7b-2-3's TX queue setup (Fase 63), following the same
            // two-step shape TX itself took (ring setup here, then a
            // real received-frame proof as separate follow-on work).
            // desc_write_flag_ok confirms the descriptor's flags word
            // reads back VRING_DESC_F_WRITE, not just that the write
            // didn't crash - the one genuinely new piece beyond TX's
            // own descriptor (which uses flags=0, read-only).
            kprintln!("[VIRTIO INIT] Setting up RX virtqueue...");
            match net::virtio::init_rx_queue(&info) {
                Ok(rx) => {
                    let rx_pfn_ok = rx.pfn_readback == rx.pfn;
                    let desc_write_flag_ok = rx.desc_flags_readback == 2;
                    kprintln!(
                        "[VIRTIO RXQ] queue_num={} frames_needed={} pfn_ok={} desc_write_flag_ok={} avail_idx_after={}",
                        rx.queue_num,
                        rx.frames_needed,
                        rx_pfn_ok,
                        desc_write_flag_ok,
                        rx.avail_idx_after
                    );
                    serial_println!(
                        "[VIRTIO] rxq_test queue_num={} frames_needed={} pfn_ok={} desc_write_flag_ok={} avail_idx_after={}",
                        rx.queue_num,
                        rx.frames_needed,
                        rx_pfn_ok,
                        desc_write_flag_ok,
                        rx.avail_idx_after
                    );

                    // 7b-2-6. Fase 66: sends a real ARP request (the
                    // same byte layout net::e1000::send_test_frame
                    // already proved SLIRP answers) and polls the RX
                    // queue armed just above for a genuine reply - the
                    // first complete, real VirtIO-net TX+RX round trip
                    // this kernel has attempted, matching
                    // net::e1000's own Fase 45.
                    kprintln!(
                        "[VIRTIO INIT] Sending a real ARP request and waiting for SLIRP's reply..."
                    );
                    match net::virtio::receive_test_frame(&info, &rx) {
                        Ok(recv) => {
                            kprintln!(
                                "[VIRTIO RX] received {} bytes, is_arp_reply={} gateway_ip={:?}",
                                recv.received_len,
                                recv.is_arp_reply,
                                recv.gateway_ip
                            );
                            serial_println!(
                                "[VIRTIO] rx_test used_idx_before={} used_idx_after={} received_len={} is_arp_reply={} gateway_ip={:?}",
                                recv.used_idx_before,
                                recv.used_idx_after,
                                recv.received_len,
                                recv.is_arp_reply,
                                recv.gateway_ip
                            );
                        }
                        Err(e) => {
                            kprintln!("[VIRTIO RX] {}", e);
                            serial_println!("[VIRTIO] rx_test error={}", e);
                        }
                    }
                }
                Err(e) => {
                    kprintln!("[VIRTIO RXQ] {}", e);
                    serial_println!("[VIRTIO] rxq_test error={}", e);
                }
            }
        }
        Err(e) => {
            kprintln!("[VIRTIO TXQ] {}", e);
            serial_println!("[VIRTIO] txq_test error={}", e);
        }
    }

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

    // 7b-6. Test real IPv4 + ICMP echo (ping) protocol construction and
    // parsing (net::icmp, Fase 47) - pure byte-buffer logic, deliberately
    // NOT wired to e1000's TX/RX yet (that needs a real destination MAC,
    // i.e. ARP resolution, which doesn't exist yet either - separate,
    // larger scope). Proves the primitive is correct in isolation first,
    // the same order Fase 21's frame allocator or Fase 37/38's VFAT
    // entry-building both used before anything downstream depended on
    // them.
    //
    // checksum_even_case_ok/checksum_odd_case_ok are hand-computed
    // vectors, NOT round-trip checks - a round-trip test (build then
    // parse with the SAME checksum function) would still pass even if
    // this code consistently disagreed with the real RFC 1071 standard,
    // since both sides would agree with each other while disagreeing
    // with any real peer (e.g. QEMU's SLIRP) computing it independently.
    // [0x00,0x01,0x00,0x02]: two words 0x0001+0x0002=0x0003, checksum =
    // !0x0003 = 0xFFFC. [0x00,0x01,0xFF]: word 0x0001 plus an odd
    // trailing byte padded as a word's high byte (0xFF00) -> sum=0xFF01,
    // checksum = !0xFF01 = 0x00FE - specifically exercises the odd-byte
    // padding path, the subtlest part of the algorithm.
    //
    // request/reply_fields_ok round-trips a full echo message through
    // build_icmp_echo -> parse_icmp_echo (a 33-byte odd-length payload,
    // so the whole 41-byte message is also odd-length end to end - the
    // same padding path exercised directly above, now in the full
    // integration path too). corrupted_checksum_rejected is the actual
    // proof this isn't a rubber stamp: one payload byte is XOR-flipped
    // after building a valid reply, and parse_icmp_echo must now report a
    // mismatch instead of silently accepting corrupted data.
    kprintln!("[KERNEL INIT] Testing real IPv4 + ICMP echo (ping) construction/parsing...");
    {
        use net::icmp;

        let checksum_even_case_ok = icmp::internet_checksum(&[0x00, 0x01, 0x00, 0x02]) == 0xFFFC;
        let checksum_odd_case_ok = icmp::internet_checksum(&[0x00, 0x01, 0xFF]) == 0x00FE;

        const PING_DATA: &[u8] = b"AgentOS ping self-test payload!!!";
        let request = icmp::build_icmp_echo(false, 0x1234, 1, PING_DATA);
        let request_fields_ok = icmp::parse_icmp_echo(&request)
            .map(|info| !info.is_reply && info.identifier == 0x1234 && info.sequence == 1)
            .unwrap_or(false);

        let reply = icmp::build_icmp_echo(true, 0x1234, 1, PING_DATA);
        let reply_fields_ok = icmp::parse_icmp_echo(&reply)
            .map(|info| info.is_reply && info.identifier == 0x1234 && info.sequence == 1)
            .unwrap_or(false);

        let mut corrupted = reply.clone();
        let last = corrupted.len() - 1;
        corrupted[last] ^= 0xFF;
        let corrupted_checksum_rejected = icmp::parse_icmp_echo(&corrupted).is_err();

        let ipv4_header = icmp::build_ipv4_header(
            1,
            64,
            icmp::IP_PROTOCOL_ICMP,
            [10, 0, 2, 15],
            [10, 0, 2, 2],
            request.len(),
        );
        let ipv4_checksum_valid = icmp::checksum_is_valid(&ipv4_header);

        kprintln!(
            "[ICMP] ping self-test: checksum_even_case_ok={} checksum_odd_case_ok={} request_fields_ok={} reply_fields_ok={} corrupted_checksum_rejected={} ipv4_checksum_valid={}",
            checksum_even_case_ok, checksum_odd_case_ok, request_fields_ok, reply_fields_ok,
            corrupted_checksum_rejected, ipv4_checksum_valid
        );
        serial_println!(
            "[ICMP] ping_selftest checksum_even_case_ok={} checksum_odd_case_ok={} request_fields_ok={} reply_fields_ok={} corrupted_checksum_rejected={} ipv4_checksum_valid={}",
            checksum_even_case_ok, checksum_odd_case_ok, request_fields_ok, reply_fields_ok,
            corrupted_checksum_rejected, ipv4_checksum_valid
        );
    }

    // 7b-6b. Test real TCP header construction/parsing + the RFC 793
    // pseudo-header checksum (net::tcp, Fase 88) - pure byte-buffer
    // logic, deliberately NOT wired to any real connection state machine
    // or device I/O yet, the same order net::icmp itself followed
    // (Fase 47 before Fase 48-49's real ARP/ping round trips).
    //
    // pseudo_checksum_hand_computed_ok is an INDEPENDENTLY hand-computed
    // vector, NOT a round-trip check - the exact discipline net::icmp's
    // own self-test above already established (see its own comment):
    // a round-trip test (build then parse with the SAME checksum
    // function) would still pass even if this code consistently
    // disagreed with the real RFC 793/1071 standard, since both sides
    // would agree with each other while disagreeing with any real peer
    // computing it independently. Pseudo-header [0,0,0,1, 0,0,0,2, 0,6,
    // 0,4] (source IP 0.0.0.1, dest IP 0.0.0.2, protocol 6, TCP length 4)
    // + a fake 4-byte "segment" [0x00,0x01,0x00,0x02]: 8 words summing to
    // 0+1+0+2+6+4+1+2=0x0010, checksum = !0x0010 = 0xFFEF (verified
    // independently before writing this assertion, not assumed).
    //
    // syn_fields_ok round-trips a real 20-byte SYN header (build_tcp_
    // header -> parse_tcp_segment) and checks every field survived
    // exactly. corrupted_checksum_rejected is the actual proof this
    // isn't a rubber stamp: one header byte is XOR-flipped after
    // building a valid segment, and parse_tcp_segment must now report a
    // mismatch instead of silently accepting corrupted data.
    kprintln!("[KERNEL INIT] Testing real TCP header construction/parsing...");
    {
        use net::tcp;

        const SRC_IP: [u8; 4] = [10, 0, 2, 15];
        const DST_IP: [u8; 4] = [10, 0, 2, 2];

        let pseudo_checksum_hand_computed_ok = {
            let source_ip = [0, 0, 0, 1];
            let dest_ip = [0, 0, 0, 2];
            let fake_segment: [u8; 4] = [0x00, 0x01, 0x00, 0x02];
            let mut buf = alloc::vec::Vec::with_capacity(12 + fake_segment.len());
            buf.extend_from_slice(&source_ip);
            buf.extend_from_slice(&dest_ip);
            buf.push(0);
            buf.push(tcp::IP_PROTOCOL_TCP);
            buf.extend_from_slice(&(fake_segment.len() as u16).to_be_bytes());
            buf.extend_from_slice(&fake_segment);
            net::icmp::internet_checksum(&buf) == 0xFFEF
        };

        let syn_header = tcp::build_tcp_header(
            SRC_IP,
            DST_IP,
            54321,
            80,
            0x1000_0000,
            0,
            tcp::TCP_FLAG_SYN,
            65535,
            &[],
        );
        let syn_fields_ok = tcp::parse_tcp_segment(SRC_IP, DST_IP, &syn_header)
            .map(|info| {
                info.source_port == 54321
                    && info.dest_port == 80
                    && info.seq_num == 0x1000_0000
                    && info.ack_num == 0
                    && info.flags == tcp::TCP_FLAG_SYN
                    && info.window == 65535
            })
            .unwrap_or(false);

        let mut corrupted = syn_header;
        corrupted[0] ^= 0xFF;
        let corrupted_checksum_rejected =
            tcp::parse_tcp_segment(SRC_IP, DST_IP, &corrupted).is_err();

        kprintln!(
            "[TCP] header self-test: pseudo_checksum_hand_computed_ok={} syn_fields_ok={} corrupted_checksum_rejected={}",
            pseudo_checksum_hand_computed_ok, syn_fields_ok, corrupted_checksum_rejected
        );
        serial_println!(
            "[TCP] header_selftest pseudo_checksum_hand_computed_ok={} syn_fields_ok={} corrupted_checksum_rejected={}",
            pseudo_checksum_hand_computed_ok, syn_fields_ok, corrupted_checksum_rejected
        );
    }

    // Fase 107: test real DNS answer-record parsing (net::dns) - closes
    // the exact gap net::e1000::dns_query_test's own doc (Fase 106) left
    // open: that test only ever confirmed a genuine reply exists
    // (transaction ID + QR bit), never extracted a real value FROM it.
    // Pure logic, no hardware dependency, the same "protocol layer
    // first" order the ICMP/TCP self-tests right above already used -
    // important since this kernel's own local dev environment observed
    // zero answer records in Fase 106's own real run (only the real CI
    // runner's own network path returned real records), so this pure
    // self-test is the ONLY way to verify the parsing logic itself on
    // this machine at all.
    //
    // a_record_parsed_ok round-trips a realistic, hand-built fake DNS
    // reply (header + a 17-byte "example.com" question + one real,
    // compressed-name A-record answer) through parse_first_a_record.
    // zero_answers_rejected/non_a_record_rejected/malformed_name_rejected
    // each mutate exactly ONE field of that same fixture and confirm
    // parse_first_a_record now correctly REJECTS it - the actual proof
    // this isn't a rubber stamp, the same discipline the ICMP/TCP
    // self-tests above already established with their own
    // corrupted-checksum cases. multi_answer_ok (Fase 115) and
    // uncompressed_name_parsed_ok (Fase 117) are separate, dedicated
    // fixtures each proving one further real capability.
    kprintln!("[KERNEL INIT] Testing real DNS answer-record parsing...");
    {
        use net::dns;

        const QUESTION_LEN: usize = 17;
        #[rustfmt::skip]
        const FAKE_REPLY: [u8; 45] = [
            // header: transaction ID, flags, QDCOUNT=1, ANCOUNT=1, NSCOUNT=0, ARCOUNT=0
            0x12, 0x34, 0x81, 0x80, 0x00, 0x01, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00,
            // question: "example.com" A/IN (17 bytes)
            0x07, b'e', b'x', b'a', b'm', b'p', b'l', b'e', 0x03, b'c', b'o', b'm', 0x00,
            0x00, 0x01, 0x00, 0x01,
            // answer: NAME=compression pointer to offset 12, TYPE=A, CLASS=IN,
            // TTL=600, RDLENGTH=4, RDATA=192.0.2.1 (RFC 5737 TEST-NET-1)
            0xC0, 0x0C, 0x00, 0x01, 0x00, 0x01, 0x00, 0x00, 0x02, 0x58, 0x00, 0x04, 192, 0, 2, 1,
        ];
        const EXPECTED_IP: [u8; 4] = [192, 0, 2, 1];

        let a_record_parsed_ok =
            dns::parse_first_a_record(&FAKE_REPLY, QUESTION_LEN) == Ok(EXPECTED_IP);

        let mut zero_answers = FAKE_REPLY;
        zero_answers[7] = 0x00; // ANCOUNT = 0
        let zero_answers_rejected = dns::parse_first_a_record(&zero_answers, QUESTION_LEN).is_err();

        let mut non_a_record = FAKE_REPLY;
        non_a_record[31] = 0x05; // TYPE = 5 (CNAME), not 1 (A)
        let non_a_record_rejected = dns::parse_first_a_record(&non_a_record, QUESTION_LEN).is_err();

        // Fase 117: `parse_first_a_record` no longer rejects EVERY
        // non-pointer NAME byte outright - an uncompressed label
        // sequence is now a real, supported shape (see below). This
        // case now proves a genuinely MALFORMED one is still rejected:
        // a label-length byte (63, the max single-label length) that
        // claims far more bytes than remain in the 45-byte fixture.
        let mut malformed_name = FAKE_REPLY;
        malformed_name[29] = 0x3F; // label length 63 - runs off the end of FAKE_REPLY
        let malformed_name_rejected =
            dns::parse_first_a_record(&malformed_name, QUESTION_LEN).is_err();

        // Fase 115: the real, previously-unexamined case - this kernel's
        // own real CI runs have observed answer_count=2 for "example.com"
        // since Fase 106, yet this function had never actually been
        // proven correct for more than a single answer record. Same
        // question section as FAKE_REPLY, but ANCOUNT=2: a CNAME answer
        // FIRST (RDLENGTH=2, an arbitrary 2-byte RDATA - its own content
        // doesn't matter, only its declared length, since skipping past
        // it correctly is exactly what's being tested), then a real A
        // record SECOND. Proves parse_first_a_record genuinely walks
        // past the non-A record using ITS OWN RDLENGTH rather than
        // assuming the first answer already is the one it wants.
        #[rustfmt::skip]
        const MULTI_ANSWER_REPLY: [u8; 59] = [
            // header: transaction ID, flags, QDCOUNT=1, ANCOUNT=2, NSCOUNT=0, ARCOUNT=0
            0x12, 0x34, 0x81, 0x80, 0x00, 0x01, 0x00, 0x02, 0x00, 0x00, 0x00, 0x00,
            // question: "example.com" A/IN (17 bytes) - same as FAKE_REPLY
            0x07, b'e', b'x', b'a', b'm', b'p', b'l', b'e', 0x03, b'c', b'o', b'm', 0x00,
            0x00, 0x01, 0x00, 0x01,
            // answer 1: NAME=compression pointer, TYPE=CNAME(5), CLASS=IN,
            // TTL=600, RDLENGTH=2, RDATA=an arbitrary 2-byte compression
            // pointer (its content is irrelevant - only its LENGTH matters,
            // to prove the skip-past-it arithmetic is correct)
            0xC0, 0x0C, 0x00, 0x05, 0x00, 0x01, 0x00, 0x00, 0x02, 0x58, 0x00, 0x02, 0xC0, 0x0C,
            // answer 2: NAME=compression pointer, TYPE=A, CLASS=IN,
            // TTL=600, RDLENGTH=4, RDATA=192.0.2.42 (RFC 5737 TEST-NET-1)
            0xC0, 0x0C, 0x00, 0x01, 0x00, 0x01, 0x00, 0x00, 0x02, 0x58, 0x00, 0x04, 192, 0, 2, 42,
        ];
        const EXPECTED_MULTI_IP: [u8; 4] = [192, 0, 2, 42];
        let multi_answer_ok =
            dns::parse_first_a_record(&MULTI_ANSWER_REPLY, QUESTION_LEN) == Ok(EXPECTED_MULTI_IP);

        // Fase 117: the real positive case - an answer record whose own
        // NAME is fully spelled out ("example.com", the same 13 wire
        // bytes `encode_qname` would build) rather than a compression
        // pointer. Same question section as FAKE_REPLY/MULTI_ANSWER_REPLY;
        // a deliberately distinct RDATA (203.0.113.5, RFC 5737 TEST-NET-3)
        // so this fixture's own IP is unmistakable in the log line.
        #[rustfmt::skip]
        const UNCOMPRESSED_NAME_REPLY: [u8; 56] = [
            // header: transaction ID, flags, QDCOUNT=1, ANCOUNT=1, NSCOUNT=0, ARCOUNT=0
            0x12, 0x34, 0x81, 0x80, 0x00, 0x01, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00,
            // question: "example.com" A/IN (17 bytes)
            0x07, b'e', b'x', b'a', b'm', b'p', b'l', b'e', 0x03, b'c', b'o', b'm', 0x00,
            0x00, 0x01, 0x00, 0x01,
            // answer: NAME=fully spelled out "example.com" (13 bytes, NOT
            // a compression pointer), TYPE=A, CLASS=IN, TTL=600,
            // RDLENGTH=4, RDATA=203.0.113.5
            0x07, b'e', b'x', b'a', b'm', b'p', b'l', b'e', 0x03, b'c', b'o', b'm', 0x00,
            0x00, 0x01, 0x00, 0x01, 0x00, 0x00, 0x02, 0x58, 0x00, 0x04, 203, 0, 113, 5,
        ];
        const EXPECTED_UNCOMPRESSED_IP: [u8; 4] = [203, 0, 113, 5];
        let uncompressed_name_parsed_ok =
            dns::parse_first_a_record(&UNCOMPRESSED_NAME_REPLY, QUESTION_LEN)
                == Ok(EXPECTED_UNCOMPRESSED_IP);

        kprintln!(
            "[DNS] answer-parse self-test: a_record_parsed_ok={} zero_answers_rejected={} non_a_record_rejected={} malformed_name_rejected={} multi_answer_ok={} uncompressed_name_parsed_ok={}",
            a_record_parsed_ok, zero_answers_rejected, non_a_record_rejected, malformed_name_rejected, multi_answer_ok, uncompressed_name_parsed_ok
        );
        serial_println!(
            "[DNS] answer_parse_selftest a_record_parsed_ok={} zero_answers_rejected={} non_a_record_rejected={} malformed_name_rejected={} multi_answer_ok={} uncompressed_name_parsed_ok={}",
            a_record_parsed_ok, zero_answers_rejected, non_a_record_rejected, malformed_name_rejected, multi_answer_ok, uncompressed_name_parsed_ok
        );
    }

    // Fase 110: test real DNS query building (net::dns::build_query /
    // encode_qname) - the encoding-side mirror of the answer-parsing
    // self-test right above, closing the gap dns_query_test's own doc
    // used to flag (a fixed "example.com" query was the only content it
    // could ever send, since there was no general-purpose builder yet).
    // Pure logic, no hardware dependency, same "protocol layer first"
    // order.
    //
    // valid_query_matches_known_bytes rebuilds Fase 106's own original
    // hand-built 29-byte "A? example.com" query (transaction ID 0x1234)
    // through the new general-purpose builder and confirms the output
    // is byte-for-byte identical - the actual proof this is a real
    // generalization, not just a different-shaped function that happens
    // to also produce SOME query. The other three cases each confirm a
    // genuinely invalid hostname is rejected rather than silently
    // mis-encoded, the same discipline the answer-parse self-test above
    // already established with its own corrupted-fixture cases.
    kprintln!("[KERNEL INIT] Testing real DNS query building (net::dns::build_query)...");
    {
        use net::dns;

        #[rustfmt::skip]
        const EXPECTED_EXAMPLE_COM_QUERY: [u8; 29] = [
            0x12, 0x34, // transaction ID
            0x01, 0x00, // flags: standard query, recursion desired
            0x00, 0x01, // QDCOUNT = 1
            0x00, 0x00, // ANCOUNT = 0
            0x00, 0x00, // NSCOUNT = 0
            0x00, 0x00, // ARCOUNT = 0
            0x07, b'e', b'x', b'a', b'm', b'p', b'l', b'e', // "example"
            0x03, b'c', b'o', b'm', // "com"
            0x00, // QNAME terminator
            0x00, 0x01, // QTYPE = A
            0x00, 0x01, // QCLASS = IN
        ];
        let valid_query_matches_known_bytes =
            dns::build_query(0x1234, "example.com") == Ok(EXPECTED_EXAMPLE_COM_QUERY.to_vec());

        let empty_hostname_rejected = dns::build_query(0x1234, "").is_err();

        let over_long_label = "a".repeat(64);
        let over_long_label_rejected = dns::build_query(0x1234, &over_long_label).is_err();

        let empty_label_rejected = dns::build_query(0x1234, "a..b").is_err();

        kprintln!(
            "[DNS] query-build self-test: valid_query_matches_known_bytes={} empty_hostname_rejected={} over_long_label_rejected={} empty_label_rejected={}",
            valid_query_matches_known_bytes, empty_hostname_rejected, over_long_label_rejected, empty_label_rejected
        );
        serial_println!(
            "[DNS] build_query_selftest valid_query_matches_known_bytes={} empty_hostname_rejected={} over_long_label_rejected={} empty_label_rejected={}",
            valid_query_matches_known_bytes, empty_hostname_rejected, over_long_label_rejected, empty_label_rejected
        );
    }

    // 7b-7. Test real, generalized ARP resolution (Fase 48) - arp_resolve
    // sends a real ARP request for an arbitrary target IP and resolves its
    // MAC via the same TX+RX mechanics send_test_frame/receive_test_frame
    // already proved, instead of only ever asking about SLIRP's hardcoded
    // gateway. Resolves that exact same gateway (10.0.2.2) here specifically
    // because its real MAC is already known from Fase 45's own captured
    // data (52:55:0a:00:02:02, SLIRP's virtual gateway) - matches_known_
    // gateway_mac being true is a precise, exact correctness check, not
    // just "got 6 bytes back from somewhere".
    kprintln!("[KERNEL INIT] Testing generalized ARP resolution (arp_resolve)...");
    match net::e1000::arp_resolve([10, 0, 2, 2]) {
        Ok(mac) => {
            const KNOWN_GATEWAY_MAC: [u8; 6] = [0x52, 0x55, 0x0a, 0x00, 0x02, 0x02];
            let matches_known_gateway_mac = mac == KNOWN_GATEWAY_MAC;
            kprintln!(
                "[E1000] arp_resolve(10.0.2.2) -> {:02x?} (matches known gateway MAC: {})",
                mac,
                matches_known_gateway_mac
            );
            serial_println!(
                "[E1000] arp_resolve_test target=10.0.2.2 mac={:02x?} matches_known_gateway_mac={}",
                mac,
                matches_known_gateway_mac
            );
        }
        Err(e) => {
            kprintln!("[E1000] arp_resolve(10.0.2.2): {}", e);
            serial_println!("[E1000] arp_resolve_test -> FAILED: {}", e);
        }
    }

    // 7b-8. Test a real, complete `ping` (Fase 49) - converges arp_resolve
    // (Fase 48, resolves the destination MAC) and net::icmp (Fase 47,
    // builds/parses the actual IPv4+ICMP bytes) into the first complete,
    // real, routable IP packet this kernel has ever assembled and sent -
    // not just a raw Ethernet-level test frame. Pings the same known
    // gateway (10.0.2.2) arp_resolve just resolved. Honestly uncertain
    // going in whether SLIRP actually answers ICMP echo (never attempted
    // before this Fase) - see net::e1000::ping's own doc for why this is
    // expected to work, not assumed.
    kprintln!("[KERNEL INIT] Testing a real, complete ping (net::e1000::ping)...");
    match net::e1000::ping([10, 0, 2, 2]) {
        Ok(()) => {}
        Err(e) => {
            kprintln!("[E1000] ping(10.0.2.2): {}", e);
            serial_println!("[E1000] ping_test -> FAILED: {}", e);
        }
    }

    // 7b-9. Test the new `ping` shell command (Fase 49) - the same "prove
    // the API, then prove the shell path" pattern this session used for
    // netcheck (Fase 46).
    shell::dispatch_command("ping 10.0.2.2");

    // 7b-9b. Test the first real TCP round trip this kernel has ever
    // attempted (Fase 89, net::e1000::tcp_syn_test) - sends a genuine
    // SYN to a `guestfwd` target (see boot_kernel.bat/kernel-ci.yml's
    // own `-netdev ...,guestfwd=tcp:10.0.2.100:9999-cmd:cat`, added this
    // same Fase and verified in isolation to cause zero regression
    // before becoming a standing part of the boot command) and polls
    // for a genuine, fully-validated SYN-ACK reply - QEMU's own SLIRP
    // terminates the connection with its own real TCP stack, entirely
    // locally, with zero dependency on real network reachability, the
    // same testing philosophy `arp_resolve`/`ping` already established
    // against SLIRP's own fixed gateway. Honestly uncertain going in
    // whether this actually produces a real SYN-ACK (never attempted
    // before this Fase, and `guestfwd` itself was new research this
    // Fase) - see net::e1000::tcp_syn_test's own doc for why this is
    // expected to work, not assumed.
    kprintln!(
        "[KERNEL INIT] Testing a real TCP SYN/SYN-ACK round trip (net::e1000::tcp_syn_test)..."
    );
    match net::e1000::tcp_syn_test([10, 0, 2, 100], 9999) {
        Ok(_) => {}
        Err(e) => {
            kprintln!("[E1000] tcp_syn_test(10.0.2.100:9999): {}", e);
            serial_println!("[E1000] tcp_syn_test_result -> FAILED: {}", e);
        }
    }

    // 7b-9c. Fase 90: complete the handshake tcp_syn_test deliberately
    // left open (its own doc comment: "does NOT yet send the final
    // ACK ... or track any ongoing connection state") and prove the
    // resulting connection is genuinely usable - a real final ACK, a
    // real PSH|ACK data segment, and (since the guestfwd target is
    // `cat`) a genuine echoed reply carrying the exact same bytes back.
    // See net::e1000::tcp_echo_test's own doc for the full sequence and
    // why a distinct source port from tcp_syn_test's own self-test
    // avoids any leftover half-open-handshake ambiguity.
    kprintln!(
        "[KERNEL INIT] Testing TCP handshake completion + real data echo (net::e1000::tcp_echo_test)..."
    );
    match net::e1000::tcp_echo_test(
        [10, 0, 2, 100],
        9999,
        54322,
        0x3000_0000,
        b"AgentOS Fase90 TCP echo\n",
    ) {
        Ok(_) => {}
        Err(e) => {
            kprintln!("[E1000] tcp_echo_test(10.0.2.100:9999): {}", e);
            serial_println!("[E1000] tcp_echo_test_result -> FAILED: {}", e);
        }
    }

    // Fase 106: the first real UDP round trip this kernel has ever
    // attempted (every prior network Fase built TCP or ICMP). Targets
    // SLIRP's own built-in DNS proxy (10.0.2.3:53) since guestfwd was
    // directly confirmed NOT to support UDP - see net::e1000::dns_query_
    // test's own doc for the full reasoning. Genuinely uncertain whether
    // real DNS resolution succeeds in this environment - only sent=true
    // is deterministic; reply_received/qr_bit_set/answer_count are
    // logged honestly rather than assumed.
    kprintln!(
        "[KERNEL INIT] Testing real UDP round trip via SLIRP's DNS proxy (net::e1000::dns_query_test)..."
    );
    match net::e1000::dns_query_test([10, 0, 2, 3], "example.com") {
        Ok(_) => {}
        Err(e) => {
            kprintln!("[E1000] dns_query_test(10.0.2.3:53): {}", e);
            serial_println!("[E1000] dns_query_test_result -> FAILED: {}", e);
        }
    }

    // Fase 114: a real boot-time self-test for resolve_hostname (Fase
    // 113's own reusable wrapper API) - closes the exact gap that
    // Fase's own honest self-correction identified: resolve_hostname
    // was previously reachable ONLY through the interactive `ping`
    // shell command, so CI's own boot sequence never actually exercised
    // it at all (confirmed by checking the real job log's own `ping`
    // lines, which showed nothing new). This calls the wrapper directly
    // rather than through `ping`, so a real ICMP echo to whatever
    // "example.com" resolves to is deliberately NOT attempted here -
    // that would add a genuinely new, separate real-world dependency
    // (does this CI runner's own sandbox even permit outbound ICMP to
    // the public internet?) this Fase does not need to answer to verify
    // resolve_hostname's own logic, which only needs dns_query_test's
    // already-proven UDP round trip (confirmed working on real CI since
    // Fase 106) underneath it. Only the fully deterministic "attempted"
    // fact is asserted in CI; the real resolved address is logged
    // honestly, the same discipline every other DNS Fase already uses.
    kprintln!("[KERNEL INIT] Testing net::e1000::resolve_hostname (Fase 113's own wrapper API)...");
    serial_println!("[E1000] resolve_hostname_test attempting=true hostname=example.com dns_server=[10, 0, 2, 3]");
    match net::e1000::resolve_hostname("example.com", [10, 0, 2, 3]) {
        Ok(ip) => {
            kprintln!("[E1000] resolve_hostname(\"example.com\") -> {:?}", ip);
            serial_println!(
                "[E1000] resolve_hostname_test hostname=example.com resolved_ip={:?}",
                ip
            );
        }
        Err(e) => {
            kprintln!("[E1000] resolve_hostname(\"example.com\"): {}", e);
            serial_println!("[E1000] resolve_hostname_test_result -> FAILED: {}", e);
        }
    }

    // Fase 108: a SECOND, sequential call to the same, completely
    // unchanged tcp_echo_test (Fase 90/102) - zero risk to that already-
    // proven, CI-asserted function, since nothing about it changes here.
    // Passes the EXACT SAME 54322/0x30000000 src_port/initial_seq this
    // call always used (a hardcoded internal constant at the time this
    // Fase was written, generalized to real parameters in Fase 112,
    // updated here to keep passing those identical values explicitly) -
    // a genuinely NEW connection reusing the same local port and
    // initial sequence number, immediately after the first call's own
    // full close. This is a real, previously-untested scenario against
    // SLIRP's real TCP stack (does it allow immediate
    // same-4-tuple reuse, or does some TIME_WAIT-like state reject a
    // second SYN too soon?) - the smallest safe step toward the "more
    // than one connection" item the TCP-connection-abstraction thread
    // has flagged open since Fase 102, without building any new
    // abstraction machinery at all.
    //
    // tcp_echo_test's own success line is IDENTICAL text on both calls
    // (same target/port baked into its own format string) - only
    // echo_len would differ, since this call's own payload is a
    // different length. Explicitly logs second_connection_ok=true in
    // the Ok(_) branch (new code here, not a change to tcp_echo_test
    // itself) so a genuine second success is unambiguous in the log,
    // not merely inferred from counting repeated identical lines.
    //
    // Genuinely uncertain going in, the same honest posture Fase 89/90/
    // 102/106/107 themselves used: cannot be verified locally at all
    // (the same fork()-on-Windows limitation already blocks the FIRST
    // call from ever reaching a real handshake on this machine) - only
    // the real CI runner can show whether SLIRP genuinely allows this.
    kprintln!(
        "[KERNEL INIT] Testing a SECOND TCP connection reusing the same local port right after the first one's own close (net::e1000::tcp_echo_test again)..."
    );
    serial_println!("[E1000] tcp_reconnect_test attempting=true target=[10, 0, 2, 100] port=9999");
    match net::e1000::tcp_echo_test(
        [10, 0, 2, 100],
        9999,
        54322,
        0x3000_0000,
        b"AgentOS Fase108 second connection reusing the same port\n",
    ) {
        Ok(_) => {
            kprintln!("[E1000] tcp_reconnect_test: second connection succeeded");
            serial_println!(
                "[E1000] tcp_reconnect_test target=[10, 0, 2, 100] port=9999 second_connection_ok=true"
            );
        }
        Err(e) => {
            kprintln!("[E1000] tcp_reconnect_test(10.0.2.100:9999): {}", e);
            serial_println!("[E1000] tcp_reconnect_test_result -> FAILED: {}", e);
        }
    }

    // Fase 112: a THIRD, sequential tcp_echo_test call - this time with
    // a genuinely DIFFERENT 4-tuple (src_port 54323, distinct from both
    // tcp_syn_test's own 54321 and tcp_echo_test's own former hardcoded
    // default 54322 the two calls above still use; initial_seq
    // 0x4000_0000, distinct from their own 0x3000_0000) rather than
    // Fase 108's own same-port reuse. Only possible now that Fase 112
    // itself turned src_port/initial_seq into real parameters - the
    // smallest safe step structurally necessary before real CONCURRENT
    // connections could ever be attempted (two connections literally
    // cannot coexist on the same 4-tuple, so proving a second, distinct
    // port/sequence pair also works correctly is a real prerequisite,
    // even though this call is still sequential, not simultaneous).
    //
    // Genuinely uncertain going in, the same honest posture every real
    // network Fase in this arc has used: cannot be verified locally at
    // all (the same fork()-on-Windows limitation blocks every call in
    // this sequence from ever reaching a real handshake on this
    // machine) - only the real CI runner can show whether a genuinely
    // different port/sequence pair also completes successfully.
    kprintln!(
        "[KERNEL INIT] Testing a THIRD TCP connection with a genuinely different local port/sequence (net::e1000::tcp_echo_test again)..."
    );
    serial_println!(
        "[E1000] tcp_multiport_test attempting=true target=[10, 0, 2, 100] port=9999 src_port=54323"
    );
    match net::e1000::tcp_echo_test(
        [10, 0, 2, 100],
        9999,
        54323,
        0x4000_0000,
        b"AgentOS Fase112 third connection, different port\n",
    ) {
        Ok(_) => {
            kprintln!("[E1000] tcp_multiport_test: third connection succeeded");
            serial_println!(
                "[E1000] tcp_multiport_test target=[10, 0, 2, 100] port=9999 src_port=54323 third_connection_ok=true"
            );
        }
        Err(e) => {
            kprintln!("[E1000] tcp_multiport_test(10.0.2.100:9999): {}", e);
            serial_println!("[E1000] tcp_multiport_test_result -> FAILED: {}", e);
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

    // Fase 80: closes a real gap Fase 79 exposed - runs ring-0 preemption
    // and a real ring-3 program CONCURRENTLY for the first time, proving
    // they coexist safely now that scheduler::preemptive::tick() skips
    // task-switching entirely for any tick that interrupts ring-3 code.
    // Same safe-return mechanism, same "runs unconditionally" reasoning
    // as the ring-3 tests earlier in this boot sequence. Deliberately
    // placed AFTER run_preemptive_demo (the last thing above to spawn
    // PCB entries): this test's own start_demo call spawns 2 more, and
    // running it any earlier would shift every subsequent PID by 2 -
    // exactly what happened the first time this was placed earlier,
    // breaking the hardcoded PID numbers several CI assertions above
    // depend on (task-alpha/bravo/charlie, preempt-task-0/1) - a real,
    // CI-caught regression, fixed by moving the call, not the numbers.
    kprintln!(
        "[KERNEL INIT] Testing ring-0 preemption and a real ring-3 program running concurrently..."
    );
    let ring3_concurrent_code = ring3::run_ring3_concurrent_preemption_test();
    kprintln!(
        "[KERNEL INIT] Back from ring-3 - concurrent preemption test returned exit_code={}",
        ring3_concurrent_code
    );

    // Fase 81: proves a ring-3 "task" can be entered via the SAME
    // switch_to/prepare_initial_stack bootstrap the ring-0 schedulers use,
    // instead of enter_ring3's own bespoke synchronous call/return - the
    // necessary foundation genuine mid-flight ring-3 preemption (real
    // separate follow-on work) will build on. Spawns no PCB entries of its
    // own (unlike Fase 80's test), so - unlike that one - there's no
    // hardcoded-PID ordering constraint on where this call goes.
    kprintln!(
        "[KERNEL INIT] Testing a ring-3 task entered via the switch_to bootstrap (not enter_ring3)..."
    );
    let ring3_switchto_code = ring3::run_ring3_switchto_bootstrap_test();
    kprintln!(
        "[KERNEL INIT] Back from ring-3 - switch_to-bootstrap test returned exit_code={}",
        ring3_switchto_code
    );

    // Fase 83: ring-3 tasks cooperatively interleaving via a new
    // voluntary yield vector (0x83) - the lower-risk stepping stone
    // toward genuine ring-3 scheduling this kernel's own ring-0 history
    // already validated (cooperative before preemptive). Zero changes to
    // the timer interrupt path; reuses Fase 81's own switch_to-bootstrap
    // mechanism entirely. Spawns no PCB entries either, same as Fase 81.
    // Fase 94 generalized this from exactly 2 hardcoded tasks to 3.
    kprintln!(
        "[KERNEL INIT] Testing 3 ring-3 tasks cooperatively interleaving via a round-robin yield vector..."
    );
    let (ring3_coop_code, ring3_coop_sig) = ring3::run_ring3_cooperative_test();
    kprintln!(
        "[KERNEL INIT] Back from ring-3 - cooperative test returned exit_code={} signature={:02x?}",
        ring3_coop_code,
        ring3_coop_sig
    );

    // Fase 95: proves ring3_coop_yield_helper's own new priority field is
    // real, not cosmetic - the cooperative-yield equivalent of Fase 92's
    // own ring3_mt priority test, but with a genuinely different safety
    // argument since this mechanism is voluntary, not timer-driven - see
    // ring3::run_priority_cooperative_test's own doc.
    kprintln!(
        "[KERNEL INIT] Testing REAL priority in the cooperative-yield mechanism (task0=High vs task1/task2=Background)..."
    );
    let (priority_coop_code, priority_coop_sig) = ring3::run_priority_cooperative_test();
    kprintln!(
        "[KERNEL INIT] Back from ring-3 - priority_cooperative_test returned exit_code={} signature={:02x?}",
        priority_coop_code,
        priority_coop_sig
    );

    // Fase 96: proves a NON-task-0 cooperative-yield task can voluntarily
    // retire for good instead of yielding-and-being-abandoned - the
    // cooperative-mechanism equivalent of Fase 93's own ring3_mt early-
    // exit test, completing the mirror of all 3 generalizations (N-task,
    // priority, early-exit) onto both ring-3 scheduling mechanisms this
    // arc has built - see ring3::run_early_exit_cooperative_test's own
    // doc.
    kprintln!(
        "[KERNEL INIT] Testing voluntary early exit in the cooperative-yield mechanism (task1 retires via int 0x85 instead of yielding)..."
    );
    let (
        early_exit_coop_code,
        early_exit_coop_sig,
        early_exit_coop_t0_done,
        early_exit_coop_t1_done,
        early_exit_coop_t2_done,
    ) = ring3::run_early_exit_cooperative_test();
    kprintln!(
        "[KERNEL INIT] Back from ring-3 - early_exit_cooperative_test returned exit_code={} signature={:02x?} task0_done={} task1_done={} task2_done={}",
        early_exit_coop_code,
        early_exit_coop_sig,
        early_exit_coop_t0_done,
        early_exit_coop_t1_done,
        early_exit_coop_t2_done
    );

    // Fase 85: the first genuine, INVOLUNTARY (timer-driven) ring-3
    // preemption this kernel has ever attempted - a real hardware tick
    // saves this ring-3 program's COMPLETE register state and correctly
    // restores it, not just the 6 registers switch_to/Fase 83's own
    // cooperative yield already protect. Deliberately the simplest
    // possible proof (save+restore round trip only, no "run something
    // else while preempted" yet) - see scheduler::ring3_preempt's own
    // module doc for the full mechanism and why it's scoped this way.
    kprintln!("[KERNEL INIT] Testing genuine timer-driven full-register ring-3 preemption...");
    let (ring3_full_preempt_code, ring3_full_preempt_hit) = ring3::run_ring3_full_preempt_test();
    kprintln!(
        "[KERNEL INIT] Back from ring-3 - full-register preemption test returned exit_code={:#x} intercepted={}",
        ring3_full_preempt_code,
        ring3_full_preempt_hit
    );

    // Fase 86: the real, larger step named as the necessary follow-on
    // above - actually running a DIFFERENT ring-3 program in the gap a
    // preempted one leaves behind, not just resuming the SAME one. See
    // scheduler::ring3_mt's own module doc for the round-robin
    // mechanism and ring3::run_ring3_mt_test's own doc for the full
    // 3-task test design (Fase 91 generalized this from a hardcoded
    // pair).
    kprintln!("[KERNEL INIT] Testing genuine multi-task ring-3 scheduling (3 DIFFERENT programs alternating via the timer)...");
    let (ring3_mt_task0, ring3_mt_others, ring3_mt_switches) = ring3::run_ring3_mt_test();
    kprintln!(
        "[KERNEL INIT] Back from ring-3 - multi-task test returned task0_checksum={:#x} task1_last_eax={:#x} task2_last_eax={:#x} switch_count={}",
        ring3_mt_task0,
        ring3_mt_others[0],
        ring3_mt_others[1],
        ring3_mt_switches
    );

    // Fase 92: proves scheduler::ring3_mt's own new priority field
    // (added this same Fase, mirroring Fase 87's ring-0 equivalent) is
    // real, not cosmetic - see ring3::run_priority_ring3_mt_test's own
    // doc for why task 0 must be the HIGH-priority one here (this
    // module has no independent tick budget of its own; letting some
    // OTHER task out-prioritize task 0 forever would hang the boot).
    kprintln!(
        "[KERNEL INIT] Testing REAL priority in ring3_mt (High vs Background, genuinely unequal)..."
    );
    let (priority_mt_task0, priority_mt_task1, priority_mt_task2, priority_mt_switches) =
        ring3::run_priority_ring3_mt_test();
    kprintln!(
        "[KERNEL INIT] Back from ring-3 - priority_mt_test returned task0_checksum={:#x} task1_last_eax={:#x} task2_last_eax={:#x} switch_count={}",
        priority_mt_task0,
        priority_mt_task1,
        priority_mt_task2,
        priority_mt_switches
    );

    // Fase 93: proves a NON-task-0 ring3_mt task can voluntarily retire
    // early (int 0x84) instead of spinning forever being the only
    // alternative to task 0's own int 0x81 exit - see ring3::run_early_
    // exit_ring3_mt_test's own doc. Safe to place here (not touching the
    // ps-visible PCB table at all, same reasoning that let Fase 92's own
    // test go here too) - only the Fase 87 ring-0 priority test below
    // must stay absolutely last, since it's the one that spawns new PCB
    // entries.
    kprintln!(
        "[KERNEL INIT] Testing voluntary early exit in ring3_mt (task1 retires via int 0x84 instead of spinning forever)..."
    );
    let (
        early_exit_task0,
        early_exit_task1_eax,
        early_exit_task1_done,
        early_exit_task2_eax,
        early_exit_task2_done,
        early_exit_switches,
    ) = ring3::run_early_exit_ring3_mt_test();
    kprintln!(
        "[KERNEL INIT] Back from ring-3 - early_exit_mt_test returned task0_checksum={:#x} task1_last_eax={:#x} task1_done={} task2_last_eax={:#x} task2_done={} switch_count={}",
        early_exit_task0,
        early_exit_task1_eax,
        early_exit_task1_done,
        early_exit_task2_eax,
        early_exit_task2_done,
        early_exit_switches
    );

    // Fase 105: proves a NON-task-0 ring3_mt task can voluntarily YIELD
    // (int 0x86) - give up its turn while remaining eligible - rather
    // than only ever being switched away involuntarily by the timer or
    // permanently via int 0x84's own RETIRE (the test right above this
    // one). See ring3::run_voluntary_yield_ring3_mt_test's own doc. Safe
    // to place here for the same reason every other ring3_mt-family test
    // in this block is: no PCB entry spawned, only the ring-0 priority
    // test below must stay absolutely last.
    kprintln!(
        "[KERNEL INIT] Testing voluntary yield in ring3_mt (task1 yields via int 0x86, should resume and keep running, then retires via int 0x84)..."
    );
    let (
        voluntary_yield_task0,
        voluntary_yield_task1_eax,
        voluntary_yield_task1_done,
        voluntary_yield_task2_eax,
        voluntary_yield_task2_done,
        voluntary_yield_switches,
    ) = ring3::run_voluntary_yield_ring3_mt_test();
    kprintln!(
        "[KERNEL INIT] Back from ring-3 - voluntary_yield_mt_test returned task0_checksum={:#x} task1_last_eax={:#x} task1_done={} task2_last_eax={:#x} task2_done={} switch_count={}",
        voluntary_yield_task0,
        voluntary_yield_task1_eax,
        voluntary_yield_task1_done,
        voluntary_yield_task2_eax,
        voluntary_yield_task2_done,
        voluntary_yield_switches
    );

    // Fase 98: proves a ring-3 program's machine code can flow through
    // the real FAT12 filesystem (write, read back, delete) and still
    // execute correctly, not just that a kernel-embedded byte array can -
    // see ring3::run_ring3_disk_loaded_test's own doc. Spawns no PCB
    // entry (a synchronous enter_ring3 call, same as Fase 73's own exit
    // test), so it's safe to run here, before the PID-sensitive test
    // below.
    let (
        disk_loaded_roundtrip_ok,
        disk_loaded_exit_code,
        disk_loaded_data_load_verified,
        disk_loaded_data_write_verified,
    ) = ring3::run_ring3_disk_loaded_test();
    kprintln!(
        "[KERNEL INIT] Back from ring-3 - disk_loaded_test roundtrip_ok={} exit_code={} data_load_verified={} data_write_verified={}",
        disk_loaded_roundtrip_ok,
        disk_loaded_exit_code,
        disk_loaded_data_load_verified,
        disk_loaded_data_write_verified
    );

    // Fase 87: proves the preemptive scheduler's own priority field is
    // now genuinely used, not cosmetic - see scheduler::preemptive::
    // run_priority_preemptive_test's own doc. Deliberately placed as the
    // VERY LAST test before the final banner: this is the first NEW
    // PCB-spawning test since Fase 80's own regression (see that Fase's
    // own fix in this file's history) - any spawn() call shifts every
    // LATER-spawned task's PID, so nothing may come after this one.
    let (priority_task0_delta, priority_task1_delta) =
        scheduler::preemptive::run_priority_preemptive_test();
    kprintln!(
        "[KERNEL INIT] Back from priority test - task0_delta={} task1_delta={}",
        priority_task0_delta,
        priority_task1_delta
    );

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
