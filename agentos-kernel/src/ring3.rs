//! The actual ring-3 transition - Fase 71, the fourth and biggest step
//! of the usermode arc (after Fase 68's GDT/TSS foundation, Fase 69's
//! DPL=3 `int 0x80` gate, and Fase 70's user-accessible page). Every
//! earlier Fase in this arc built infrastructure without ever actually
//! using it to lower the CPU's privilege level - this is the first code
//! that does.
//!
//! **Deliberately NOT run unconditionally on every boot.** Exposed only
//! as the `ring3test` shell command, invoked explicitly - not because
//! the transition itself is expected to fail, but because its own
//! verification strategy (below) intentionally ends in a permanent
//! halt, and running that automatically during every boot would mean
//! this kernel's interactive shell prompt could never be reached for
//! real, everyday use. **Not exercised by CI at all** - unlike the
//! interactive Shift-key test (see main.rs), which CI *can* simulate
//! because it resumes normally afterward, `ring3test` ends in a
//! permanent halt, so there's no safe point in the shared boot-test job
//! to invoke it without either breaking real interactive use or
//! preventing every check after it from ever running. Verified instead
//! via a real, repeatable LOCAL test (3x, temporarily invoked in
//! main.rs, removed before commit) - see kernel-ci.yml's own comment
//! for the full reasoning.
//!
//! **Verification strategy, decided before writing any code**: rather
//! than trying to build a way to safely RETURN from ring-3 back into
//! the rest of the boot sequence (a genuinely harder, separate problem -
//! it would need the interrupt/syscall handler that regains ring-0 to
//! deliberately rewrite its own `InterruptStackFrame` to resume at a
//! DIFFERENT address than the one that got interrupted, not just let
//! the normal `iretq` epilogue return to ring-3 where it left off), this
//! Fase's own ring-3 "program" is a single, deliberately illegal
//! instruction: `cli` (opcode `0xFA`). `cli`/`sti` require `CPL <=
//! IOPL`, and `IOPL` defaults to 0 - so executing `cli` at `CPL=3`
//! (which reaching and fetching it at all already requires) triggers a
//! real `#GP`, caught by the EXISTING, unmodified `general_protection_
//! fault_handler` (already proven correct by this kernel's own earlier
//! exception-handling work), which prints its own diagnostic and halts
//! forever - the exact same "boot ends in a deliberate, expected,
//! diagnosed halt" shape this kernel's own idle loop already relies on
//! (an external timeout is what makes a healthy boot-test run end,
//! either way). Reaching and executing `cli` at all is only possible if
//! EVERY earlier piece - the GDT selectors' own RPL bits (Fase 68), the
//! `iretq` stack frame built below, and the page's own `USER_ACCESSIBLE`
//! bit (Fase 70) - was genuinely correct; the resulting `#GP`'s own
//! `code_segment`/`instruction_pointer` fields (dumped by the existing
//! handler) show a CS with RPL=3 and a RIP matching this exact page's
//! address, real evidence the CPU was genuinely executing in ring-3 at
//! a real, mapped address when the deliberate privilege violation fired
//! - not just "some fault happened somewhere".
//!
//! **Fase 73 solves the "genuinely harder, separate problem" named
//! above**: `enter_ring3`/`ring3_exit_entry_asm`/`run_ring3_exit_test`,
//! further down this file, let a ring-3 program voluntarily exit and
//! have the KERNEL'S OWN execution genuinely continue afterward -
//! unlike every test above, this one does NOT end in a halt, so it runs
//! unconditionally as a normal, always-on self-test (see main.rs) and
//! IS asserted in CI like any other Fase, no special opt-in shell
//! command needed. See that section's own doc for the mechanism.

use crate::memory::user_page::USER_TEST_PAGE_ADDR;
use crate::{gdt, kprintln, serial_println};
use core::arch::naked_asm;

/// `cli` - see this module's own doc for why a single illegal
/// instruction is the entire "program".
const RING3_TEST_OPCODE: u8 = 0xFA;

/// Bit 1 is always reserved-as-1 in real `RFLAGS`; bit 9 (`IF`) is set
/// so the ring-3 context starts with interrupts enabled, the normal
/// expected condition - though since `cli` faults on its very first
/// fetch regardless of the current `IF` value, this doesn't change the
/// outcome, only what a well-formed starting `RFLAGS` should look like.
const RING3_TEST_RFLAGS: u64 = 0x202;

