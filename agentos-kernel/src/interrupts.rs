use crate::{gdt, kprintln, serial_println};
use core::sync::atomic::{AtomicU64, Ordering};
use lazy_static::lazy_static;
use pic8259::ChainedPics;
use spin::Mutex;
use x86_64::registers::control::Cr2;
use x86_64::structures::idt::{InterruptDescriptorTable, InterruptStackFrame, PageFaultErrorCode};
use x86_64::{PrivilegeLevel, VirtAddr};

/// Monotonic count of real timer ticks since boot. A simple counter, so a
/// plain `AtomicU64` (unlike the context-switch code's `saved_rsp` fields)
/// is fine here - there's no "restore a stale value" hazard, just an
/// increment nothing else contends with in a way that matters.
static TIMER_TICKS: AtomicU64 = AtomicU64::new(0);

pub fn timer_ticks() -> u64 {
    TIMER_TICKS.load(Ordering::Relaxed)
}

/// The legacy 8259 PIC defaults to delivering IRQ0-7 on vectors 0x08-0x0F,
/// which collide head-on with CPU exceptions (0x08 is literally our
/// double-fault vector). Remapping both PICs to start at 0x20 (32) is what
/// makes it safe to ever call `interrupts::enable()` - without this, the
/// very first timer tick would look like a double fault to the CPU.
pub const PIC_1_OFFSET: u8 = 32;
pub const PIC_2_OFFSET: u8 = PIC_1_OFFSET + 8;

pub static PICS: Mutex<ChainedPics> =
    Mutex::new(unsafe { ChainedPics::new(PIC_1_OFFSET, PIC_2_OFFSET) });

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum InterruptIndex {
    Timer = PIC_1_OFFSET,
    Keyboard,
}

impl InterruptIndex {
    fn as_u8(self) -> u8 {
        self as u8
    }
}

/// IRQ14, the primary ATA channel's completion interrupt - PIC_2_OFFSET+6
/// (IRQ8 is the slave PIC's first line, so IRQ14 is its 6th). `ata.rs`
/// only ever does polling PIO reads and disables interrupts while doing
/// so, but the drive raises this anyway the moment a command finishes,
/// and it stays *pending* at the PIC until interrupts are re-enabled -
/// leaving it with no handler at all faulted (straight to double-fault,
/// not even a catchable #GP) the first time this code ran a real read.
pub const ATA_PRIMARY_IRQ_VECTOR: u8 = PIC_2_OFFSET + 6;

/// The classic Unix/Linux x86 software-interrupt syscall vector - chosen
/// specifically because it's a genuinely well-known, deliberate convention
/// (not an arbitrary pick), and sits far from every other vector this
/// kernel already uses (PIC remapped to 32-47, `ATA_PRIMARY_IRQ_VECTOR`=46 -
/// see `init_pics`'s own doc). This is the first real piece of the ring-3
/// transition arc (Fase 68's own `gdt.rs` work) that's actually
/// INVOCABLE with a lowered privilege level, once ring-3 code exists to
/// invoke it: the gate itself is registered below with `set_privilege_
/// level(Ring3)`, unlike every other handler in this file (which default
/// to DPL=0, meaning ring-3 code attempting `int` on any of THEM would
/// itself take a #GP - by design, not an oversight, since none of those
/// are meant to be a controlled entry point into the kernel).
pub const SYSCALL_INT_VECTOR: u8 = 0x80;

/// A second, dedicated DPL=3 vector (Fase 73) - a ring-3 program's
/// voluntary "I'm done" signal, deliberately separate from
/// `SYSCALL_INT_VECTOR` rather than another `dispatch_syscall` number:
/// its handler (`ring3::ring3_exit_entry_asm`) does something
/// structurally different from every other handler in this file - it
/// never returns to whatever invoked it, it switches to a completely
/// different, previously-saved kernel stack instead. Keeping that
/// firmly separate from the normal, resuming-as-usual syscall path (one
/// dedicated vector each) is simpler to reason about than one handler
/// that sometimes resumes ring-3 and sometimes doesn't, branching on a
/// magic syscall number.
pub const RING3_EXIT_INT_VECTOR: u8 = 0x81;

/// Counts real invocations of the DPL=3 syscall gate - lets a self-test
/// verify the gate genuinely fired (not just "the CPU didn't crash"),
/// the same reasoning `TIMER_TICKS` already established for verifying
/// the timer IRQ actually happened.
static SYSCALL_INT_COUNT: AtomicU64 = AtomicU64::new(0);

pub fn syscall_int_count() -> u64 {
    SYSCALL_INT_COUNT.load(Ordering::Relaxed)
}

