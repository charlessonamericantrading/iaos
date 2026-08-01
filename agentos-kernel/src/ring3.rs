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

use crate::memory::user_page::USER_TEST_PAGE_ADDR;
use crate::{gdt, kprintln, serial_println};

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
