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
/// before exiting voluntarily via `int 0x81`, reusing
/// `enter_ring3`/`ring3_exit_entry_asm` (Fase 73) exactly as
/// `run_ring3_exit_test` does: this also genuinely returns rather than
/// halting, and runs unconditionally like that one does.
///
/// **`LOOP_COUNT` found the hard way, not assumed correct on the first
/// guess**: an initial 20,000,000 (chosen as "generous headroom over
/// the ~55ms a single tick takes at this kernel's ~18.2Hz PIT rate",
/// the same empirically-verified-not-just-assumed spirit as `ata.rs`'s
/// own `MAX_POLL_ITERATIONS`) passed CI once, but a later LOCAL rerun
/// showed `ticked_during_ring3=false` - zero ticks landed during that
/// specific run's own loop. QEMU's TCG JIT evidently doesn't execute
/// this tiny loop at a fixed, predictable speed run-to-run (translation
/// caching/warm-up effects, most likely) - meaning 20,000,000 was
/// marginal, not safely above one tick period, and the test could have
/// been silently flaky in CI too. Raised to 150,000,000 (7.5x) for real
/// margin, re-verified across multiple repeated local boots to
/// confirm `ticked_during_ring3=true` every single time before trusting
/// it again.
///
///   `mov ecx, 150000000` -> `B9` + 4-byte imm (loop counter)
///   `dec ecx`            -> `FF C9`
///   `jnz <dec ecx>`      -> `75 FC` (rel8 = -4, back to the `dec`)
///   `mov eax, 77`        -> `B8` + 4-byte imm (distinct exit code)
///   `int 0x81`           -> `CD 81`
pub fn run_ring3_timer_tick_test() -> u64 {
    let info = gdt::ring3_info();
    let user_cs = info.user_code_selector as u64;
    let user_ss = info.user_data_selector as u64;

    let code_addr = USER_TEST_PAGE_ADDR;
    let stack_top = USER_TEST_PAGE_ADDR + 4096;

    const LOOP_COUNT: u32 = 150_000_000;
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

/// Runs a ring-0 preemptive demo and a real ring-3 program CONCURRENTLY
/// for the first time (Fase 80) - every prior test of either ran
/// sequentially, never overlapping. Closes a real, previously-untested
/// gap `scheduler::preemptive::tick`'s own doc explains in full:
/// `switch_to` cannot safely swap out a ring-3-interrupted stack the way
/// it does between two ring-0 tasks (an arbitrary register state and
/// privilege level at an arbitrary point, not a nested call frame it
/// can safely resume later), so a tick landing on ring-3 code must be
/// entirely invisible to that scheduler - this Fase's own actual fix.
///
/// Reuses `run_ring3_timer_tick_test`'s exact spin-loop shape (Fase 79,
/// including its own `LOOP_COUNT` fix - see that function's own doc for
/// why 150,000,000, not the smaller value first tried, is what actually
/// guarantees a real tick lands during the loop), with its own distinct
/// exit code (88) so log lines are unambiguous about
/// which test produced them. The orchestration - starting a shorter
/// concurrent demo, capturing the tick budget before/after entering
/// ring-3, waiting for the demo to finish, comparing task counters -
/// lives in `scheduler::preemptive::run_concurrent_ring3_preemption_test`,
/// which takes this function's own ring-3 entry as a closure precisely
/// because `enter_ring3` is private to this module: `preemptive.rs`
/// never needs to know how to build a ring-3 program, only when to run
/// one.
pub fn run_ring3_concurrent_preemption_test() -> u64 {
    let info = gdt::ring3_info();
    let user_cs = info.user_code_selector as u64;
    let user_ss = info.user_data_selector as u64;

    let code_addr = USER_TEST_PAGE_ADDR;
    let stack_top = USER_TEST_PAGE_ADDR + 4096;

    const LOOP_COUNT: u32 = 150_000_000;
    const EXIT_CODE: u32 = 88;

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

    kprintln!("[RING3] Attempting a real ring-3 spin loop CONCURRENTLY with ring-0 preemption, for the first time...");
    serial_println!(
        "[RING3] ring3_concurrent_preemption_test entering rip={:#x} cs={:#06x} ss={:#06x} rsp={:#x} rflags={:#x} loop_count={}",
        code_addr,
        user_cs,
        user_ss,
        stack_top,
        RING3_TEST_RFLAGS,
        LOOP_COUNT
    );

    let (exit_code, budget_untouched_during_ring3, tasks_advanced_after) =
        crate::scheduler::preemptive::run_concurrent_ring3_preemption_test(|| unsafe {
            enter_ring3(code_addr, user_cs, user_ss, stack_top, RING3_TEST_RFLAGS)
        });

    kprintln!(
        "[RING3] Back in ring-0 - exit_code={} (ring-0 preemption tick budget untouched during ring-3: {}, ring-0 tasks advanced afterward: {})",
        exit_code,
        budget_untouched_during_ring3,
        tasks_advanced_after
    );
    serial_println!(
        "[RING3] ring3_concurrent_preemption_test exit_code={} budget_untouched_during_ring3={} tasks_advanced_after={}",
        exit_code,
        budget_untouched_during_ring3,
        tasks_advanced_after
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

// ---- Fase 81: a ring-3 "task" entered via the SAME switch_to/prepare_
// initial_stack bootstrap the ring-0 cooperative/preemptive schedulers
// already use, instead of enter_ring3's direct, synchronous iretq ----
//
// Every ring-3 mechanism above (run_ring3_test, run_ring3_syscall_test,
// enter_ring3/run_ring3_exit_test) enters ring-3 via a plain Rust function
// call that builds an iretq frame from its OWN arguments and executes it
// directly - simple, and correct for "call into ring-3 and wait for it to
// finish", but NOT the shape a real scheduler needs: `scheduler::
// preemptive::tick`'s own doc explains why a tick landing on ring-3 code
// currently has to be entirely invisible to it (Fase 80) - `switch_to`
// cannot safely swap out an arbitrary interrupted ring-3 context the way
// it does between two ring-0 tasks, because a ring-0 task's "resume
// point" is always a nested `switch_to` call frame buried inside some
// earlier invocation of `timer_interrupt_handler`, not an arbitrary
// instruction address at an arbitrary privilege level.
//
// This Fase does NOT yet solve that (genuine mid-flight preemption of a
// running ring-3 program is real, separate follow-on work - seeing
// EXACTLY why is the whole point of building this first). What it DOES
// prove: a ring-3 "task" can be entered the same way a ring-0 task is -
// via `switch_to` landing on a freshly prepared stack - rather than via
// enter_ring3's own bespoke calling convention. That's the necessary
// foundation: only a task that ENTERS this way could ever be resumed via
// `switch_to` from somewhere other than its own original caller later.
//
// The mechanism, mirroring `prepare_initial_stack` closely:
// `prepare_ring3_initial_stack` lays out a fresh KERNEL-side stack so
// `switch_to`'s own `ret` doesn't land on an ordinary `entry` function
// (which would run at ring-0, the existing ring-0 task shape) but on
// `ring3_entry_trampoline` instead - a naked function whose ENTIRE body
// is a bare `iretq`, correct because the SAME fake stack also has a
// complete 5-field iretq frame (RIP/CS/RFLAGS/RSP/SS) sitting exactly
// where RSP points the instant the trampoline is reached (right after
// `ret` pops the trampoline's own address). No register loads needed -
// the stack layout alone does all the work.
//
// The return trip needs its own new mechanism too, NOT `enter_ring3`'s:
// see `ring3_task_exit_entry_asm`'s own doc, plus RING3_TASK_EXIT_INT_
// VECTOR's doc in interrupts.rs, for exactly why a third IDT vector
// (0x82) rather than reusing 0x81.

static mut RING3_TASK_CALLER_RSP: u64 = 0;
static mut RING3_TASK_EXIT_CODE: u64 = 0;

/// The address `prepare_ring3_initial_stack` points `switch_to`'s own
/// `ret` at. By the time this runs, RSP already points exactly at the
/// 5-field `iretq` frame that function built (RIP, CS, RFLAGS, RSP, SS,
/// in that order, low to high) - so the entire body really is just
/// `iretq`, with no register loads first.
#[unsafe(naked)]
extern "C" fn ring3_entry_trampoline() {
    naked_asm!("iretq");
}

/// Lays out a fresh KERNEL-side stack so the FIRST `switch_to` into it
/// enters ring-3 directly via `ring3_entry_trampoline`'s `iretq`, instead
/// of "returning" into an ordinary ring-0 `entry` fn the way
/// `prepare_initial_stack` does. Same 7-quadword switch_to shape
/// (6 zeroed callee-saved slots + a return address) as that function,
/// with 5 MORE quadwords above it holding the real iretq frame fields -
/// unlike the 6 zeroed slots (whose exact sub-order never mattered, since
/// they're all zero anyway), these 5 are all genuinely different values
/// and MUST land at the exact offsets `iretq` itself expects, so they're
/// written in the exact reverse order of how `iretq` consumes them (SS
/// highest, RIP lowest of the five) - verified by hand against
/// `switch_to`'s own pop sequence before writing this, not assumed.
///
/// # Safety
/// Same preconditions as `enter_ring3`: `user_rip` must point at real,
/// executable, user-accessible code; `user_rsp` a valid ring-3 stack
/// pointer; `user_cs`/`user_ss` genuine RPL=3 selectors.
pub(crate) unsafe fn prepare_ring3_initial_stack(
    kernel_stack_top: *mut u8,
    user_rip: u64,
    user_cs: u64,
    user_ss: u64,
    user_rsp: u64,
    user_rflags: u64,
) -> u64 {
    let top = ((kernel_stack_top as u64) & !0xf) - 8;
    let mut sp = top;

    sp -= 8;
    *(sp as *mut u64) = user_ss;
    sp -= 8;
    *(sp as *mut u64) = user_rsp;
    sp -= 8;
    *(sp as *mut u64) = user_rflags;
    sp -= 8;
    *(sp as *mut u64) = user_cs;
    sp -= 8;
    *(sp as *mut u64) = user_rip;

    sp -= 8;
    *(sp as *mut u64) = ring3_entry_trampoline as *const () as u64;

    for _ in 0..6 {
        sp -= 8;
        *(sp as *mut u64) = 0;
    }

    sp
}

/// Entry point for vector 0x82 (`RING3_TASK_EXIT_INT_VECTOR`) - the
/// voluntary exit signal for a ring-3 task entered via
/// `prepare_ring3_initial_stack` + `switch_to`. Resumes whoever called
/// `switch_to` to enter this task, using `switch_to`'s OWN pop convention
/// (r15/r14/r13/r12/rbx/rbp) rather than `enter_ring3`'s.
///
/// `sti` immediately before `ret`, NOT a saved-RFLAGS `popfq` like
/// `ring3_exit_entry_asm` uses: `switch_to`'s own callers never had their
/// RFLAGS captured in the first place (`switch_to` itself doesn't push
/// them), so there's nothing for a `popfq` here to restore FROM. But the
/// underlying problem `ring3_exit_entry_asm`'s own doc describes is
/// identical: every IDT entry defaults to a 64-bit INTERRUPT gate, which
/// unconditionally clears `RFLAGS.IF` on entry - without re-enabling it
/// somehow, interrupts would stay off forever after this ran, the exact
/// silent-hang failure mode Fase 73 hit for real. `sti` fixes it
/// directly since `switch_to`'s caller had IF=1 to begin with (it was
/// ordinary, non-interrupt kernel code). x86's documented one-instruction
/// "sti shadow" (the instruction immediately after `sti` always executes
/// before any interrupt can be taken) means the final `ret` itself is
/// still guaranteed to run first - no window where a real timer tick
/// could land mid-restore.
#[unsafe(naked)]
pub(crate) extern "C" fn ring3_task_exit_entry_asm() {
    naked_asm!(
        "mov [rip + {exit_code}], rax",
        "mov rsp, [rip + {caller_rsp}]",
        "pop r15",
        "pop r14",
        "pop r13",
        "pop r12",
        "pop rbx",
        "pop rbp",
        "sti",
        "ret",
        exit_code = sym RING3_TASK_EXIT_CODE,
        caller_rsp = sym RING3_TASK_CALLER_RSP,
    );
}

/// Proves the new switch_to-bootstrap mechanism works end to end: enter
/// ring-3 via `prepare_ring3_initial_stack` + `switch_to` (NOT
/// `enter_ring3`), run a real (small) computation - not just an
/// immediate exit - then exit voluntarily via the new `int 0x82` vector
/// and resume right here, exactly as if `switch_to` itself had returned
/// normally.
///
/// The ring-3 "program" computes 5+4+3+2+1=15 in `eax` via a genuine loop
/// before exiting - deliberately more than a trivial immediate-exit
/// program, so the returned exit code is real proof the CPU executed
/// actual ring-3 instructions correctly through this NEW bootstrap path
/// (fresh kernel stack -> trampoline -> iretq -> real execution -> int
/// 0x82 -> ring3_task_exit_entry_asm's own switch_to-style return), not
/// just that some incidental value came back.
///
///   mov ecx, 5    -> B9 05 00 00 00
///   mov eax, 0    -> B8 00 00 00 00
///   add eax, ecx  -> 01 C8              (loop target, offset 10)
///   dec ecx       -> FF C9
///   jnz <loop>    -> 75 FA (rel8 = -6, back to `add eax, ecx`)
///   int 0x82      -> CD 82 (RING3_TASK_EXIT_INT_VECTOR)
pub fn run_ring3_switchto_bootstrap_test() -> u64 {
    let info = gdt::ring3_info();
    let user_cs = info.user_code_selector as u64;
    let user_ss = info.user_data_selector as u64;

    let code_addr = USER_TEST_PAGE_ADDR;
    let stack_top = USER_TEST_PAGE_ADDR + 4096;

    const EXPECTED_EXIT_CODE: u64 = 15;
    const PROGRAM: [u8; 18] = [
        0xB9, 0x05, 0x00, 0x00, 0x00, // mov ecx, 5
        0xB8, 0x00, 0x00, 0x00, 0x00, // mov eax, 0
        0x01, 0xC8, // add eax, ecx
        0xFF, 0xC9, // dec ecx
        0x75, 0xFA, // jnz -6 (back to `add eax, ecx`)
        0xCD, 0x82, // int 0x82
    ];
    unsafe {
        core::ptr::copy_nonoverlapping(PROGRAM.as_ptr(), code_addr as *mut u8, PROGRAM.len());
    }

    kprintln!("[RING3] Attempting a ring-3 task entered via the switch_to/prepare_initial_stack bootstrap (not enter_ring3) for the first time...");
    serial_println!(
        "[RING3] ring3_switchto_bootstrap_test entering rip={:#x} cs={:#06x} ss={:#06x} rsp={:#x} rflags={:#x}",
        code_addr,
        user_cs,
        user_ss,
        stack_top,
        RING3_TEST_RFLAGS
    );

    let mut kernel_stack = alloc::boxed::Box::new([0u8; 16 * 1024]);
    let kernel_stack_top = unsafe { kernel_stack.as_mut_ptr().add(16 * 1024) };
    let new_rsp = unsafe {
        prepare_ring3_initial_stack(
            kernel_stack_top,
            code_addr,
            user_cs,
            user_ss,
            stack_top,
            RING3_TEST_RFLAGS,
        )
    };

    unsafe {
        crate::scheduler::context_switch::switch_to(
            core::ptr::addr_of_mut!(RING3_TASK_CALLER_RSP),
            new_rsp,
        );
    }
    // Resumes HERE once the ring-3 program voluntarily exits via int 0x82
    // - ring3_task_exit_entry_asm's own switch_to-style ret lands right
    // after this call, exactly as if switch_to itself had returned.

    let exit_code = unsafe { core::ptr::addr_of!(RING3_TASK_EXIT_CODE).read() };
    drop(kernel_stack);

    kprintln!(
        "[RING3] Back in ring-0 via the NEW switch_to-bootstrap exit path - exit_code={} (expected {})",
        exit_code,
        EXPECTED_EXIT_CODE
    );
    serial_println!(
        "[RING3] ring3_switchto_bootstrap_test exit_code={}",
        exit_code
    );

    exit_code
}

// ---- Fase 83: COOPERATIVE ring-3 multi-tasking - two ring-3 programs
// voluntarily interleaving via a NEW yield vector, neither ever running
// to completion in one shot the way every ring-3 mechanism above does ----
//
// Mirrors this kernel's OWN ring-0 scheduling history: cooperative
// multi-tasking (Fase 15/28) came BEFORE preemptive, timer-driven
// multi-tasking (Fase 31/32) - genuine mid-flight, ASYNCHRONOUS
// preemption of ring-3 code (interrupted at an arbitrary point by a
// timer tick, not a point the program itself chose) remains real,
// separate, substantially larger follow-on work (it needs the timer
// handler itself to become a full-GPR-saving naked stub, not just the
// `bool` flag Fase 79/80 threaded through the existing one). This Fase
// takes the SAME lower-risk path the ring-0 scheduler's own history
// already validated: prove VOLUNTARY, COOPERATIVE interleaving first,
// entirely by reusing Fase 81's own already-proven switch_to-bootstrap
// mechanism, with ZERO changes to the timer interrupt path at all.
//
// The mechanism: a FOURTH dedicated DPL=3 vector (`RING3_COOP_YIELD_INT_
// VECTOR` = 0x83, interrupts.rs) whose handler, `ring3_coop_yield_entry_
// asm`, does exactly what `switch_to` itself does (push 6 callee-saved
// registers, save the resulting RSP, load a NEW RSP, pop that task's own
// 6 registers, `ret`) - except it ALSO pushes a "return address" of
// `ring3_entry_trampoline` first (the SAME trampoline Fase 81's own
// `prepare_ring3_initial_stack` targets), since unlike an ordinary
// `switch_to` call site (which already has a real return address on the
// stack from its own `call` instruction), a raw interrupt entry has none
// - manufacturing one here means a YIELDED task's own saved stack ends
// up in the EXACT SAME 12-quadword shape (6 GPRs + trampoline address +
// 5-field iretq frame) a FRESHLY bootstrapped one already has, so the
// SAME resume path (switch_to landing on the trampoline's bare `iretq`)
// correctly handles EITHER case with no special-casing.
//
// Only the SAME 6 registers `switch_to` already guarantees (rbp, rbx,
// r12-r15) survive a yield - deliberately NOT a full-GPR save (that
// remains real, separate follow-on work for genuine async preemption,
// where the interrupted program never got a chance to arrange its own
// live values into "safe" registers the way voluntary-yield code can).
// The test's own two ring-3 "programs" are written with this convention
// in mind: `bl` (the task's own identity, 0 or 1) and `r12` (a shared
// buffer address) are the only state that needs to survive a yield, and
// both are deliberately kept in the preserved set.

static mut RING3_COOP_TASK_RSP: [u64; 2] = [0, 0];
static mut RING3_COOP_CURRENT: usize = 0;

/// Entry point for vector 0x83 - a ring-3 task's voluntary "let someone
/// else run for a while" signal. Structurally its own thing, not a
/// variant of `ring3_task_exit_entry_asm` (0x82): that one discards the
/// yielding context entirely and resumes a plain Rust caller; this one
/// PRESERVES the yielding context (by manufacturing the same fake
/// "return-to-trampoline" frame `prepare_ring3_initial_stack` already
/// establishes for fresh tasks - see this section's own doc) and resumes
/// a DIFFERENT ring-3 task instead of ring-0 at all.
///
/// `mov rdi, rsp` / `call {helper}` mid-sequence is the same "call an
/// ordinary Rust function from within a naked stub" pattern `syscall_
/// entry_asm` already established (Fase 72) - `ring3_coop_yield_helper`
/// below is a completely normal (non-naked) function, so all the
/// bookkeeping logic (which task is current, updating the saved-RSP
/// table) is exactly as safe to write as any other Rust in this
/// codebase; only the raw register save/restore around it is naked asm.
/// Stack alignment verified by hand before writing this, the same
/// discipline `syscall_entry_asm`'s own doc already established: the
/// CPU's own cross-privilege interrupt entry pushes 5 fields (40 bytes),
/// this stub then pushes 7 more (trampoline address + 6 GPRs, 56 bytes) -
/// 96 bytes total, an exact multiple of 16, so the SysV-required
/// 16-byte-aligned-before-`call` invariant holds given RSP0's own
/// configured top is itself 16-aligned (already relied upon, proven
/// working, by `syscall_entry_asm`'s own identical reasoning).
#[unsafe(naked)]
pub(crate) extern "C" fn ring3_coop_yield_entry_asm() {
    naked_asm!(
        "lea rax, [rip + {trampoline}]",
        "push rax",
        "push rbp",
        "push rbx",
        "push r12",
        "push r13",
        "push r14",
        "push r15",
        "mov rdi, rsp",
        "call {helper}",
        "mov rsp, rax",
        "pop r15",
        "pop r14",
        "pop r13",
        "pop r12",
        "pop rbx",
        "pop rbp",
        "ret",
        trampoline = sym ring3_entry_trampoline,
        helper = sym ring3_coop_yield_helper,
    );
}

/// Copies the yielding task's own just-built 96-byte snapshot (6 GPRs +
/// trampoline address + 5-field iretq frame - built by the naked stub
/// above, in the exact same shape a fresh `prepare_ring3_initial_stack`
/// bootstrap already uses) OFF of RSP0 and into that task's OWN fixed,
/// dedicated parking location (`RING3_COOP_TASK_RSP[cur]`, set ONCE at
/// startup to that task's own `prepare_ring3_initial_stack`-returned
/// address and never changed afterward), alternates `RING3_COOP_CURRENT`
/// between the only two slots this first proof supports, and returns the
/// OTHER task's own fixed location - either holding a previously-yielded
/// snapshot in that same shape (copied there by ITS OWN earlier yield),
/// or, the first time it's ever resumed, its own still-untouched fresh
/// bootstrap.
///
/// **A real bug, caught by the very first boot test, not assumed
/// correct**: the first version of this function saved `current_rsp`
/// itself (a pointer INTO RSP0) into `RING3_COOP_TASK_RSP[cur]`, instead
/// of copying the bytes it points at anywhere else. That's broken
/// because RSP0 is NOT a per-task resource - the CPU reloads it fresh
/// from the TSS's own fixed field on EVERY ring3->ring0 transition
/// (never from wherever a previous handler happened to leave RSP), so a
/// SECOND task's own yield reuses the EXACT SAME memory the first task's
/// "saved" pointer still referred to, silently overwriting it before it
/// was ever read back. Caught directly by this Fase's own signature-byte
/// test: the expected `[0x41, 0x42, 0x43]` ("ABC") came back
/// `[0x41, 0x42, 0x44]` - task 0's resume showed `bl=1` (task 1's own
/// value) instead of its own `bl=0`, exactly what reading task 1's own
/// (RSP0-overlapping) snapshot instead of task 0's would produce. Fixed
/// by copying the 96 bytes into each task's own dedicated, never-shared
/// kernel stack memory (the exact fix this doc now describes) rather
/// than just remembering where they transiently were.
///
/// Deliberately hardcoded to exactly 2 tasks, alternating unconditionally.
/// A real N-task ring-3 scheduler (priority, more than 2 tasks, one task
/// exiting while another keeps running) is real, separate, more
/// substantial follow-on work; this function's only job is proving the
/// mechanism itself is correct.
extern "C" fn ring3_coop_yield_helper(current_rsp: u64) -> u64 {
    unsafe {
        let current_ptr = core::ptr::addr_of_mut!(RING3_COOP_CURRENT);
        let cur = *current_ptr;
        let tasks = core::ptr::addr_of!(RING3_COOP_TASK_RSP);
        let dest = (*tasks)[cur];
        core::ptr::copy_nonoverlapping(current_rsp as *const u8, dest as *mut u8, 96);
        let next = 1 - cur;
        *current_ptr = next;
        (*tasks)[next]
    }
}

/// Proves two ring-3 "tasks" can genuinely interleave via voluntary
/// yields, not just enter-and-run-to-completion one at a time. Both
/// tasks share the SAME instruction bytes (the same "N tasks, shared
/// code, distinct identity" shape `scheduler::preemptive`'s own
/// `preempt_task_body(id)` already established for ring-0) - each has a
/// tiny, distinct prologue (loading its own task id into `bl` and a
/// shared signature-buffer address into `r12`) before jumping into
/// shared logic, rather than needing a genuinely second user-accessible
/// code page (real, separate follow-on work - `memory::user_page`'s own
/// doc already notes this kernel has exactly one such page).
///
/// Real proof of correct interleaving, not just "nothing crashed": task
/// 0 enters first (via the ordinary Fase 81 switch_to-bootstrap), writes
/// `'A'` (0x41) to the shared buffer, then yields (int 0x83) - resuming
/// task 1 for the very first time (its own fresh bootstrap, reached ONLY
/// via a yield, never a direct switch_to call from Rust - proving the
/// yield path itself can originate a task's first run, not merely
/// hand off between two already-started ones). Task 1 writes `'B'`
/// (0x42, its own id folded into the same shared instruction via `bl`),
/// then yields BACK - resuming task 0 exactly where it left off. Task
/// 0's second run writes `'C'` (0x43) - real proof `bl` (its own task
/// id) survived the full round trip through task 1 and back - then
/// exits via the existing Fase 81 vector with `exit_code=42`. Task 1 is
/// deliberately never resumed again after its own single yield (its
/// kernel-side bootstrap stack is simply abandoned, freed once this
/// function returns) - a real N-task scheduler that keeps every task
/// alive indefinitely is the separate, more substantial follow-on work
/// this Fase's own module doc already names.
///
///   Task 0 entry (offset 0):  mov r12, sig_addr -> 49 BC + imm64
///                             mov bl, 0         -> B3 00
///                             jmp +0x12         -> EB 12  (-> offset 32)
///   Task 1 entry (offset 16): mov r12, sig_addr -> 49 BC + imm64
///                             mov bl, 1         -> B3 01
///                             jmp +0x02         -> EB 02  (-> offset 32)
///   Shared body (offset 32):  mov al, bl        -> 8A C3
///                             add al, 0x41      -> 04 41
///                             mov [r12+rbx], al -> 41 88 04 1C
///                             int 0x83 (yield)  -> CD 83
///                             mov al, bl        -> 8A C3
///                             add al, 0x43      -> 04 43
///                             mov [r12+2], al   -> 41 88 44 24 02
///                             mov eax, 42       -> B8 2A 00 00 00
///                             int 0x82 (exit)   -> CD 82
pub fn run_ring3_cooperative_test() -> (u64, [u8; 3]) {
    let info = gdt::ring3_info();
    let user_cs = info.user_code_selector as u64;
    let user_ss = info.user_data_selector as u64;

    let code_addr = USER_TEST_PAGE_ADDR;
    const SIG_OFFSET: u64 = 600;
    let sig_addr = code_addr + SIG_OFFSET;
    let sig_bytes = sig_addr.to_le_bytes();

    let mut program = [0u8; 58];
    program[0] = 0x49;
    program[1] = 0xBC; // mov r12, imm64
    program[2..10].copy_from_slice(&sig_bytes);
    program[10] = 0xB3;
    program[11] = 0x00; // mov bl, 0
    program[12] = 0xEB;
    program[13] = 0x12; // jmp +0x12 -> offset 32
    program[16] = 0x49;
    program[17] = 0xBC; // mov r12, imm64
    program[18..26].copy_from_slice(&sig_bytes);
    program[26] = 0xB3;
    program[27] = 0x01; // mov bl, 1
    program[28] = 0xEB;
    program[29] = 0x02; // jmp +0x02 -> offset 32
    program[32] = 0x8A;
    program[33] = 0xC3; // mov al, bl
    program[34] = 0x04;
    program[35] = 0x41; // add al, 0x41
    program[36] = 0x41;
    program[37] = 0x88;
    program[38] = 0x04;
    program[39] = 0x1C; // mov [r12+rbx], al
    program[40] = 0xCD;
    program[41] = 0x83; // int 0x83 (yield)
    program[42] = 0x8A;
    program[43] = 0xC3; // mov al, bl
    program[44] = 0x04;
    program[45] = 0x43; // add al, 0x43
    program[46] = 0x41;
    program[47] = 0x88;
    program[48] = 0x44;
    program[49] = 0x24;
    program[50] = 0x02; // mov [r12+2], al
    program[51] = 0xB8;
    program[52] = 0x2A;
    program[53] = 0x00;
    program[54] = 0x00;
    program[55] = 0x00; // mov eax, 42
    program[56] = 0xCD;
    program[57] = 0x82; // int 0x82 (exit)

    unsafe {
        core::ptr::copy_nonoverlapping(program.as_ptr(), code_addr as *mut u8, program.len());
        core::ptr::write_bytes(sig_addr as *mut u8, 0, 3);
    }

    let task0_entry = code_addr;
    let task1_entry = code_addr + 16;
    let task0_stack_top = code_addr + 2048;
    let task1_stack_top = code_addr + 3072;

    let mut kernel_stack0 = alloc::boxed::Box::new([0u8; 16 * 1024]);
    let mut kernel_stack1 = alloc::boxed::Box::new([0u8; 16 * 1024]);
    let k0_top = unsafe { kernel_stack0.as_mut_ptr().add(16 * 1024) };
    let k1_top = unsafe { kernel_stack1.as_mut_ptr().add(16 * 1024) };

    let rsp0 = unsafe {
        prepare_ring3_initial_stack(
            k0_top,
            task0_entry,
            user_cs,
            user_ss,
            task0_stack_top,
            RING3_TEST_RFLAGS,
        )
    };
    let rsp1 = unsafe {
        prepare_ring3_initial_stack(
            k1_top,
            task1_entry,
            user_cs,
            user_ss,
            task1_stack_top,
            RING3_TEST_RFLAGS,
        )
    };

    unsafe {
        let tasks = core::ptr::addr_of_mut!(RING3_COOP_TASK_RSP);
        (*tasks)[0] = rsp0;
        (*tasks)[1] = rsp1;
        *core::ptr::addr_of_mut!(RING3_COOP_CURRENT) = 0;
    }

    kprintln!(
        "[RING3] Attempting two ring-3 tasks cooperatively interleaving via a new yield vector (0x83)..."
    );
    serial_println!(
        "[RING3] ring3_cooperative_test task0_entry={:#x} task1_entry={:#x} sig_addr={:#x}",
        task0_entry,
        task1_entry,
        sig_addr
    );

    unsafe {
        crate::scheduler::context_switch::switch_to(
            core::ptr::addr_of_mut!(RING3_TASK_CALLER_RSP),
            rsp0,
        );
    }
    // Resumes HERE once task 0 exits via int 0x82, after its own real
    // round trip through task 1 and back.

    let exit_code = unsafe { core::ptr::addr_of!(RING3_TASK_EXIT_CODE).read() };
    let mut sig = [0u8; 3];
    unsafe {
        sig[0] = core::ptr::read_volatile(sig_addr as *const u8);
        sig[1] = core::ptr::read_volatile((sig_addr + 1) as *const u8);
        sig[2] = core::ptr::read_volatile((sig_addr + 2) as *const u8);
    }

    drop(kernel_stack0);
    drop(kernel_stack1);

    kprintln!(
        "[RING3] Back in ring-0 - cooperative test exit_code={} signature={:02x?} (expected [41, 42, 43] = \"ABC\")",
        exit_code,
        sig
    );
    serial_println!(
        "[RING3] ring3_cooperative_test exit_code={} signature={:02x?}",
        exit_code,
        sig
    );

    (exit_code, sig)
}

/// Fase 85: proves genuine, INVOLUNTARY (timer-driven) ring-3 preemption.
/// A real hardware tick, landing at an arbitrary point this program did
/// NOT choose, saves this program's COMPLETE register state and
/// correctly restores it - not just the 6 registers `switch_to`/Fase
/// 83's own cooperative yield already protect. See `scheduler::ring3_
/// preempt`'s own module doc for the actual mechanism (the timer stub's
/// full 15-GPR save, Fase 84, now genuinely used for the first time).
///
/// The ring-3 program loads 5 DELIBERATELY "caller-saved" registers
/// (`eax`/`ecx`/`edx`/`esi`/`edi` - none of `switch_to`'s own preserved
/// set of `rbp`/`rbx`/`r12`-`r15`, so this test is only meaningful if
/// the NEW full-context mechanism is what preserved them, not an
/// incidental side effect of an already-proven, unrelated one) with
/// distinct patterns, spins long enough for a real tick to land
/// (reusing Fase 79's own `LOOP_COUNT=150_000_000`, already
/// empirically proven to reliably span multiple real tick periods - see
/// that Fase's own doc for the reliability bug that established this
/// exact value), then XORs all 5 together into `eax` and exits via the
/// existing Fase 73 mechanism. `r8d` (not one of the 5 checked
/// registers, and not in `switch_to`'s own preserved set either) is
/// purely the loop counter, deliberately distinct from all 5 checked
/// registers so the loop itself never touches what's being verified.
///
///   mov eax, 0x11111111 -> B8 11 11 11 11
///   mov ecx, 0x22222222 -> B9 22 22 22 22
///   mov edx, 0x33333333 -> BA 33 33 33 33
///   mov esi, 0x44444444 -> BE 44 44 44 44
///   mov edi, 0x55555555 -> BF 55 55 55 55
///   mov r8d, 150000000  -> 41 B8 + imm32        (loop target, offset 31)
///   dec r8d             -> 41 FF C8
///   jnz <dec r8d>        -> 75 FB (rel8 = -5)
///   xor eax, ecx         -> 31 C8
///   xor eax, edx         -> 31 D0
///   xor eax, esi         -> 31 F0
///   xor eax, edi         -> 31 F8                (checksum: 0x11111111)
///   int 0x81             -> CD 81 (Fase 73's own exit vector)
pub fn run_ring3_full_preempt_test() -> (u64, bool) {
    let info = gdt::ring3_info();
    let user_cs = info.user_code_selector as u64;
    let user_ss = info.user_data_selector as u64;

    let code_addr = USER_TEST_PAGE_ADDR;
    let stack_top = USER_TEST_PAGE_ADDR + 4096;

    const EXPECTED_CHECKSUM: u64 = 0x1111_1111;
    const LOOP_COUNT: u32 = 150_000_000;

    let mut program = [0u8; 46];
    program[0] = 0xB8;
    program[1..5].copy_from_slice(&0x1111_1111u32.to_le_bytes());
    program[5] = 0xB9;
    program[6..10].copy_from_slice(&0x2222_2222u32.to_le_bytes());
    program[10] = 0xBA;
    program[11..15].copy_from_slice(&0x3333_3333u32.to_le_bytes());
    program[15] = 0xBE;
    program[16..20].copy_from_slice(&0x4444_4444u32.to_le_bytes());
    program[20] = 0xBF;
    program[21..25].copy_from_slice(&0x5555_5555u32.to_le_bytes());
    program[25] = 0x41;
    program[26] = 0xB8;
    program[27..31].copy_from_slice(&LOOP_COUNT.to_le_bytes());
    program[31] = 0x41;
    program[32] = 0xFF;
    program[33] = 0xC8; // dec r8d
    program[34] = 0x75;
    program[35] = 0xFB; // jnz -5 (back to `dec r8d`)
    program[36] = 0x31;
    program[37] = 0xC8; // xor eax, ecx
    program[38] = 0x31;
    program[39] = 0xD0; // xor eax, edx
    program[40] = 0x31;
    program[41] = 0xF0; // xor eax, esi
    program[42] = 0x31;
    program[43] = 0xF8; // xor eax, edi
    program[44] = 0xCD;
    program[45] = 0x81; // int 0x81

    unsafe {
        core::ptr::copy_nonoverlapping(program.as_ptr(), code_addr as *mut u8, program.len());
    }

    kprintln!(
        "[RING3] Attempting genuine timer-driven full-register preemption for the first time..."
    );
    serial_println!(
        "[RING3] ring3_full_preempt_test entering rip={:#x} cs={:#06x} ss={:#06x} rsp={:#x} rflags={:#x} loop_count={}",
        code_addr,
        user_cs,
        user_ss,
        stack_top,
        RING3_TEST_RFLAGS,
        LOOP_COUNT
    );

    let (exit_code, intercepted) = crate::scheduler::ring3_preempt::run_intercepting(|| unsafe {
        enter_ring3(code_addr, user_cs, user_ss, stack_top, RING3_TEST_RFLAGS)
    });

    kprintln!(
        "[RING3] Back in ring-0 - full_preempt_test exit_code={:#x} (expected {:#x}) intercepted={}",
        exit_code,
        EXPECTED_CHECKSUM,
        intercepted
    );
    serial_println!(
        "[RING3] ring3_full_preempt_test exit_code={:#x} intercepted={}",
        exit_code,
        intercepted
    );

    (exit_code, intercepted)
}

/// Builds a fresh, never-yet-run ring-3 task's own 160-byte context
/// buffer, in the EXACT shape `timer_interrupt_entry_asm`'s own epilogue
/// expects to resume from after `scheduler::ring3_mt::tick` hands it
/// back (see that function's own doc, and `interrupts.rs`'s naked stub,
/// for the full byte layout): 15 zeroed GPR slots (their values don't
/// matter before the task's own first instruction overwrites them, the
/// same reasoning `prepare_ring3_initial_stack`'s own 6 zeroed
/// callee-saved slots already rely on) followed by a real 5-field
/// `iretq` frame - RIP/CS/RFLAGS/RSP/SS, in the exact field order
/// `iretq` consumes them (the same offsets `ring3_preempt`'s own
/// 160-byte snapshot already uses: 0..120 = 15 GPRs, 120=RIP, 128=CS,
/// 136=RFLAGS, 144=RSP, 152=SS).
///
/// Unlike Fase 81's own `prepare_ring3_initial_stack` (built for
/// `switch_to`'s `ret`-based, 6-callee-saved-register convention, which
/// needs a manufactured "return-to-trampoline" address as an
/// indirection step before it can reach a real `iretq`), this needs NO
/// trampoline at all: the timer stub's own epilogue reaches `iretq`
/// DIRECTLY after popping the 15 (here: zeroed) GPRs, with no
/// intermediate `ret` to redirect first - a genuine simplification
/// Fase 86 gets for free specifically because it resumes via the timer
/// stub's `iretq`-based tail rather than `switch_to`'s `ret`-based one.
fn prepare_ring3_mt_task_ctx(
    entry: u64,
    user_cs: u64,
    user_ss: u64,
    stack_top: u64,
    rflags: u64,
) -> [u8; 160] {
    let mut ctx = [0u8; 160];
    ctx[120..128].copy_from_slice(&entry.to_le_bytes());
    ctx[128..136].copy_from_slice(&user_cs.to_le_bytes());
    ctx[136..144].copy_from_slice(&rflags.to_le_bytes());
    ctx[144..152].copy_from_slice(&stack_top.to_le_bytes());
    ctx[152..160].copy_from_slice(&user_ss.to_le_bytes());
    ctx
}

/// Fase 86: the real, larger step `ring3_preempt`'s own module doc
/// (Fase 85) named as the necessary follow-on - actually running a
/// DIFFERENT ring-3 program in the gap a preempted one leaves behind,
/// not just resuming the SAME one. See `scheduler::ring3_mt`'s own
/// module doc for the round-robin mechanism; this function builds the
/// two ring-3 programs and drives the experiment end to end.
///
/// **Task 0** (entered normally via the existing, unmodified
/// `enter_ring3`/`int 0x81` exit mechanism - EXACTLY Fase 85's own test
/// program, reused verbatim except for its 5 immediate constants,
/// chosen with a `0x1...` prefix so a wrong-task-resumed bug would show
/// up as an immediately wrong, distinguishable value): loads 5
/// "caller-saved" registers, spins Fase 79/85's own already-proven
/// `LOOP_COUNT=150_000_000` (reliably spanning multiple real tick
/// periods), XORs them into `eax`, exits via `int 0x81`.
///
/// **Task 1** (entered ONLY by `scheduler::ring3_mt`'s own round-robin
/// switching, cold, via `prepare_ring3_mt_task_ctx` above - it never
/// runs via `enter_ring3` at all): the IDENTICAL shape with a `0x2...`
/// prefix instead, and a MUCH smaller loop count, so it reliably
/// finishes its own checksum early and spends the rest of the
/// experiment just spinning (`jmp $`, harmless, and critically
/// non-destructive of its own `eax`) rather than racing task 0's own
/// much larger loop. Task 1 deliberately never exits on its own - there
/// is no way for it to signal completion the way task 0 does, since two
/// tasks both racing for the single existing `int 0x81`/
/// `RING3_RETURN_RSP` slot would break the "the kernel regains control
/// exactly once, when task 0 itself decides to" invariant every earlier
/// ring3 test already relies on. Instead, verification reads task 1's
/// own LAST-saved `eax` directly out of its dedicated context buffer
/// (`scheduler::ring3_mt::run_multitasking`'s own return value) - valid
/// precisely because `jmp $` never touches `eax`, so whatever value is
/// sitting there the LAST time task 1 happens to be preempted (virtually
/// certain to be well after it finished its own tiny loop, given how
/// much smaller it is than task 0's) is its real, final checksum.
///
/// Deliberately does NOT prove a general N-task scheduler, priorities,
/// or independent per-task exit - exactly like Fase 83's own
/// cooperative test, this hardcodes 2 tasks and one one-directional
/// completion signal (task 0's alone). A fully general ring-3 scheduler
/// remains real, separate, larger follow-on work.
pub fn run_ring3_mt_test() -> (u64, u32, usize) {
    let info = gdt::ring3_info();
    let user_cs = info.user_code_selector as u64;
    let user_ss = info.user_data_selector as u64;

    let code_addr = USER_TEST_PAGE_ADDR;
    let task0_entry = code_addr;
    let task1_entry = code_addr + 128;
    let task0_stack_top = code_addr + 2048;
    let task1_stack_top = code_addr + 3072;

    const TASK0_EXPECTED: u32 = 0x1000_0004;
    const TASK1_EXPECTED: u32 = 0x2000_0004;
    const TASK0_LOOP_COUNT: u32 = 150_000_000;
    const TASK1_LOOP_COUNT: u32 = 10_000_000;

    let mut task0 = [0u8; 46];
    task0[0] = 0xB8;
    task0[1..5].copy_from_slice(&0x1000_0000u32.to_le_bytes());
    task0[5] = 0xB9;
    task0[6..10].copy_from_slice(&0x1000_0001u32.to_le_bytes());
    task0[10] = 0xBA;
    task0[11..15].copy_from_slice(&0x1000_0002u32.to_le_bytes());
    task0[15] = 0xBE;
    task0[16..20].copy_from_slice(&0x1000_0003u32.to_le_bytes());
    task0[20] = 0xBF;
    task0[21..25].copy_from_slice(&0x1000_0004u32.to_le_bytes());
    task0[25] = 0x41;
    task0[26] = 0xB8;
    task0[27..31].copy_from_slice(&TASK0_LOOP_COUNT.to_le_bytes());
    task0[31] = 0x41;
    task0[32] = 0xFF;
    task0[33] = 0xC8; // dec r8d
    task0[34] = 0x75;
    task0[35] = 0xFB; // jnz -5 (back to `dec r8d`)
    task0[36] = 0x31;
    task0[37] = 0xC8; // xor eax, ecx
    task0[38] = 0x31;
    task0[39] = 0xD0; // xor eax, edx
    task0[40] = 0x31;
    task0[41] = 0xF0; // xor eax, esi
    task0[42] = 0x31;
    task0[43] = 0xF8; // xor eax, edi
    task0[44] = 0xCD;
    task0[45] = 0x81; // int 0x81 (Fase 73's own exit vector)

    let mut task1 = [0u8; 46];
    task1[0] = 0xB8;
    task1[1..5].copy_from_slice(&0x2000_0000u32.to_le_bytes());
    task1[5] = 0xB9;
    task1[6..10].copy_from_slice(&0x2000_0001u32.to_le_bytes());
    task1[10] = 0xBA;
    task1[11..15].copy_from_slice(&0x2000_0002u32.to_le_bytes());
    task1[15] = 0xBE;
    task1[16..20].copy_from_slice(&0x2000_0003u32.to_le_bytes());
    task1[20] = 0xBF;
    task1[21..25].copy_from_slice(&0x2000_0004u32.to_le_bytes());
    task1[25] = 0x41;
    task1[26] = 0xB8;
    task1[27..31].copy_from_slice(&TASK1_LOOP_COUNT.to_le_bytes());
    task1[31] = 0x41;
    task1[32] = 0xFF;
    task1[33] = 0xC8; // dec r8d
    task1[34] = 0x75;
    task1[35] = 0xFB; // jnz -5 (back to `dec r8d`)
    task1[36] = 0x31;
    task1[37] = 0xC8; // xor eax, ecx
    task1[38] = 0x31;
    task1[39] = 0xD0; // xor eax, edx
    task1[40] = 0x31;
    task1[41] = 0xF0; // xor eax, esi
    task1[42] = 0x31;
    task1[43] = 0xF8; // xor eax, edi
    task1[44] = 0xEB;
    task1[45] = 0xFE; // jmp $ (never exits - see this fn's own doc)

    unsafe {
        core::ptr::copy_nonoverlapping(task0.as_ptr(), task0_entry as *mut u8, task0.len());
        core::ptr::copy_nonoverlapping(task1.as_ptr(), task1_entry as *mut u8, task1.len());
    }

    let task1_ctx = prepare_ring3_mt_task_ctx(
        task1_entry,
        user_cs,
        user_ss,
        task1_stack_top,
        RING3_TEST_RFLAGS,
    );

    kprintln!(
        "[RING3] Attempting genuine multi-task ring-3 scheduling - two DIFFERENT programs alternating via involuntary timer ticks..."
    );
    serial_println!(
        "[RING3] ring3_mt_test task0_entry={:#x} task1_entry={:#x} task0_loop={} task1_loop={}",
        task0_entry,
        task1_entry,
        TASK0_LOOP_COUNT,
        TASK1_LOOP_COUNT
    );

    let (task0_exit_code, task1_last_eax, switch_count) =
        crate::scheduler::ring3_mt::run_multitasking(task1_ctx, || unsafe {
            enter_ring3(
                task0_entry,
                user_cs,
                user_ss,
                task0_stack_top,
                RING3_TEST_RFLAGS,
            )
        });

    kprintln!(
        "[RING3] Back in ring-0 - mt_test task0_checksum={:#x} (expected {:#x}) task1_last_eax={:#x} (expected {:#x}) switch_count={}",
        task0_exit_code,
        TASK0_EXPECTED,
        task1_last_eax,
        TASK1_EXPECTED,
        switch_count
    );
    serial_println!(
        "[RING3] ring3_mt_test task0_checksum={:#x} task1_last_eax={:#x} switch_count={}",
        task0_exit_code,
        task1_last_eax,
        switch_count
    );

    (task0_exit_code, task1_last_eax, switch_count)
}