lazy_static! {
    static ref IDT: InterruptDescriptorTable = {
        let mut idt = InterruptDescriptorTable::new();
        idt.breakpoint.set_handler_fn(breakpoint_handler);
        idt.page_fault.set_handler_fn(page_fault_handler);
        idt.general_protection_fault
            .set_handler_fn(general_protection_fault_handler);
        idt.divide_error.set_handler_fn(divide_error_handler);
        idt.invalid_opcode.set_handler_fn(invalid_opcode_handler);
        unsafe {
            idt.double_fault
                .set_handler_fn(double_fault_handler)
                .set_stack_index(gdt::DOUBLE_FAULT_IST_INDEX);
        }
        idt[InterruptIndex::Timer.as_u8()].set_handler_fn(timer_interrupt_handler);
        idt[InterruptIndex::Keyboard.as_u8()].set_handler_fn(keyboard_interrupt_handler);
        idt[ATA_PRIMARY_IRQ_VECTOR].set_handler_fn(ata_primary_interrupt_handler);
        // DPL=3, unlike every entry above (all default to DPL=0) - the
        // one deliberate exception, since this is a real, controlled
        // entry point FROM ring-3 code (proven for real in Fase 72, via
        // ring3.rs's own run_ring3_syscall_test), not just another
        // kernel-internal handler. Wired via `set_handler_addr`, NOT
        // `set_handler_fn` - see `syscall_entry_asm`'s own doc for why:
        // this needs raw register access `extern "x86-interrupt"`
        // handlers can't provide.
        unsafe {
            idt[SYSCALL_INT_VECTOR]
                .set_handler_addr(VirtAddr::new(syscall_entry_asm as *const () as u64))
                .set_privilege_level(PrivilegeLevel::Ring3);
        }
        // Same reasoning as SYSCALL_INT_VECTOR above (DPL=3, raw
        // set_handler_addr) - see RING3_EXIT_INT_VECTOR's own doc for
        // why this is a second, dedicated vector rather than folded
        // into the syscall dispatcher above.
        unsafe {
            idt[RING3_EXIT_INT_VECTOR]
                .set_handler_addr(VirtAddr::new(
                    crate::ring3::ring3_exit_entry_asm as *const () as u64,
                ))
                .set_privilege_level(PrivilegeLevel::Ring3);
        }
        idt
    };
}

pub fn init_idt() {
    IDT.load();
    kprintln!("[IDT] Native IDT Loaded into CPU Register (LIDT)");
    serial_println!("[IDT] Native IDT Loaded into CPU Register (LIDT)");
    kprintln!("[IDT] Handlers armed: breakpoint, double-fault(IST), page-fault, GPF, divide-error, invalid-opcode, timer(IRQ0), keyboard(IRQ1), ata-primary(IRQ14), syscall-int(0x80,DPL=3)");
}

/// Remaps the 8259 PIC to `PIC_1_OFFSET..PIC_2_OFFSET+8` and unmasks IRQs.
/// Must run after `init_idt()` (so vectors 32/33 already have handlers) and
/// before `x86_64::instructions::interrupts::enable()`.
pub fn init_pics() {
    unsafe { PICS.lock().initialize() };
    kprintln!(
        "[PIC] 8259 remapped to vectors {}-{} (was 8-15, collided with CPU exceptions)",
        PIC_1_OFFSET,
        PIC_2_OFFSET + 7
    );
    serial_println!(
        "[PIC] 8259 remapped to vectors {}-{}",
        PIC_1_OFFSET,
        PIC_2_OFFSET + 7
    );
}

extern "x86-interrupt" fn timer_interrupt_handler(_stack_frame: InterruptStackFrame) {
    TIMER_TICKS.fetch_add(1, Ordering::Relaxed);

    // EOI must happen before `preemptive::tick()`, which may switch to a
    // different context and never return from *this* call - if we hadn't
    // acknowledged the interrupt yet, the PIC would think it's still in
    // service and could withhold further same/lower-priority IRQs.
    unsafe {
        PICS.lock()
            .notify_end_of_interrupt(InterruptIndex::Timer.as_u8());
    }

    crate::scheduler::preemptive::tick();
}

extern "x86-interrupt" fn keyboard_interrupt_handler(_stack_frame: InterruptStackFrame) {
    use x86_64::instructions::port::Port;

    let mut data_port: Port<u8> = Port::new(0x60);
    let scancode: u8 = unsafe { data_port.read() };
    crate::keyboard::handle_scancode(scancode);

    unsafe {
        PICS.lock()
            .notify_end_of_interrupt(InterruptIndex::Keyboard.as_u8());
    }
}