/// Builds a real `iretq` stack frame by hand and executes it - the
/// first genuine ring-0 -> ring-3 privilege-lowering control transfer
/// this kernel has ever attempted. Never returns: either the CPU
/// reaches ring-3 and immediately faults on `cli` (the expected,
/// verified-correct outcome, ending in the existing GPF handler's own
/// permanent halt), or something earlier in this arc was subtly wrong
/// and a DIFFERENT fault/hang occurs instead - either way, nothing
/// after the `iretq` in this function ever executes again.
pub fn run_ring3_test() -> ! {
    let info = gdt::ring3_info();
    let user_cs = info.user_code_selector as u64;
    let user_ss = info.user_data_selector as u64;

    let code_addr = USER_TEST_PAGE_ADDR;
    // Reuses the SAME page as a stack, growing down from its top - safe
    // here specifically because `cli` never pushes or pops anything, so
    // this stack is never actually touched before the expected fault;
    // a real, larger ring-3 program would need its own, separate stack
    // page instead.
    let stack_top = USER_TEST_PAGE_ADDR + 4096;

    unsafe {
        core::ptr::write_volatile(code_addr as *mut u8, RING3_TEST_OPCODE);
    }

    kprintln!("[RING3] Attempting a real ring-3 entry (iretq) - expect an immediate #GP from `cli` at CPL=3...");
    serial_println!(
        "[RING3] ring3_test entering rip={:#x} cs={:#06x} ss={:#06x} rsp={:#x} rflags={:#x}",
        code_addr,
        user_cs,
        user_ss,
        stack_top,
        RING3_TEST_RFLAGS
    );

    unsafe {
        core::arch::asm!(
            "push {ss}",
            "push {rsp}",
            "push {rflags}",
            "push {cs}",
            "push {rip}",
            "iretq",
            ss = in(reg) user_ss,
            rsp = in(reg) stack_top,
            rflags = in(reg) RING3_TEST_RFLAGS,
            cs = in(reg) user_cs,
            rip = in(reg) code_addr,
            options(noreturn)
        );
    }
}

// ---- Fase 73: a SAFE ring-3 -> ring-0 return path ----
//
// Every test above ends in a deliberate, permanent halt - the simplest
// possible proof, but not a REUSABLE mechanism: a real ring-3 "process"
// needs a way to voluntarily hand control back to the kernel and have
// the kernel's own execution genuinely CONTINUE, not just prove a fault
// handler works. That needs the interrupt handler regaining ring-0 to
// resume at a DIFFERENT saved context than the one that got interrupted
// - exactly the harder problem `run_ring3_test`'s own doc named and
// deliberately deferred.
//
// The mechanism: `enter_ring3` (below) is `context_switch.rs`'s own
// `switch_to` trick, adapted to cross a ring-3 boundary instead of just
// a cooperative-task boundary. It saves the SAME 6 callee-saved
// registers (`rbp`/`rbx`/`r12`-`r15`) in the SAME order `switch_to`
// already established, stores the resulting RSP into `RING3_RETURN_RSP`,
// then builds and executes a real `iretq` frame - so far identical in
// spirit to `run_ring3_test` above. The new piece is the OTHER
// direction: `ring3_exit_entry_asm`, entered via a SECOND, DEDICATED
// interrupt vector (`0x81` - see `interrupts.rs`), does NOT restore any
// ring-3 register state at all (there is none worth keeping - the
// ring-3 program is finished) - it stashes the ring-3 program's exit
// code (passed in `rax`, the same register a syscall return value
// already uses) into `RING3_EXIT_CODE`, then loads RSP directly from
// `RING3_RETURN_RSP`, pops the SAME 6 registers `enter_ring3` pushed,
// and `ret`s - popping the exact return address `enter_ring3`'s OWN
// caller pushed via its `call`, resuming there as if `enter_ring3` had
// returned normally. `enter_ring3` itself never executes a `ret` of its
// own; `ring3_exit_entry_asm` does the returning on its behalf, on a
// completely different stack.
//
// Two things make this safe rather than merely convenient: first, both
// vectors default to a 64-bit INTERRUPT gate (confirmed via the
// `x86_64` crate's own `EntryOptions::minimal()`, which every `Entry` in
// this kernel's IDT uses), which the CPU itself unconditionally clears
// `RFLAGS.IF` on entry to - so a timer tick can never fire mid-stub and
// corrupt `RSP0` while either of these is actively using it. Second,
// `RING3_RETURN_RSP` lives on the KERNEL's own ordinary stack (wherever
// `enter_ring3` was called from), entirely separate from `RSP0` (the
// TSS's dedicated ring3->ring0 transition stack, always reset to the
// same fixed top address on every entry) and separate again from the
// ring-3 program's own stack (Fase 70's user page) - three genuinely
// distinct stacks, each used for exactly one thing, never overlapping.
static mut RING3_RETURN_RSP: u64 = 0;
static mut RING3_EXIT_CODE: u64 = 0;

