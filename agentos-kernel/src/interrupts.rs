use crate::{gdt, kprintln, serial_println};
use core::sync::atomic::{AtomicU64, Ordering};
use lazy_static::lazy_static;
use pic8259::ChainedPics;
use spin::Mutex;
use x86_64::registers::control::Cr2;
use x86_64::structures::idt::{InterruptDescriptorTable, InterruptStackFrame, PageFaultErrorCode};
use x86_64::PrivilegeLevel;

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
        // one deliberate exception, since this is meant to eventually be
        // a real, controlled entry point FROM ring-3 code, not just
        // another kernel-internal handler. Verified for real below (Fase
        // 69's own self-test actually executes `int 0x80`, from ring-0
        // for now since no ring-3 code exists yet - CPL=0 <= DPL=3 is
        // always permitted, so this doesn't yet prove ring-3 can reach
        // it, only that the gate itself is correctly wired and DOES
        // fire), not just read back as a structural claim.
        idt[SYSCALL_INT_VECTOR]
            .set_handler_fn(syscall_interrupt_handler)
            .set_privilege_level(PrivilegeLevel::Ring3);
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

/// Deliberately minimal for Fase 69's own scope: proves the DPL=3 gate
/// itself is real and genuinely reachable via `int 0x80` (incrementing a
/// counter a self-test can observe), without yet reading the caller's
/// general-purpose registers (RAX/RDI/RSI/RDX would need to hold a real
/// syscall number + args, which `extern "x86-interrupt"` handlers don't
/// receive as parameters the way a normal calling convention would pass
/// them - reading them correctly needs either inline asm or a hand-
/// written naked wrapper, real, separate follow-on work, the same
/// "don't cram two distinct technical challenges into one Fase"
/// reasoning this project already applies elsewhere). No `notify_end_
/// of_interrupt` here, unlike the IRQ handlers above - this is a
/// *software* interrupt (`int 0x80`), never routed through the 8259
/// PIC at all, so there's nothing to acknowledge.
extern "x86-interrupt" fn syscall_interrupt_handler(_stack_frame: InterruptStackFrame) {
    SYSCALL_INT_COUNT.fetch_add(1, Ordering::Relaxed);
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