/// `ata.rs` only ever polls status registers - it never waits on this
/// interrupt - but the drive raises it anyway, so it needs *a* handler to
/// exist even though this one has nothing useful to do beyond
/// acknowledging it. IRQ8-15 are chained through the slave PIC, so
/// `notify_end_of_interrupt` must (and does, via `pic8259`) EOI both the
/// slave and the master - skipping the master would leave it thinking
/// the slave's cascade line is still busy, blocking IRQ8-13/15 too.
extern "x86-interrupt" fn ata_primary_interrupt_handler(_stack_frame: InterruptStackFrame) {
    unsafe {
        PICS.lock().notify_end_of_interrupt(ATA_PRIMARY_IRQ_VECTOR);
    }
}

/// Real syscall entry point for vector 0x80 (Fase 72) - registered
/// directly via `Entry::set_handler_addr` above, NOT `set_handler_fn`.
/// Every OTHER handler in this file uses `extern "x86-interrupt"`, a
/// compiler-generated wrapper that ONLY ever exposes the pushed
/// `InterruptStackFrame` - it never exposes the general-purpose
/// registers (RAX/RDI/RSI/RDX) a real syscall convention uses to carry
/// the call number and arguments. That was exactly the limitation
/// Fase 69's own first attempt at this gate left deliberately
/// unresolved. A `#[unsafe(naked)]` function has NO compiler-generated
/// prologue/epilogue at all - every register save/restore and the
/// final `iretq` are hand-written below, the same category of risk as
/// `context_switch.rs`'s own asm, worked through with the same care.
///
/// Preserves ALL 15 general-purpose registers other than the CPU-
/// managed interrupt frame (deliberately more conservative than the
/// real Linux/SysV syscall convention, which only guarantees callee-
/// saved registers survive) - simplest possible contract for a ring-3
/// caller to rely on, and there's no reason yet to want the extra
/// scratch registers a laxer contract would free up.
///
/// Push order is deliberately rax/rdi/rsi/rdx LAST (closest to the
/// current stack top once all 15 are pushed), landing them at the
/// simplest possible offsets (0/8/16/24) - chosen specifically to make
/// the offset arithmetic below easy to verify by hand rather than
/// error-prone. After all 15 pushes, RSP is 16-byte aligned (interrupt
/// frame: 5 pushes = 40 bytes; here: 15 pushes = 120 bytes; 160 bytes
/// total is an exact multiple of 16) - exactly satisfying the SysV
/// ABI's "RSP must be 16-aligned immediately before `call`" requirement
/// for the `call {handler}` below, verified by hand before writing this
/// rather than assumed.
///
/// Fase 75 reads one more thing before calling into Rust: the CPU's own
/// interrupt frame - pushed BEFORE any of these 15 registers, so it sits
/// right above them, starting at offset 120 (15 * 8) from the current
/// RSP - always has `RIP` as its first field and `CS` as its second,
/// REGARDLESS of whether this was a same-privilege entry (3 fields:
/// `RIP`/`CS`/`RFLAGS`, e.g. the ring-0 self-test's own bare `int 0x80`)
/// or a privilege-elevating one (5 fields, adding `RSP`/`SS` - a real
/// ring-3 call): either way `CS` is always the 2nd field, at offset
/// `120 + 8 = 128`. Its low 2 bits are the CPU's own Requested Privilege
/// Level at the moment `int 0x80` fired - reading them tells
/// `handle_real_syscall` whether this call genuinely came from ring-3 or
/// ring-0, real information `dispatch_syscall` never had access to
/// before this Fase.
#[unsafe(naked)]
extern "C" fn syscall_entry_asm() {
    core::arch::naked_asm!(
        "push rbx",
        "push rcx",
        "push rbp",
        "push r8",
        "push r9",
        "push r10",
        "push r11",
        "push r12",
        "push r13",
        "push r14",
        "push r15",
        "push rdx",
        "push rsi",
        "push rdi",
        "push rax",
        // SysV call args (rdi, rsi, rdx, rcx) <- saved (rax, rdi, rsi, rdx),
        // i.e. sys_nr, arg1, arg2, arg3 - matching dispatch_syscall's own
        // parameter order. Safe to clobber the live rdi/rsi/rdx/rcx here:
        // their original values are already saved on the stack above.
        "mov rdi, [rsp]",
        "mov rsi, [rsp + 8]",
        "mov rdx, [rsp + 16]",
        "mov rcx, [rsp + 24]",
        "mov r8, [rsp + 128]",
        "and r8, 3",
        "call {handler}",
        // Overwrite the saved-rax slot with the real return value, so the
        // `pop rax` below hands it back to the ring-3 (or ring-0 self-
        // test) caller in rax, exactly where a syscall return value belongs.
        "mov [rsp], rax",
        "pop rax",
        "pop rdi",
        "pop rsi",
        "pop rdx",
        "pop r15",
        "pop r14",
        "pop r13",
        "pop r12",
        "pop r11",
        "pop r10",
        "pop r9",
        "pop r8",
        "pop rbp",
        "pop rcx",
        "pop rbx",
        "iretq",
        handler = sym handle_real_syscall,
    );
}