/// Enters ring-3 at `entry` (with the given `iretq` frame fields) and
/// does not return in the usual sense - `ring3_exit_entry_asm` (fired by
/// the ring-3 program's own `int 0x81`) is what actually returns to this
/// function's caller, with the real exit code in `rax`. Mirrors
/// `context_switch::switch_to`'s own save/restore convention exactly
/// (same 6 registers, same order) so the two are trivially comparable.
///
/// # Safety
/// `entry` must point at real, executable, user-accessible code (e.g.
/// Fase 70's user page); `user_rsp` must be a valid, writable ring-3
/// stack pointer; `user_cs`/`user_ss` must be genuine RPL=3 selectors
/// (e.g. `gdt::ring3_info()`'s own fields) - the same preconditions
/// `run_ring3_test`'s own `iretq` frame already relies on.
#[unsafe(naked)]
unsafe extern "C" fn enter_ring3(
    entry: u64,
    user_cs: u64,
    user_ss: u64,
    user_rsp: u64,
    user_rflags: u64,
) -> u64 {
    naked_asm!(
        "push rbp",
        "push rbx",
        "push r12",
        "push r13",
        "push r14",
        "push r15",
        "mov [rip + {ret_rsp}], rsp",
        "push rdx", // ss     (arg3)
        "push rcx", // rsp    (arg4)
        "push r8",  // rflags (arg5)
        "push rsi", // cs     (arg2)
        "push rdi", // rip    (arg1)
        "iretq",
        ret_rsp = sym RING3_RETURN_RSP,
    );
}

/// Entry point for vector `0x81` (`interrupts::RING3_EXIT_INT_VECTOR`) -
/// a ring-3 program's deliberate, voluntary "I'm done" signal, with its
/// exit code in `rax`. Never restores ring-3 state: it switches to the
/// kernel stack `enter_ring3` saved and resumes THERE instead, handing
/// the exit code back as if `enter_ring3` itself had returned it.
#[unsafe(naked)]
pub(crate) extern "C" fn ring3_exit_entry_asm() {
    naked_asm!(
        "mov [rip + {exit_code}], rax",
        "mov rsp, [rip + {ret_rsp}]",
        "pop r15",
        "pop r14",
        "pop r13",
        "pop r12",
        "pop rbx",
        "pop rbp",
        "mov rax, [rip + {exit_code}]",
        "ret",
        exit_code = sym RING3_EXIT_CODE,
        ret_rsp = sym RING3_RETURN_RSP,
    );
}

