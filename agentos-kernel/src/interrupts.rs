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

/// Counts real timer ticks that interrupted ring-3 code specifically -
/// Fase 79's own first step toward eventual ring-3-aware preemption
/// (currently `scheduler::preemptive` only ever switches between ring-0
/// tasks; see its own module doc). `InterruptStackFrame`'s `code_segment`
/// field already carries this for free via the CPU's own interrupt-entry
/// mechanism - no naked asm needed, unlike `syscall_entry_asm`'s own
/// CS.RPL read (Fase 75), which needed one specifically because THAT
/// handler had to be naked for an unrelated reason (exposing general-
/// purpose registers). `timer_interrupt_handler` stays an ordinary
/// `extern "x86-interrupt"` fn - ring-3 detection alone never needed
/// that.
static TIMER_TICKS_WHILE_RING3: AtomicU64 = AtomicU64::new(0);

pub fn ticks_while_ring3() -> u64 {
    TIMER_TICKS_WHILE_RING3.load(Ordering::Relaxed)
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

/// A THIRD, dedicated DPL=3 vector (Fase 81) - the voluntary exit signal
/// for a ring-3 "task" entered via `ring3::run_ring3_switchto_bootstrap_
/// test`'s new bootstrap mechanism (`prepare_ring3_initial_stack` +
/// `switch_to`), which is itself structurally different from BOTH the
/// synchronous `enter_ring3`/`ring3_exit_entry_asm` pair above: that pair
/// resumes an ordinary Rust call's own caller (`enter_ring3`'s own
/// convention, including a `pushfq`/`popfq` pair since `enter_ring3`
/// itself does the `pushfq`); this one resumes whoever called the
/// cooperative-scheduler's own `switch_to` primitive, using ITS pop
/// convention (r15/r14/r13/r12/rbx/rbp, no saved RFLAGS - `switch_to`
/// never pushes any) instead. Reusing 0x81 for both would mean one
/// handler branching on which convention applies - exactly the
/// complexity RING3_EXIT_INT_VECTOR's own doc above already argued
/// against once; the same reasoning applies again here.
pub const RING3_TASK_EXIT_INT_VECTOR: u8 = 0x82;

/// A FOURTH DPL=3 vector (Fase 83) - a switch_to-bootstrapped ring-3
/// task's voluntary YIELD signal (as opposed to RING3_TASK_EXIT_INT_
/// VECTOR's own voluntary EXIT), letting two such tasks cooperatively
/// interleave instead of always running to completion uninterrupted.
/// Structurally different from every vector above it (this one resumes
/// a DIFFERENT ring-3 task's own saved context, not a plain Rust caller
/// via either convention those two already use) - same "one dedicated
/// vector per structurally-different behavior" reasoning that already
/// justified keeping 0x81/0x82 apart from 0x80 and from each other.
pub const RING3_COOP_YIELD_INT_VECTOR: u8 = 0x83;

/// A FIFTH DPL=3 vector (Fase 93) - a `scheduler::ring3_mt`-scheduled
/// NON-task-0 task's voluntary "I'm done, stop scheduling me" signal, as
/// opposed to `RING3_COOP_YIELD_INT_VECTOR`'s own voluntary YIELD (which
/// resumes the SAME task again later) or `RING3_EXIT_INT_VECTOR`'s own
/// task-0-only completion signal (which ends the whole `run_multitasking`
/// session). Structurally closest to the TIMER tick's own handling of a
/// ring-3 context (`scheduler::ring3_mt::tick` operates on the identical
/// 160-byte full-15-GPR context this vector's own asm stub captures,
/// unlike `RING3_TASK_EXIT_INT_VECTOR`'s smaller `switch_to`-convention
/// snapshot or `RING3_COOP_YIELD_INT_VECTOR`'s own cooperative one) - so
/// its entry stub reuses `timer_interrupt_entry_asm`'s exact shape
/// verbatim, just reached voluntarily via `int 0x84` rather than by
/// hardware, and calls `scheduler::ring3_mt::task_done` instead of
/// `handle_timer_tick`. Kept as its own dedicated vector rather than
/// folded into any vector above for the same "one dedicated vector per
/// structurally-different behavior" reasoning that already justified
/// keeping 0x81/0x82/0x83 apart.
pub const RING3_MT_TASK_DONE_INT_VECTOR: u8 = 0x84;

/// A SIXTH DPL=3 vector (Fase 96) - a NON-task-0 cooperative-yield task's
/// own voluntary "I'm done, stop scheduling me" signal, the cooperative-
/// mechanism counterpart to `RING3_MT_TASK_DONE_INT_VECTOR` above. Reuses
/// `ring3_coop_yield_entry_asm`'s exact 96-byte-context shape (6 callee-
/// saved registers + a manufactured trampoline address + the CPU's own
/// 5-field `iretq` frame) rather than `RING3_MT_TASK_DONE_INT_VECTOR`'s
/// own 160-byte full-15-GPR one - the two "task done" vectors are
/// structurally different for the exact same reason `RING3_COOP_YIELD_
/// INT_VECTOR` and `RING3_MT_TASK_DONE_INT_VECTOR` already were: each
/// cooperative-mechanism vector operates on `switch_to`'s own smaller
/// convention, each involuntary-timer-mechanism vector on the timer
/// stub's own full-register one. Calls `ring3::ring3_coop_task_done_
/// helper` instead of `ring3::ring3_coop_yield_helper`.
pub const RING3_COOP_TASK_DONE_INT_VECTOR: u8 = 0x85;

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
        // Fase 84: `set_handler_addr`, not `set_handler_fn` - the timer
        // entry point is now a hand-written naked stub (see its own doc
        // below), the same reasoning `SYSCALL_INT_VECTOR`'s own
        // registration already established for the identical reason
        // (exposing general-purpose registers `extern "x86-interrupt"`
        // can't). Stays DPL=0 (unlike the syscall/ring3 vectors below) -
        // a hardware IRQ is never reachable via a deliberate ring-3 `int`
        // instruction, so there's no privilege level to relax here.
        unsafe {
            idt[InterruptIndex::Timer.as_u8()]
                .set_handler_addr(VirtAddr::new(timer_interrupt_entry_asm as *const () as u64));
        }
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
        // Same reasoning (DPL=3, raw set_handler_addr) - see
        // RING3_TASK_EXIT_INT_VECTOR's own doc for why this is a THIRD,
        // dedicated vector rather than folded into either handler above.
        unsafe {
            idt[RING3_TASK_EXIT_INT_VECTOR]
                .set_handler_addr(VirtAddr::new(
                    crate::ring3::ring3_task_exit_entry_asm as *const () as u64,
                ))
                .set_privilege_level(PrivilegeLevel::Ring3);
        }
        // Same reasoning (DPL=3, raw set_handler_addr) - see
        // RING3_COOP_YIELD_INT_VECTOR's own doc for why this is a
        // FOURTH, dedicated vector.
        unsafe {
            idt[RING3_COOP_YIELD_INT_VECTOR]
                .set_handler_addr(VirtAddr::new(
                    crate::ring3::ring3_coop_yield_entry_asm as *const () as u64,
                ))
                .set_privilege_level(PrivilegeLevel::Ring3);
        }
        // Same reasoning (DPL=3, raw set_handler_addr) - see
        // RING3_MT_TASK_DONE_INT_VECTOR's own doc for why this is a
        // FIFTH, dedicated vector.
        unsafe {
            idt[RING3_MT_TASK_DONE_INT_VECTOR]
                .set_handler_addr(VirtAddr::new(
                    crate::ring3::ring3_mt_task_done_entry_asm as *const () as u64,
                ))
                .set_privilege_level(PrivilegeLevel::Ring3);
        }
        // Same reasoning (DPL=3, raw set_handler_addr) - see
        // RING3_COOP_TASK_DONE_INT_VECTOR's own doc for why this is a
        // SIXTH, dedicated vector.
        unsafe {
            idt[RING3_COOP_TASK_DONE_INT_VECTOR]
                .set_handler_addr(VirtAddr::new(
                    crate::ring3::ring3_coop_task_done_entry_asm as *const () as u64,
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

/// Fase 84: the timer IRQ entry point converted from a compiler-generated
/// `extern "x86-interrupt"` fn (which only ever transparently preserves
/// whatever THIS SAME invocation's own compiled body happens to touch,
/// on the assumption its OWN epilogue will eventually restore them) to a
/// hand-written naked stub that explicitly saves ALL 15 general-purpose
/// registers - the exact same shape, push/pop order, and CS.RPL-read-at-
/// offset-128 trick `syscall_entry_asm` (Fase 72/75) already established
/// and this kernel already relies on working. Reused here verbatim
/// rather than re-derived, since the underlying reasoning (a
/// cross-privilege interrupt frame's CS field sits at a FIXED offset
/// above 15 pushed registers regardless of whether this specific
/// interrupt happened to be same-privilege or cross-privilege, since the
/// extra RSP/SS fields a cross-privilege entry adds sit ABOVE RFLAGS,
/// never between RIP and CS) applies identically to the timer IRQ.
///
/// **Fase 84 was a PURE, behavior-preserving refactor - no new
/// capability, no new CI assertion.** The only thing it changed was HOW
/// MANY registers get captured at the moment a tick lands (all 15, not
/// just whatever LLVM happened to need) - the necessary prerequisite for
/// genuine mid-flight ring-3 preemption (which needs the FULL register
/// state of whatever was interrupted, not just the 6 callee-saved ones
/// `switch_to`/Fase 83's own cooperative yield already handle).
///
/// **Fase 85 is the first Fase to actually USE those captured
/// registers.** `mov rsi, rsp` (right after the 15 pushes) passes the
/// base of this whole 15-GPR block to `handle_timer_tick` as a second
/// argument - the CPU's own 5-field iretq frame sits directly above it,
/// so this single pointer covers all 20 quadwords (160 bytes) of a ring-
/// 3 task's ENTIRE resumable state. `handle_timer_tick` now returns a
/// `u64` - the RSP to actually resume from - and `mov rsp, rax` (right
/// after the call, before any pops) adopts it. For every case except the
/// new one (`scheduler::ring3_preempt`'s own interception, gated behind
/// its own explicit enable flag - see that module's own doc), this
/// returned value is `saved_ctx_ptr` UNCHANGED, making `mov rsp, rax` a
/// genuine no-op and keeping this byte-identical to Fase 84's own
/// already-verified behavior for every EXISTING self-test.
#[unsafe(naked)]
extern "C" fn timer_interrupt_entry_asm() {
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
        "mov rdi, [rsp + 128]",
        "and rdi, 3",
        "mov rsi, rsp",
        "call {handler}",
        "mov rsp, rax",
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
        handler = sym handle_timer_tick,
    );
}

/// Called by `timer_interrupt_entry_asm` with the interrupted context's
/// own CS.RPL (0 or 3) already isolated to its low 2 bits, plus (Fase
/// 85) `saved_ctx_ptr`, the base of that stub's own 15-GPR save block -
/// a normal, non-naked `extern "C" fn`, so this is exactly as safe to
/// write as any other Rust code in this codebase, the same reasoning
/// `handle_real_syscall` (Fase 72) already established for the identical
/// "raw register work stays in the naked stub, ordinary logic lives
/// here" split.
///
/// Returns the RSP the naked stub's own epilogue should actually resume
/// from. `preemptive::tick(interrupted_ring3)` still runs FIRST,
/// completely unchanged from Fase 80/84 - it already skips ring-0
/// task-switching entirely whenever the interrupted context was ring-3,
/// so this new return-value plumbing never interferes with it either
/// way. `ring3_preempt::tick(saved_ctx_ptr)` and (Fase 86) `ring3_mt::
/// tick(..)` then run in a strictly layered CHAIN, ring-3-case only:
/// each returns its own input UNCHANGED (a genuine no-op) unless ITS
/// OWN mechanism is the one currently armed - see each module's own
/// doc - so exactly one of the two ever actually does anything on any
/// given boot (today's self-tests never enable both at once), and
/// every EXISTING self-test that arms neither sees this whole chain stay
/// byte-identical to Fase 84's own already-verified behavior.
extern "C" fn handle_timer_tick(caller_rpl: u64, saved_ctx_ptr: u64) -> u64 {
    TIMER_TICKS.fetch_add(1, Ordering::Relaxed);
    let interrupted_ring3 = caller_rpl == 3;
    if interrupted_ring3 {
        TIMER_TICKS_WHILE_RING3.fetch_add(1, Ordering::Relaxed);
    }

    // EOI must happen before `preemptive::tick()`, which may switch to a
    // different context and never return from *this* call - if we hadn't
    // acknowledged the interrupt yet, the PIC would think it's still in
    // service and could withhold further same/lower-priority IRQs.
    unsafe {
        PICS.lock()
            .notify_end_of_interrupt(InterruptIndex::Timer.as_u8());
    }

    // interrupted_ring3 threads straight through to tick() - see
    // scheduler::preemptive::tick's own doc (Fase 80) for why this
    // matters: it must skip ring-0 task-switching entirely whenever the
    // interrupted context was ring-3, not just log it like the counter
    // above does.
    crate::scheduler::preemptive::tick(interrupted_ring3);

    if interrupted_ring3 {
        let ctx = crate::scheduler::ring3_preempt::tick(saved_ctx_ptr);
        crate::scheduler::ring3_mt::tick(ctx)
    } else {
        saved_ctx_ptr
    }
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
/// Fase 76 puts this value to real use: passed into `dispatch_syscall`
/// as `caller_rpl == 3`, which `SYS_TENSOR_EVAL`'s own arm now uses to
/// require `USER_ACCESSIBLE` on every pointer it dereferences, but only
/// when the call genuinely came from ring-3 - see `syscall.rs`'s own
/// `pointer_is_mapped_checked` for exactly how the two cases differ.
extern "C" fn handle_real_syscall(
    sys_nr: u64,
    arg1: u64,
    arg2: u64,
    arg3: u64,
    caller_rpl: u64,
) -> u64 {
    SYSCALL_INT_COUNT.fetch_add(1, Ordering::Relaxed);
    let ret = crate::syscall::dispatch_syscall(sys_nr, arg1, arg2, arg3, caller_rpl == 3);
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