/// Called by `syscall_entry_asm` with the caller's real RAX/RDI/RSI/RDX -
/// the syscall number and first three arguments, in that order, matching
/// `syscall::dispatch_syscall`'s own parameter order exactly - plus,
/// since Fase 75, the caller's own CS.RPL at the moment `int 0x80` fired
/// (0 or 3), read directly from the CPU's own interrupt frame by
/// `syscall_entry_asm` itself (see that function's own doc for exactly
/// where). A normal (non-naked) `extern "C" fn`, so this is exactly as
/// safe to write as any other Rust code in this codebase - all the
/// raw-register/stack-layout risk is confined to `syscall_entry_asm`
/// itself, above.
///
/// Deliberately does NOT yet pass `caller_rpl` into `dispatch_syscall`
/// or use it to change any behavior - this Fase's own scope is proving
/// the value is read correctly, not yet acting on it. Enforcing that a
/// ring-3 caller may only pass pointers to memory it could otherwise
/// access (tightening `memory::paging::pointer_is_mapped`'s own
/// Fase 74 doc, which named exactly this as real, separate follow-on
/// work) is real, separate follow-on work, not crammed into this Fase.
extern "C" fn handle_real_syscall(
    sys_nr: u64,
    arg1: u64,
    arg2: u64,
    arg3: u64,
    caller_rpl: u64,
) -> u64 {
    SYSCALL_INT_COUNT.fetch_add(1, Ordering::Relaxed);
    let ret = crate::syscall::dispatch_syscall(sys_nr, arg1, arg2, arg3);
    serial_println!(
        "[SYSCALL] real_syscall_from_ring3 sys_nr={} arg1={} caller_rpl={} returned={}",
        sys_nr,
        arg1,
        caller_rpl,
        ret
    );
    ret
}

/// Non-fatal: a debugger/test breakpoint (int3). Execution continues after this.
extern "x86-interrupt" fn breakpoint_handler(stack_frame: InterruptStackFrame) {
    kprintln!(
        "[EXCEPTION] Breakpoint at {:#x}",
        stack_frame.instruction_pointer.as_u64()
    );
    serial_println!("[EXCEPTION] Breakpoint\n{:#?}", stack_frame);
}

/// Fatal: only fires when a second exception occurs while handling the first
/// (e.g. a page-fault handler itself faults). Runs on its own IST stack
/// (see gdt.rs) because the normal stack may already be corrupt at this point.
extern "x86-interrupt" fn double_fault_handler(
    stack_frame: InterruptStackFrame,
    _error_code: u64,
) -> ! {
    serial_println!("[FATAL] DOUBLE FAULT\n{:#?}", stack_frame);
    kprintln!("[FATAL] DOUBLE FAULT - Kernel Halted");
    loop {
        x86_64::instructions::hlt();
    }
}

extern "x86-interrupt" fn page_fault_handler(
    stack_frame: InterruptStackFrame,
    error_code: PageFaultErrorCode,
) {
    let fault_addr = Cr2::read();
    kprintln!("[EXCEPTION] Page Fault accessing {:?}", fault_addr);
    serial_println!(
        "[EXCEPTION] Page Fault at {:?}, code {:?}\n{:#?}",
        fault_addr,
        error_code,
        stack_frame
    );
    kprintln!("[EXCEPTION] Kernel Halted (no recovery yet - see Fase 4 paging work)");
    loop {
        x86_64::instructions::hlt();
    }
}

extern "x86-interrupt" fn general_protection_fault_handler(
    stack_frame: InterruptStackFrame,
    error_code: u64,
) {
    kprintln!(
        "[EXCEPTION] General Protection Fault, error code {}",
        error_code
    );
    serial_println!("[EXCEPTION] GPF code {}\n{:#?}", error_code, stack_frame);
    loop {
        x86_64::instructions::hlt();
    }
}

extern "x86-interrupt" fn divide_error_handler(stack_frame: InterruptStackFrame) {
    kprintln!("[EXCEPTION] Divide Error (division by zero)");
    serial_println!("[EXCEPTION] Divide Error\n{:#?}", stack_frame);
    loop {
        x86_64::instructions::hlt();
    }
}

extern "x86-interrupt" fn invalid_opcode_handler(stack_frame: InterruptStackFrame) {
    kprintln!(
        "[EXCEPTION] Invalid Opcode at {:#x}",
        stack_frame.instruction_pointer.as_u64()
    );
    serial_println!("[EXCEPTION] Invalid Opcode\n{:#?}", stack_frame);
    loop {
        x86_64::instructions::hlt();
    }
}