/// Real ring-3 "program" (Fase 73): a normal syscall (proving Fase 72's
/// own register-passing dispatch still works from within a program that
/// doesn't immediately halt), then a real, voluntary exit via `int
/// 0x81` with a distinctive exit code - and, unlike `run_ring3_test`/
/// `run_ring3_syscall_test`, this ACTUALLY RETURNS, both from the CPU's
/// own perspective (ring-3 -> ring-0) and from Rust's (this function
/// returns a plain `u64` to a completely normal caller). The trailing
/// `cli` is a safety net, not part of the intended path: it is only
/// ever reached if `ring3_exit_entry_asm` somehow failed to redirect
/// control away from this program, in which case it would fault exactly
/// like `run_ring3_test`'s own program - loud, diagnosable failure
/// instead of silent wrong behavior.
///
///   `mov eax, 1`   -> `B8 01 00 00 00` (SYS_SERIAL_PRINT)
///   `int 0x80`     -> `CD 80`
///   `mov eax, 42`  -> `B8 2A 00 00 00` (the exit code this test expects back)
///   `int 0x81`     -> `CD 81` (RING3_EXIT_INT_VECTOR)
///   `cli`          -> `FA`   (safety net only - see above)
pub fn run_ring3_exit_test() -> u64 {
    let info = gdt::ring3_info();
    let user_cs = info.user_code_selector as u64;
    let user_ss = info.user_data_selector as u64;

    let code_addr = USER_TEST_PAGE_ADDR;
    let stack_top = USER_TEST_PAGE_ADDR + 4096;

    const PROGRAM: [u8; 15] = [
        0xB8, 0x01, 0x00, 0x00, 0x00, 0xCD, 0x80, 0xB8, 0x2A, 0x00, 0x00, 0x00, 0xCD, 0x81, 0xFA,
    ];
    unsafe {
        core::ptr::copy_nonoverlapping(PROGRAM.as_ptr(), code_addr as *mut u8, PROGRAM.len());
    }

    kprintln!("[RING3] Attempting a real ring-3 program that exits voluntarily (int 0x81) instead of faulting...");
    serial_println!(
        "[RING3] ring3_exit_test entering rip={:#x} cs={:#06x} ss={:#06x} rsp={:#x} rflags={:#x}",
        code_addr,
        user_cs,
        user_ss,
        stack_top,
        RING3_TEST_RFLAGS
    );

    let exit_code =
        unsafe { enter_ring3(code_addr, user_cs, user_ss, stack_top, RING3_TEST_RFLAGS) };

    kprintln!(
        "[RING3] Genuinely resumed in ring-0 after a real ring-3 exit - exit_code={}",
        exit_code
    );
    serial_println!("[RING3] ring3_exit_test exit_code={}", exit_code);

    exit_code
}

/// A real ring-3-originated syscall (Fase 72), followed by the same
/// proven `cli`-fault ending `run_ring3_test` (Fase 71) already
/// established. The "program" is three real instructions, hand-encoded
/// the same way `run_ring3_test`'s single `cli` byte was:
///   `mov eax, 1`  -> `B8 01 00 00 00` (SYS_SERIAL_PRINT, opcode `B8+rd`
///                    for EAX plus a 4-byte little-endian immediate)
///   `int 0x80`    -> `CD 80`
///   `cli`         -> `FA` (reused from Fase 71 - see its own doc)
///
/// Unlike `run_ring3_test`, execution genuinely RESUMES in ring-3 after
/// the `int 0x80` (the DPL=3 gate's `iretq` returns to right where it
/// was invoked, exactly as a normal syscall should) - real, direct proof
/// that `syscall_entry_asm` correctly preserves and restores the ring-3
/// context across a full round trip, not just that it can be entered.
/// Only THEN does the same deliberately-illegal `cli` fault as before,
/// for the same reason: a clean, already-proven, diagnosed halt is a
/// natural test endpoint, not something to engineer a safe general
/// ring-3->ring-0 return around (that remains real, separate follow-on
/// work).
pub fn run_ring3_syscall_test() -> ! {
    let info = gdt::ring3_info();
    let user_cs = info.user_code_selector as u64;
    let user_ss = info.user_data_selector as u64;

    let code_addr = USER_TEST_PAGE_ADDR;
    let stack_top = USER_TEST_PAGE_ADDR + 4096;

    const PROGRAM: [u8; 8] = [0xB8, 0x01, 0x00, 0x00, 0x00, 0xCD, 0x80, 0xFA];
    unsafe {
        core::ptr::copy_nonoverlapping(PROGRAM.as_ptr(), code_addr as *mut u8, PROGRAM.len());
    }

    kprintln!("[RING3] Attempting a real ring-3 syscall (int 0x80, sys_nr=SYS_SERIAL_PRINT) - expect it to resume here, then fault on `cli`...");
    serial_println!(
        "[RING3] ring3_syscall_test entering rip={:#x} cs={:#06x} ss={:#06x} rsp={:#x} rflags={:#x}",
        code_addr,
        user_cs,
        user_ss,
        stack_top,
        RING3_TEST_RFLAGS
    );

    unsafe {
        core::arch::asm!(
            "push {ss}",
            "push {rsp}",
            "push {rflags}",
            "push {cs}",
            "push {rip}",
            "iretq",
            ss = in(reg) user_ss,
            rsp = in(reg) stack_top,
            rflags = in(reg) RING3_TEST_RFLAGS,
            cs = in(reg) user_cs,
            rip = in(reg) code_addr,
            options(noreturn)
        );
    }
}
