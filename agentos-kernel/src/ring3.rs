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
//
// A real bug lived here on the first attempt, caught by CI rather than
// local testing: `ring3_exit_entry_asm` exits via `ret`, not `iretq` -
// meaning it never restores `RFLAGS`, so the `IF` bit the CPU itself
// force-cleared on entry (same INTERRUPT-gate behavior noted above)
// stayed cleared FOREVER after the first ring-3 exit, since nothing
// else in the whole kernel was going to re-enable interrupts on its
// own. Every OTHER self-test still ran (none of them specifically
// depend on a NEW interrupt firing to make progress) until the
// preemptive scheduler demo, which genuinely cannot complete without
// real timer ticks forcing a switch - it hung forever, timing out the
// whole CI job. Fixed by saving `RFLAGS` (via `pushfq`) alongside the 6
// callee-saved registers in `enter_ring3`, and restoring it (via
// `popfq`) in `ring3_exit_entry_asm` before the matching register pops
// - see both functions' own doc for the exact mechanics.
static mut RING3_RETURN_RSP: u64 = 0;
static mut RING3_EXIT_CODE: u64 = 0;

/// Enters ring-3 at `entry` (with the given `iretq` frame fields) and
/// does not return in the usual sense - `ring3_exit_entry_asm` (fired by
/// the ring-3 program's own `int 0x81`) is what actually returns to this
/// function's caller, with the real exit code in `rax`. Mirrors
/// `context_switch::switch_to`'s own save/restore convention (same 6
/// registers, same order) plus one addition `switch_to` doesn't need:
/// `pushfq` saves this caller's own `RFLAGS` (in particular `IF`)
/// alongside them, restored by `ring3_exit_entry_asm`'s own `popfq` -
/// see that function's own doc for why this one, unlike every other
/// naked/interrupt-gate handler in this kernel, genuinely needs it.
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
        "pushfq",
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
///
/// `popfq` matters more than it looks: every IDT entry in this kernel
/// (confirmed via the `x86_64` crate's own `EntryOptions::minimal()`)
/// defaults to a 64-bit INTERRUPT gate, which the CPU itself
/// unconditionally clears `RFLAGS.IF` on entry to. `syscall_entry_asm`
/// (Fase 72) never has to think about this because it always exits via
/// `iretq`, which restores the FULL `RFLAGS` (including `IF`) as part
/// of its own defined behavior. This function is the one place in the
/// whole kernel that DOESN'T exit via `iretq` - it `ret`s back into
/// ordinary kernel code instead - and a plain `ret` does not touch
/// `RFLAGS` at all. Without this `popfq`, `IF` would stay stuck at 0
/// forever after the very first ring-3 exit, silently breaking every
/// `hlt()`-based wait anywhere else in this kernel from that point on
/// (nothing could ever wake from `hlt` again) - exactly the failure a
/// real CI run caught (a later self-test hung, timing out the whole
/// boot-test job) before this fix.
#[unsafe(naked)]
pub(crate) extern "C" fn ring3_exit_entry_asm() {
    naked_asm!(
        "mov [rip + {exit_code}], rax",
        "mov rsp, [rip + {ret_rsp}]",
        "popfq",
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

/// Proves `SYS_TENSOR_EVAL`'s new `USER_ACCESSIBLE` enforcement (Fase 76)
/// is real, not just "the code compiles": a genuine ring-3 program calls
/// it with a `TensorEvalArgs` whose embedded pointers all target
/// `memory::heap::HEAP_START` - real, `PRESENT` memory (so Fase 74's own
/// check alone would have let it through), but never `USER_ACCESSIBLE`
/// (every mapping before Fase 70 was `PRESENT | WRITABLE` only - see
/// `memory/user_page.rs`'s own doc). Expects `u64::MAX` back: unlike
/// `run_ring3_exit_test`'s own valid call (which this kernel's own
/// `syscall.rs` self-test elsewhere still proves works from ring-0), a
/// ring-3 caller pointing at kernel-only memory should now be rejected.
///
/// Reuses the exact same safe-return mechanism as `run_ring3_exit_test`
/// (`enter_ring3`/`ring3_exit_entry_asm`, Fase 73) - this test also
/// genuinely returns rather than halting, so it runs unconditionally
/// like that one does. The `TensorEvalArgs` struct itself is written as
/// an ordinary Rust value into the user page (not hand-encoded bytes,
/// unlike the small instruction sequences below it) - only the tiny
/// ring-3 "program" that invokes the syscall needs raw opcodes, since
/// it has to run to reach `int 0x80` at all.
///
///   `mov rax, 4`    -> `48 B8` + 8-byte imm (`SYS_TENSOR_EVAL`, needs a
///                       64-bit immediate: `mov eax, imm32` can't reach
///                       past 4 GiB, and this page's own address can't
///                       fit in 32 bits)
///   `mov rdi, addr` -> `48 BF` + 8-byte imm (the `TensorEvalArgs` pointer)
///   `int 0x80`      -> `CD 80` (leaves the real return value in `rax`)
///   `int 0x81`      -> `CD 81` (exits with THAT value, untouched, as
///                       this function's own return - so `u64::MAX`
///                       here is proof the syscall itself was rejected,
///                       not an artifact of the exit mechanism)
pub fn run_ring3_pointer_reject_test() -> u64 {
    let info = gdt::ring3_info();
    let user_cs = info.user_code_selector as u64;
    let user_ss = info.user_data_selector as u64;

    let code_addr = USER_TEST_PAGE_ADDR;
    let stack_top = USER_TEST_PAGE_ADDR + 4096;
    // Well clear of the 20-byte code sequence below, still inside the
    // same 4 KiB page.
    let args_addr = USER_TEST_PAGE_ADDR + 256;

    let kernel_only_ptr = crate::memory::heap::HEAP_START as u64;
    let args = crate::syscall::TensorEvalArgs {
        weights: kernel_only_ptr as *const f32,
        weights_len: 1,
        inputs: kernel_only_ptr as *const f32,
        inputs_len: 1,
        bias: kernel_only_ptr as *const f32,
        bias_len: 1,
        outputs: kernel_only_ptr as *mut f32,
        outputs_len: 1,
        in_dim: 1,
        out_dim: 1,
    };
    unsafe {
        core::ptr::write_volatile(args_addr as *mut crate::syscall::TensorEvalArgs, args);
    }

    let mut program = [0u8; 20];
    program[0] = 0x48;
    program[1] = 0xB8;
    program[2..10].copy_from_slice(&crate::syscall::SYS_TENSOR_EVAL.to_le_bytes());
    program[10] = 0x48;
    program[11] = 0xBF;
    program[12..20].copy_from_slice(&args_addr.to_le_bytes());
    unsafe {
        core::ptr::copy_nonoverlapping(program.as_ptr(), code_addr as *mut u8, program.len());
        // int 0x80 ; int 0x81, right after the two 10-byte mov's above.
        core::ptr::write_volatile((code_addr + 20) as *mut u8, 0xCD);
        core::ptr::write_volatile((code_addr + 21) as *mut u8, 0x80);
        core::ptr::write_volatile((code_addr + 22) as *mut u8, 0xCD);
        core::ptr::write_volatile((code_addr + 23) as *mut u8, 0x81);
    }

    kprintln!("[RING3] Attempting SYS_TENSOR_EVAL from ring-3 with a kernel-only (non-user-accessible) pointer - expecting rejection...");
    serial_println!(
        "[RING3] ring3_pointer_reject_test entering rip={:#x} cs={:#06x} ss={:#06x} rsp={:#x} rflags={:#x} kernel_only_ptr={:#x}",
        code_addr,
        user_cs,
        user_ss,
        stack_top,
        RING3_TEST_RFLAGS,
        kernel_only_ptr
    );

    let exit_code =
        unsafe { enter_ring3(code_addr, user_cs, user_ss, stack_top, RING3_TEST_RFLAGS) };

    kprintln!(
        "[RING3] Back in ring-0 - SYS_TENSOR_EVAL from ring-3 returned {:#x}",
        exit_code
    );
    serial_println!(
        "[RING3] ring3_pointer_reject_test exit_code={:#x}",
        exit_code
    );

    exit_code
}

/// Proves `SYS_TENSOR_EVAL`'s pointer checks now cover a slice's WHOLE
/// length, not just its starting address (Fase 77). Reuses
/// `run_ring3_pointer_reject_test`'s own shape almost exactly, with one
/// deliberate change: `weights` points 4 bytes before the user page's
/// own end (`USER_TEST_PAGE_ADDR + 4092`) - a real, `PRESENT` AND
/// `USER_ACCESSIBLE` address, so the OLD (Fase 74/76) starting-address-
/// only check would have WRONGLY ACCEPTED it - but `weights_len = 2`
/// asks for 8 bytes (`2 * size_of::<f32>()`), and only 4 remain before
/// the page boundary. The next page (`USER_TEST_PAGE_ADDR + 4096`) was
/// never mapped at all (Fase 70's own page is a single, deliberately
/// isolated 4 KiB mapping - see that module's own doc), so the RANGE
/// genuinely runs off the end into nothing. `inputs`/`bias`/`outputs`
/// all use `len = 0` (skipped by `pointer_is_mapped_checked` itself),
/// keeping this test isolated to exactly the one new thing being
/// proven, not re-proving Fase 76's own kernel-only-pointer check too.
pub fn run_ring3_slice_overrun_test() -> u64 {
    let info = gdt::ring3_info();
    let user_cs = info.user_code_selector as u64;
    let user_ss = info.user_data_selector as u64;

    let code_addr = USER_TEST_PAGE_ADDR;
    let stack_top = USER_TEST_PAGE_ADDR + 4096;
    let args_addr = USER_TEST_PAGE_ADDR + 256;
    // 4 bytes before the page's own end - real, mapped, user-accessible
    // memory, but with no room left for the 8 bytes `weights_len = 2`
    // asks for.
    let overrunning_ptr = USER_TEST_PAGE_ADDR + 4092;

    let args = crate::syscall::TensorEvalArgs {
        weights: overrunning_ptr as *const f32,
        weights_len: 2,
        inputs: core::ptr::null(),
        inputs_len: 0,
        bias: core::ptr::null(),
        bias_len: 0,
        outputs: core::ptr::null_mut(),
        outputs_len: 0,
        in_dim: 1,
        out_dim: 1,
    };
    unsafe {
        core::ptr::write_volatile(args_addr as *mut crate::syscall::TensorEvalArgs, args);
    }

    let mut program = [0u8; 20];
    program[0] = 0x48;
    program[1] = 0xB8;
    program[2..10].copy_from_slice(&crate::syscall::SYS_TENSOR_EVAL.to_le_bytes());
    program[10] = 0x48;
    program[11] = 0xBF;
    program[12..20].copy_from_slice(&args_addr.to_le_bytes());
    unsafe {
        core::ptr::copy_nonoverlapping(program.as_ptr(), code_addr as *mut u8, program.len());
        core::ptr::write_volatile((code_addr + 20) as *mut u8, 0xCD);
        core::ptr::write_volatile((code_addr + 21) as *mut u8, 0x80);
        core::ptr::write_volatile((code_addr + 22) as *mut u8, 0xCD);
        core::ptr::write_volatile((code_addr + 23) as *mut u8, 0x81);
    }

    kprintln!("[RING3] Attempting SYS_TENSOR_EVAL from ring-3 with a slice that overruns its starting page - expecting rejection...");
    serial_println!(
        "[RING3] ring3_slice_overrun_test entering rip={:#x} cs={:#06x} ss={:#06x} rsp={:#x} rflags={:#x} overrunning_ptr={:#x}",
        code_addr,
        user_cs,
        user_ss,
        stack_top,
        RING3_TEST_RFLAGS,
        overrunning_ptr
    );

    let exit_code =
        unsafe { enter_ring3(code_addr, user_cs, user_ss, stack_top, RING3_TEST_RFLAGS) };

    kprintln!(
        "[RING3] Back in ring-0 - SYS_TENSOR_EVAL from ring-3 (overrunning slice) returned {:#x}",
        exit_code
    );
    serial_println!(
        "[RING3] ring3_slice_overrun_test exit_code={:#x}",
        exit_code
    );

    exit_code
}

/// Fase 79, first step toward eventual multi-ring3-process scheduling:
/// proves the timer interrupt can genuinely tell it interrupted RING-3
/// code specifically, not just that a tick happened (`interrupts.rs`'s
/// own `TIMER_TICKS_WHILE_RING3`, incremented from the CPU's own
/// `code_segment.rpl()` on every tick). `scheduler::preemptive`'s
/// existing timer-driven preemption only ever switches between ring-0
/// tasks (see its own module doc) - genuinely scheduling ring-3
/// "processes" needs the timer handler to be able to save/restore a
/// FULL ring-3 register + privilege-level context, not just the
/// callee-saved registers `switch_to` currently handles. This is
/// deliberately NOT that larger mechanism - just the first, narrower
/// thing it depends on: can this kernel even tell, from inside the
/// timer handler, that ring-3 code was running when the tick fired?
///
/// The ring-3 "program" spins in a tight hand-encoded decrement loop
/// (20,000,000 iterations - generous headroom over the ~55ms a single
/// tick takes at this kernel's ~18.2Hz PIT rate, chosen the same
/// empirically-verified-not-just-assumed way `ata.rs`'s own
/// `MAX_POLL_ITERATIONS` was) before exiting voluntarily via `int 0x81`,
/// reusing `enter_ring3`/`ring3_exit_entry_asm` (Fase 73) exactly as
/// `run_ring3_exit_test` does: this also genuinely returns rather than
/// halting, and runs unconditionally like that one does.
///
///   `mov ecx, 20000000` -> `B9` + 4-byte imm (loop counter)
///   `dec ecx`           -> `FF C9`
///   `jnz <dec ecx>`     -> `75 FC` (rel8 = -4, back to the `dec`)
///   `mov eax, 77`       -> `B8` + 4-byte imm (distinct exit code)
///   `int 0x81`          -> `CD 81`
pub fn run_ring3_timer_tick_test() -> u64 {
    let info = gdt::ring3_info();
    let user_cs = info.user_code_selector as u64;
    let user_ss = info.user_data_selector as u64;

    let code_addr = USER_TEST_PAGE_ADDR;
    let stack_top = USER_TEST_PAGE_ADDR + 4096;

    const LOOP_COUNT: u32 = 20_000_000;
    const EXIT_CODE: u32 = 77;

    let mut program = [0u8; 16];
    program[0] = 0xB9; // mov ecx, imm32
    program[1..5].copy_from_slice(&LOOP_COUNT.to_le_bytes());
    program[5] = 0xFF; // dec ecx
    program[6] = 0xC9;
    program[7] = 0x75; // jnz rel8
    program[8] = 0xFC; // -4: back to `dec ecx` at offset 5
    program[9] = 0xB8; // mov eax, imm32
    program[10..14].copy_from_slice(&EXIT_CODE.to_le_bytes());
    program[14] = 0xCD; // int
    program[15] = 0x81;
    unsafe {
        core::ptr::copy_nonoverlapping(program.as_ptr(), code_addr as *mut u8, program.len());
    }

    let ticks_before = crate::interrupts::ticks_while_ring3();

    kprintln!("[RING3] Attempting a real ring-3 spin loop long enough for a real timer tick to land mid-execution...");
    serial_println!(
        "[RING3] ring3_timer_tick_test entering rip={:#x} cs={:#06x} ss={:#06x} rsp={:#x} rflags={:#x} loop_count={} ticks_while_ring3_before={}",
        code_addr,
        user_cs,
        user_ss,
        stack_top,
        RING3_TEST_RFLAGS,
        LOOP_COUNT,
        ticks_before
    );

    let exit_code =
        unsafe { enter_ring3(code_addr, user_cs, user_ss, stack_top, RING3_TEST_RFLAGS) };

    let ticks_after = crate::interrupts::ticks_while_ring3();
    let ticked_during_ring3 = ticks_after > ticks_before;

    kprintln!(
        "[RING3] Back in ring-0 - exit_code={} ticks_while_ring3 before={} after={} (a real timer tick fired while ring-3 code was running: {})",
        exit_code,
        ticks_before,
        ticks_after,
        ticked_during_ring3
    );
    serial_println!(
        "[RING3] ring3_timer_tick_test exit_code={} ticks_while_ring3_after={} ticked_during_ring3={}",
        exit_code,
        ticks_after,
        ticked_during_ring3
    );

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
