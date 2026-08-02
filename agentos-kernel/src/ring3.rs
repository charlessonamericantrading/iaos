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

use crate::memory::user_page::{
    USER_DISK_PROGRAM_PAGE_ADDR, USER_DISK_PROGRAM_STACK_ADDR, USER_TEST_PAGE_ADDR,
};
use crate::{fat12, gdt, kprintln, serial_println, shell};
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
// The test's own ring-3 "programs" (Fase 94: 3 of them, generalized from
// the original 2) are written with this convention in mind: `bl` (the
// task's own identity) and `r12` (a shared buffer address) are the only
// state that needs to survive a yield, and both are deliberately kept in
// the preserved set.

/// Fase 94: real, fixed headroom for how many cooperative-yield tasks
/// this mechanism can track at once - the same "genuine limit, not a
/// shortcut" reasoning `scheduler::ring3_mt::RING3_MT_MAX_TASKS` (Fase
/// 91) already established, mirrored here for consistency. NOT the
/// number actually active in a given run - that's `RING3_COOP_ACTIVE_
/// TASKS`, set fresh by each test.
const RING3_COOP_MAX_TASKS: usize = 4;

static mut RING3_COOP_TASK_RSP: [u64; RING3_COOP_MAX_TASKS] = [0; RING3_COOP_MAX_TASKS];
static mut RING3_COOP_ACTIVE_TASKS: usize = 0;
static mut RING3_COOP_CURRENT: usize = 0;

/// Fase 95: each slot's own priority, as `Priority as u8` (lower value =
/// actually higher priority, the SAME convention `scheduler::preemptive`
/// (Fase 87) and `scheduler::ring3_mt` (Fase 92) already share). Set
/// fresh by every test, read by the yield helper's own tier search.
static mut RING3_COOP_TASK_PRIORITY: [u8; RING3_COOP_MAX_TASKS] = [0; RING3_COOP_MAX_TASKS];

/// Fase 96: which slots have voluntarily retired for good via
/// `ring3_coop_task_done_helper` below - task 0's own slot is NEVER set
/// here, since task 0 always exits through the separate, unmodified
/// `RING3_TASK_EXIT_INT_VECTOR` (`int 0x82`) path instead. Reset to all-
/// `false` by every test, the same "task 0 never marked done, so the
/// selection search can never come up empty" invariant `scheduler::
/// ring3_mt::RING3_MT_TASK_DONE` (Fase 93) already established for the
/// OTHER mechanism.
static mut RING3_COOP_TASK_DONE: [bool; RING3_COOP_MAX_TASKS] = [false; RING3_COOP_MAX_TASKS];

// Fase 97: `ring3_coop_yield_helper` and `ring3_coop_task_done_helper`
// below now call `scheduler::tier_select::select_next` directly - this
// used to be its own local copy of that exact algorithm (ported from
// `scheduler::ring3_mt::select_next` across Fase 95/96), byte-for-byte
// identical except for which `MAX_TASKS` constant it closed over. See
// `tier_select`'s own module doc for why that made it a real, safe
// unification target rather than just a coincidence.

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
/// address and never changed afterward), advances `RING3_COOP_CURRENT`
/// to the NEXT task in round-robin order (Fase 94: `(cur + 1) %
/// active_tasks`, generalized from the original hardcoded `1 - cur`
/// two-slot toggle), and returns that task's own fixed location - either
/// holding a previously-yielded snapshot in that same shape (copied
/// there by ITS OWN earlier yield), or, the first time it's ever
/// resumed, its own still-untouched fresh bootstrap.
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
/// **Fase 94 generalizes this from exactly 2 hardcoded tasks to a real
/// N-task round-robin** (still bounded by `RING3_COOP_MAX_TASKS`, the
/// same "real, fixed headroom" reasoning `scheduler::ring3_mt::
/// RING3_MT_MAX_TASKS` already established for the OTHER, involuntary-
/// timer-driven ring-3 scheduling mechanism) - this mechanism still has
/// no early-exit-before-the-caller-decides (every task besides the one
/// that eventually calls `int 0x82` is simply abandoned once the test
/// function returns), real, separate, further follow-on work.
///
/// **Fase 95 adds real priority**, porting the exact same tier-search
/// algorithm `scheduler::ring3_mt::tick` already uses since Fase 92
/// (itself ported from `scheduler::preemptive::tick`, Fase 87): find the
/// highest actual priority among active slots (lowest `Priority as u8`
/// value wins), then round-robin only within that top tier, starting
/// right after `cur` and wrapping. Reduces to the OLD plain round-robin
/// byte-for-byte whenever every active task shares one priority (Fase
/// 94's own test still passes one equal priority for all 3, preserving
/// its exact behavior).
///
/// **A safety property genuinely different from `ring3_mt`'s own,
/// because this mechanism is VOLUNTARY, not involuntary-timer-driven**:
/// `scheduler::ring3_mt`'s own tick fires on every real hardware
/// interrupt regardless of what any task does, so Fase 92's own hang
/// risk was "some OTHER task could out-prioritize task 0 forever."
/// HERE, nothing ever transfers control except a task's OWN explicit
/// `int 0x83` - so if task 0 alone holds the top priority tier and it
/// voluntarily yields, the tier search finds no OTHER slot at that same
/// tier, and round-robins all the way back around to task 0 ITSELF -
/// a safe, immediate self-resume, not a hang. This is exactly the
/// property `run_priority_cooperative_test` proves: yielding while
/// holding the sole top priority never actually hands control away.
///
/// **Fase 96: now shares its tier-search-then-round-robin logic with
/// `ring3_coop_task_done_helper` below** via `coop_select_next` - the
/// same factoring `scheduler::ring3_mt`'s own `tick`/`task_done` pair
/// already established in Fase 93, so this yield path and the new
/// permanent-retirement path can never drift out of sync on how they
/// pick the next task.
extern "C" fn ring3_coop_yield_helper(current_rsp: u64) -> u64 {
    unsafe {
        let current_ptr = core::ptr::addr_of_mut!(RING3_COOP_CURRENT);
        let cur = *current_ptr;
        let tasks = core::ptr::addr_of!(RING3_COOP_TASK_RSP);
        let dest = (*tasks)[cur];
        core::ptr::copy_nonoverlapping(current_rsp as *const u8, dest as *mut u8, 96);
        let active_tasks = *core::ptr::addr_of!(RING3_COOP_ACTIVE_TASKS);
        let prios: &[u8; RING3_COOP_MAX_TASKS] = &*core::ptr::addr_of!(RING3_COOP_TASK_PRIORITY);
        let done: &[bool; RING3_COOP_MAX_TASKS] = &*core::ptr::addr_of!(RING3_COOP_TASK_DONE);
        let next = crate::scheduler::tier_select::select_next(active_tasks, cur, prios, done);
        *current_ptr = next;
        (*tasks)[next]
    }
}

/// Fase 96: entry point for vector 0x85 - a NON-task-0 cooperative-yield
/// task's voluntary "I'm done, stop scheduling me for good" signal, as
/// opposed to `ring3_coop_yield_entry_asm`'s own voluntary YIELD (which
/// resumes the SAME task again later). Byte-for-byte the same naked-asm
/// shape as `ring3_coop_yield_entry_asm` (manufacture a trampoline
/// return address, push the same 6 callee-saved registers, call a
/// helper, adopt whatever RSP it returns, pop the same 6, `ret`) -
/// reused verbatim since this vector's whole point is interoperating
/// with the SAME 96-byte context format the yield path already uses,
/// not `scheduler::ring3_mt`'s own 160-byte one.
#[unsafe(naked)]
pub(crate) extern "C" fn ring3_coop_task_done_entry_asm() {
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
        helper = sym ring3_coop_task_done_helper,
    );
}

/// Saves the retiring task's own 96-byte snapshot into its dedicated
/// buffer (identical copy to `ring3_coop_yield_helper`'s own - no
/// poisoning here, unlike `scheduler::ring3_mt`'s own `tick`/`task_done`,
/// since this mechanism never relied on poisoning to catch a reuse bug
/// the way that one's own RSP0 history did), marks it done in `RING3_
/// COOP_TASK_DONE`, and picks the next task via the SAME `coop_select_
/// next` the yield path uses - the one difference from a plain yield.
/// Never called for task 0 (see `RING3_COOP_TASK_DONE`'s own doc for
/// why task 0's slot must never be marked done) - by construction, only
/// a ring-3 PROGRAM built to execute `int 0x85` can ever reach this at
/// all, and no test in this codebase puts that instruction into task
/// 0's own program.
extern "C" fn ring3_coop_task_done_helper(current_rsp: u64) -> u64 {
    unsafe {
        let current_ptr = core::ptr::addr_of_mut!(RING3_COOP_CURRENT);
        let cur = *current_ptr;
        let tasks = core::ptr::addr_of!(RING3_COOP_TASK_RSP);
        let dest = (*tasks)[cur];
        core::ptr::copy_nonoverlapping(current_rsp as *const u8, dest as *mut u8, 96);

        let done_slots = core::ptr::addr_of_mut!(RING3_COOP_TASK_DONE);
        (*done_slots)[cur] = true;

        let active_tasks = *core::ptr::addr_of!(RING3_COOP_ACTIVE_TASKS);
        let prios: &[u8; RING3_COOP_MAX_TASKS] = &*core::ptr::addr_of!(RING3_COOP_TASK_PRIORITY);
        let done: &[bool; RING3_COOP_MAX_TASKS] = &*core::ptr::addr_of!(RING3_COOP_TASK_DONE);
        let next = crate::scheduler::tier_select::select_next(active_tasks, cur, prios, done);

        *current_ptr = next;
        (*tasks)[next]
    }
}

/// Fase 94: builds one of `run_ring3_cooperative_test`'s own 16-byte
/// per-task prologues (`mov r12, sig_addr` / `mov bl, task_id` / `jmp
/// rel8`) - COMPUTING the trailing `jmp`'s relative offset from where
/// this prologue sits and where the shared body starts, rather than
/// hand-deriving a separate hex constant per task the way the original
/// 2-task version did. `rel8` is relative to the address right after the
/// 2-byte `jmp` instruction itself (`prologue_offset + 14`, since the
/// `mov r12,imm64`/`mov bl,imm8` pair ahead of it are 10+2=12 bytes) -
/// asserts the result fits a short (single-byte, forward-only) jump
/// rather than silently truncating or wrapping if a future caller ever
/// spaced prologues/body far enough apart to need a near/far jump
/// instead.
fn build_coop_task_prologue(
    task_id: u8,
    sig_addr: u64,
    prologue_offset: usize,
    shared_body_offset: usize,
) -> [u8; 16] {
    let mut p = [0u8; 16];
    p[0] = 0x49;
    p[1] = 0xBC; // mov r12, imm64
    p[2..10].copy_from_slice(&sig_addr.to_le_bytes());
    p[10] = 0xB3;
    p[11] = task_id; // mov bl, task_id
    p[12] = 0xEB; // jmp rel8
    let next_ip = prologue_offset + 14;
    let rel8 = shared_body_offset as isize - next_ip as isize;
    assert!(
        (0..=127).contains(&rel8),
        "build_coop_task_prologue: rel8={rel8} out of short-forward-jump range"
    );
    p[13] = rel8 as u8;
    p
}

/// Proves N ring-3 "tasks" can genuinely interleave via voluntary
/// yields, not just enter-and-run-to-completion one at a time. All
/// tasks share the SAME instruction bytes (the same "N tasks, shared
/// code, distinct identity" shape `scheduler::preemptive`'s own
/// `preempt_task_body(id)` already established for ring-0) - each has a
/// tiny, distinct prologue (loading its own task id into `bl` and a
/// shared signature-buffer address into `r12`) before jumping into
/// shared logic, rather than needing a genuinely second user-accessible
/// code page (real, separate follow-on work - `memory::user_page`'s own
/// doc already notes this kernel has exactly one such page).
///
/// **Fase 94 generalizes this from exactly 2 hardcoded tasks to 3** -
/// not an arbitrary bigger number, but the same "smallest scale that
/// actually proves the mechanism isn't secretly still a hardcoded pair"
/// reasoning Fase 91 already applied when generalizing `scheduler::
/// ring3_mt` (the OTHER, involuntary-timer-driven ring-3 scheduling
/// mechanism) the identical way.
///
/// Real proof of correct interleaving, not just "nothing crashed": task
/// 0 enters first (via the ordinary Fase 81 switch_to-bootstrap), writes
/// `'A'` (0x41) to `sig[0]`, then yields (int 0x83) - resuming task 1 for
/// the very first time (its own fresh bootstrap, reached ONLY via a
/// yield, never a direct switch_to call from Rust). Task 1 writes `'B'`
/// (0x42) to `sig[1]`, then yields - resuming task 2 (NOT back to task
/// 0 - the real, new proof that round-robin genuinely visits every
/// task in turn, not just ping-pongs between the first two). Task 2
/// writes `'C'` (0x43) to `sig[2]`, then yields - wrapping back around to
/// task 0, which resumes exactly where it left off. Task 0's second run
/// writes a DELIBERATELY distinct completion marker, `sig[3] = bl + 0x58`
/// (`0x58` for task 0's own `bl=0`, chosen specifically so it can never
/// collide with any of the `'A'`/`'B'`/`'C'` first-round values) - real
/// proof `bl` (its own task id) survived the full round trip through
/// BOTH other tasks and back - then exits via the existing Fase 81
/// vector with `exit_code=42`. Tasks 1 and 2 are deliberately never
/// resumed again after their own single yield (their kernel-side
/// bootstrap stacks are simply abandoned, freed once this function
/// returns) - real per-task early-exit/retirement (mirroring what Fase
/// 93 added to the OTHER, involuntary-timer mechanism) remains real,
/// separate, further follow-on work for this cooperative one.
///
/// Each 16-byte prologue slot is built by `build_coop_task_prologue`,
/// which COMPUTES the correct `jmp rel8` target from the prologue's own
/// offset and the shared body's offset, rather than 3 separately
/// hand-derived hex constants - deliberately safer given this whole
/// class of hand-encoded x86 has bitten this codebase before (Fase 83's
/// own RSP0 bug, among others).
///
///   Shared body (offset 48):  mov al, bl          -> 8A C3
///                              add al, 0x41        -> 04 41
///                              mov [r12+rbx], al   -> 41 88 04 1C
///                              int 0x83 (yield)    -> CD 83
///                              mov al, bl          -> 8A C3
///                              add al, 0x58         -> 04 58
///                              mov [r12+3], al      -> 41 88 44 24 03
///                              mov eax, 42          -> B8 2A 00 00 00
///                              int 0x82 (exit)      -> CD 82
const RING3_COOP_SHARED_BODY_OFFSET: usize = 48;
const RING3_COOP_TASK0_ENTRY_OFFSET: usize = 0;
const RING3_COOP_TASK1_ENTRY_OFFSET: usize = 16;
const RING3_COOP_TASK2_ENTRY_OFFSET: usize = 32;

/// Fase 95: builds the 3-task cooperative-yield test program once,
/// shared by `run_ring3_cooperative_test` (Fase 94's own round-robin
/// proof) and `run_priority_cooperative_test` below - the program BYTES
/// are identical either way (every task's own prologue and the shared
/// body), only the PRIORITIES assigned at setup time differ, which is a
/// scheduling-metadata concern, not a program-construction one.
fn build_cooperative_test_program(sig_addr: u64) -> [u8; RING3_COOP_SHARED_BODY_OFFSET + 26] {
    let prologue0 = build_coop_task_prologue(
        0,
        sig_addr,
        RING3_COOP_TASK0_ENTRY_OFFSET,
        RING3_COOP_SHARED_BODY_OFFSET,
    );
    let prologue1 = build_coop_task_prologue(
        1,
        sig_addr,
        RING3_COOP_TASK1_ENTRY_OFFSET,
        RING3_COOP_SHARED_BODY_OFFSET,
    );
    let prologue2 = build_coop_task_prologue(
        2,
        sig_addr,
        RING3_COOP_TASK2_ENTRY_OFFSET,
        RING3_COOP_SHARED_BODY_OFFSET,
    );
    let body: [u8; 26] = [
        0x8A, 0xC3, // mov al, bl
        0x04, 0x41, // add al, 0x41
        0x41, 0x88, 0x04, 0x1C, // mov [r12+rbx], al
        0xCD, 0x83, // int 0x83 (yield)
        0x8A, 0xC3, // mov al, bl
        0x04, 0x58, // add al, 0x58
        0x41, 0x88, 0x44, 0x24, 0x03, // mov [r12+3], al
        0xB8, 0x2A, 0x00, 0x00, 0x00, // mov eax, 42
        0xCD, 0x82, // int 0x82 (exit)
    ];

    let mut program = [0u8; RING3_COOP_SHARED_BODY_OFFSET + 26];
    program[RING3_COOP_TASK0_ENTRY_OFFSET..RING3_COOP_TASK0_ENTRY_OFFSET + 16]
        .copy_from_slice(&prologue0);
    program[RING3_COOP_TASK1_ENTRY_OFFSET..RING3_COOP_TASK1_ENTRY_OFFSET + 16]
        .copy_from_slice(&prologue1);
    program[RING3_COOP_TASK2_ENTRY_OFFSET..RING3_COOP_TASK2_ENTRY_OFFSET + 16]
        .copy_from_slice(&prologue2);
    program[RING3_COOP_SHARED_BODY_OFFSET..RING3_COOP_SHARED_BODY_OFFSET + 26]
        .copy_from_slice(&body);
    program
}

pub fn run_ring3_cooperative_test() -> (u64, [u8; 4]) {
    let info = gdt::ring3_info();
    let user_cs = info.user_code_selector as u64;
    let user_ss = info.user_data_selector as u64;

    let code_addr = USER_TEST_PAGE_ADDR;
    const SIG_OFFSET: u64 = 600;
    let sig_addr = code_addr + SIG_OFFSET;

    let task0_entry_offset = RING3_COOP_TASK0_ENTRY_OFFSET;
    let task1_entry_offset = RING3_COOP_TASK1_ENTRY_OFFSET;
    let task2_entry_offset = RING3_COOP_TASK2_ENTRY_OFFSET;

    let program = build_cooperative_test_program(sig_addr);

    unsafe {
        core::ptr::copy_nonoverlapping(program.as_ptr(), code_addr as *mut u8, program.len());
        core::ptr::write_bytes(sig_addr as *mut u8, 0, 4);
    }

    let task0_entry = code_addr + task0_entry_offset as u64;
    let task1_entry = code_addr + task1_entry_offset as u64;
    let task2_entry = code_addr + task2_entry_offset as u64;
    let task0_stack_top = code_addr + 2048;
    let task1_stack_top = code_addr + 3072;
    let task2_stack_top = code_addr + 4096;

    let mut kernel_stack0 = alloc::boxed::Box::new([0u8; 16 * 1024]);
    let mut kernel_stack1 = alloc::boxed::Box::new([0u8; 16 * 1024]);
    let mut kernel_stack2 = alloc::boxed::Box::new([0u8; 16 * 1024]);
    let k0_top = unsafe { kernel_stack0.as_mut_ptr().add(16 * 1024) };
    let k1_top = unsafe { kernel_stack1.as_mut_ptr().add(16 * 1024) };
    let k2_top = unsafe { kernel_stack2.as_mut_ptr().add(16 * 1024) };

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
    let rsp2 = unsafe {
        prepare_ring3_initial_stack(
            k2_top,
            task2_entry,
            user_cs,
            user_ss,
            task2_stack_top,
            RING3_TEST_RFLAGS,
        )
    };

    // Fase 95: passes Priority::Normal for all 3 tasks explicitly,
    // preserving this test's exact existing round-robin behavior byte
    // for byte (real priority reduces to round-robin exactly when every
    // active task shares one tier) - the same update Fase 92 already
    // made to `run_ring3_mt_test`'s own existing callers when priority
    // was added to that OTHER mechanism.
    use crate::scheduler::process::Priority;
    unsafe {
        let tasks = core::ptr::addr_of_mut!(RING3_COOP_TASK_RSP);
        (*tasks)[0] = rsp0;
        (*tasks)[1] = rsp1;
        (*tasks)[2] = rsp2;
        let prios = core::ptr::addr_of_mut!(RING3_COOP_TASK_PRIORITY);
        (*prios)[0] = Priority::Normal as u8;
        (*prios)[1] = Priority::Normal as u8;
        (*prios)[2] = Priority::Normal as u8;
        // Fase 96: reset the done-mask fresh for this test - shared
        // static state a PRIOR test in the same boot could have left
        // set, since nothing else ever clears it besides a fresh setup.
        let done = core::ptr::addr_of_mut!(RING3_COOP_TASK_DONE);
        for i in 0..RING3_COOP_MAX_TASKS {
            (*done)[i] = false;
        }
        *core::ptr::addr_of_mut!(RING3_COOP_ACTIVE_TASKS) = 3;
        *core::ptr::addr_of_mut!(RING3_COOP_CURRENT) = 0;
    }

    kprintln!(
        "[RING3] Attempting 3 ring-3 tasks cooperatively interleaving via a round-robin yield vector (0x83)..."
    );
    serial_println!(
        "[RING3] ring3_cooperative_test task0_entry={:#x} task1_entry={:#x} task2_entry={:#x} sig_addr={:#x}",
        task0_entry,
        task1_entry,
        task2_entry,
        sig_addr
    );

    unsafe {
        crate::scheduler::context_switch::switch_to(
            core::ptr::addr_of_mut!(RING3_TASK_CALLER_RSP),
            rsp0,
        );
    }
    // Resumes HERE once task 0 exits via int 0x82, after its own real
    // round trip through tasks 1 and 2 and back.

    let exit_code = unsafe { core::ptr::addr_of!(RING3_TASK_EXIT_CODE).read() };
    let mut sig = [0u8; 4];
    unsafe {
        sig[0] = core::ptr::read_volatile(sig_addr as *const u8);
        sig[1] = core::ptr::read_volatile((sig_addr + 1) as *const u8);
        sig[2] = core::ptr::read_volatile((sig_addr + 2) as *const u8);
        sig[3] = core::ptr::read_volatile((sig_addr + 3) as *const u8);
    }

    drop(kernel_stack0);
    drop(kernel_stack1);
    drop(kernel_stack2);

    kprintln!(
        "[RING3] Back in ring-0 - cooperative test exit_code={} signature={:02x?} (expected [41, 42, 43, 58] = \"ABC\" + completion marker)",
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

/// Fase 95: proves `ring3_coop_yield_helper`'s own new priority field is
/// real, not cosmetic - the cooperative-yield equivalent of `ring3::
/// run_priority_ring3_mt_test` (Fase 92), but with
/// a genuinely different safety argument since this mechanism is
/// VOLUNTARY rather than timer-driven (see `ring3_coop_yield_helper`'s
/// own doc for the full reasoning). Task 0 gets `Priority::High`; tasks
/// 1 and 2 get `Priority::Background` - genuinely unequal, reusing the
/// EXACT SAME program `build_cooperative_test_program` already builds
/// for the plain round-robin test (Fase 94) - priority is purely a
/// scheduling-setup concern here, not a program-construction one.
///
/// Task 0 writes `'A'` to `sig[0]`, then yields (`int 0x83`) - since it
/// alone holds the top priority tier, the yield helper's own tier
/// search finds no OTHER slot at that tier and round-robins all the way
/// back around to task 0 ITSELF, resuming it immediately. Task 0 then
/// writes its own completion marker (`bl + 0x58 = 0x58`) to `sig[3]` and
/// exits normally via the existing Fase 81 vector. Tasks 1 and 2
/// (`Background`) are never scheduled even once - real, safe programs
/// are still written to memory for them (in case a bug ever let them
/// run, jumping into a genuine, harmless instruction sequence is safer
/// than jumping into whatever memory happened to be there), but proof
/// they never executed is negative: `sig[1]`/`sig[2]` (where they would
/// write `'B'`/`'C'`) stay at the zero-fill this test's own setup wrote
/// before entering ring-3, since nothing else ever touches them.
pub fn run_priority_cooperative_test() -> (u64, [u8; 4]) {
    use crate::scheduler::process::Priority;

    let info = gdt::ring3_info();
    let user_cs = info.user_code_selector as u64;
    let user_ss = info.user_data_selector as u64;

    let code_addr = USER_TEST_PAGE_ADDR;
    const SIG_OFFSET: u64 = 600;
    let sig_addr = code_addr + SIG_OFFSET;

    let task0_entry_offset = RING3_COOP_TASK0_ENTRY_OFFSET;
    let task1_entry_offset = RING3_COOP_TASK1_ENTRY_OFFSET;
    let task2_entry_offset = RING3_COOP_TASK2_ENTRY_OFFSET;

    let program = build_cooperative_test_program(sig_addr);

    unsafe {
        core::ptr::copy_nonoverlapping(program.as_ptr(), code_addr as *mut u8, program.len());
        core::ptr::write_bytes(sig_addr as *mut u8, 0, 4);
    }

    let task0_entry = code_addr + task0_entry_offset as u64;
    let task1_entry = code_addr + task1_entry_offset as u64;
    let task2_entry = code_addr + task2_entry_offset as u64;
    let task0_stack_top = code_addr + 2048;
    let task1_stack_top = code_addr + 3072;
    let task2_stack_top = code_addr + 4096;

    let mut kernel_stack0 = alloc::boxed::Box::new([0u8; 16 * 1024]);
    let mut kernel_stack1 = alloc::boxed::Box::new([0u8; 16 * 1024]);
    let mut kernel_stack2 = alloc::boxed::Box::new([0u8; 16 * 1024]);
    let k0_top = unsafe { kernel_stack0.as_mut_ptr().add(16 * 1024) };
    let k1_top = unsafe { kernel_stack1.as_mut_ptr().add(16 * 1024) };
    let k2_top = unsafe { kernel_stack2.as_mut_ptr().add(16 * 1024) };

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
    let rsp2 = unsafe {
        prepare_ring3_initial_stack(
            k2_top,
            task2_entry,
            user_cs,
            user_ss,
            task2_stack_top,
            RING3_TEST_RFLAGS,
        )
    };

    unsafe {
        let tasks = core::ptr::addr_of_mut!(RING3_COOP_TASK_RSP);
        (*tasks)[0] = rsp0;
        (*tasks)[1] = rsp1;
        (*tasks)[2] = rsp2;
        let prios = core::ptr::addr_of_mut!(RING3_COOP_TASK_PRIORITY);
        (*prios)[0] = Priority::High as u8;
        (*prios)[1] = Priority::Background as u8;
        (*prios)[2] = Priority::Background as u8;
        // Fase 96: reset the done-mask fresh for this test - see the
        // identical reset in run_ring3_cooperative_test's own setup for
        // why (shared static state a prior test could have left set).
        let done = core::ptr::addr_of_mut!(RING3_COOP_TASK_DONE);
        for i in 0..RING3_COOP_MAX_TASKS {
            (*done)[i] = false;
        }
        *core::ptr::addr_of_mut!(RING3_COOP_ACTIVE_TASKS) = 3;
        *core::ptr::addr_of_mut!(RING3_COOP_CURRENT) = 0;
    }

    kprintln!(
        "[RING3] Testing REAL priority in the cooperative-yield mechanism (task0=High vs task1/task2=Background)..."
    );
    serial_println!(
        "[RING3] priority_cooperative_test starting: task0=High task1=Background task2=Background sig_addr={:#x}",
        sig_addr
    );

    unsafe {
        crate::scheduler::context_switch::switch_to(
            core::ptr::addr_of_mut!(RING3_TASK_CALLER_RSP),
            rsp0,
        );
    }
    // Resumes HERE once task 0 exits via int 0x82 - its own single yield
    // resolved back to itself, since it alone holds the top priority
    // tier; tasks 1/2 never got a turn.

    let exit_code = unsafe { core::ptr::addr_of!(RING3_TASK_EXIT_CODE).read() };
    let mut sig = [0u8; 4];
    unsafe {
        sig[0] = core::ptr::read_volatile(sig_addr as *const u8);
        sig[1] = core::ptr::read_volatile((sig_addr + 1) as *const u8);
        sig[2] = core::ptr::read_volatile((sig_addr + 2) as *const u8);
        sig[3] = core::ptr::read_volatile((sig_addr + 3) as *const u8);
    }

    drop(kernel_stack0);
    drop(kernel_stack1);
    drop(kernel_stack2);

    kprintln!(
        "[RING3] Back in ring-0 - priority_cooperative_test exit_code={} signature={:02x?} (expected [41, 00, 00, 58] - task0 ran twice via self-resume, tasks 1/2 never ran)",
        exit_code,
        sig
    );
    serial_println!(
        "[RING3] priority_cooperative_test exit_code={} signature={:02x?}",
        exit_code,
        sig
    );

    (exit_code, sig)
}

/// Fase 96: proves a NON-task-0 cooperative-yield task can voluntarily
/// retire for good instead of yielding-and-being-abandoned - the
/// cooperative-mechanism equivalent of `ring3::run_early_exit_ring3_mt_
/// test` (Fase 93), completing the mirror of all 3 generalizations
/// (N-task, priority, early-exit) onto BOTH ring-3 scheduling mechanisms
/// this arc has built (Fase 91/94 for N-task, Fase 92/95 for priority,
/// Fase 93/96 for early-exit).
///
/// Deliberately isolates this ONE new variable, the same discipline
/// Fase 93 already established for the OTHER mechanism: all 3 tasks
/// share `Priority::Normal` (priority is NOT under test here). Task 0
/// and task 2 keep the EXACT SAME "yield body" `build_cooperative_test_
/// program` already builds (write a char, `int 0x83`, and - only ever
/// reachable for task 0, which is the one the round-robin brings back
/// around to - write a completion marker and exit via `int 0x82`). Task
/// 1 gets a genuinely different, SHORTER "retire body": write `'B'` to
/// `sig[1]`, then `int 0x85` instead of `int 0x83` - there is no "second
/// half" to reach, since retiring means this task never runs again.
///
/// Real, ordered proof: task 0 writes `'A'`, yields to task 1. Task 1
/// writes `'B'`, then RETIRES (rather than yielding) - `coop_select_
/// next` marks it done and picks the next NON-done slot, task 2 (NOT
/// task 1 again - proof the retirement genuinely removed it from the
/// rotation). Task 2 writes `'C'`, yields - wrapping around to task 0
/// (correctly skipping the now-done task 1), which resumes and writes
/// its own completion marker before exiting normally.
///
/// Verification is stronger than the signature alone (which numerically
/// matches Fase 94/95's own round-robin result, `[41, 42, 43, 58]` -
/// task 1 reached `'B'` either way): reading `RING3_COOP_TASK_DONE`
/// directly after the call returns gives independent, structural proof
/// task 1 genuinely retired (not merely yielded and happened to never
/// be resumed again within this short test) - `done[1]=true` while
/// `done[0]=false`/`done[2]=false` (task 0 exits through a completely
/// separate mechanism; task 2 only ever yields, never retires).
pub fn run_early_exit_cooperative_test() -> (u64, [u8; 4], bool, bool, bool) {
    use crate::scheduler::process::Priority;

    let info = gdt::ring3_info();
    let user_cs = info.user_code_selector as u64;
    let user_ss = info.user_data_selector as u64;

    let code_addr = USER_TEST_PAGE_ADDR;
    const SIG_OFFSET: u64 = 600;
    let sig_addr = code_addr + SIG_OFFSET;

    const YIELD_BODY_OFFSET: usize = 48;
    const RETIRE_BODY_OFFSET: usize = 74;
    let task0_entry_offset = RING3_COOP_TASK0_ENTRY_OFFSET;
    let task1_entry_offset = RING3_COOP_TASK1_ENTRY_OFFSET;
    let task2_entry_offset = RING3_COOP_TASK2_ENTRY_OFFSET;

    let prologue0 = build_coop_task_prologue(0, sig_addr, task0_entry_offset, YIELD_BODY_OFFSET);
    let prologue1 = build_coop_task_prologue(1, sig_addr, task1_entry_offset, RETIRE_BODY_OFFSET);
    let prologue2 = build_coop_task_prologue(2, sig_addr, task2_entry_offset, YIELD_BODY_OFFSET);
    let yield_body: [u8; 26] = [
        0x8A, 0xC3, // mov al, bl
        0x04, 0x41, // add al, 0x41
        0x41, 0x88, 0x04, 0x1C, // mov [r12+rbx], al
        0xCD, 0x83, // int 0x83 (yield)
        0x8A, 0xC3, // mov al, bl
        0x04, 0x58, // add al, 0x58
        0x41, 0x88, 0x44, 0x24, 0x03, // mov [r12+3], al
        0xB8, 0x2A, 0x00, 0x00, 0x00, // mov eax, 42
        0xCD, 0x82, // int 0x82 (exit)
    ];
    let retire_body: [u8; 10] = [
        0x8A, 0xC3, // mov al, bl
        0x04, 0x41, // add al, 0x41
        0x41, 0x88, 0x04, 0x1C, // mov [r12+rbx], al
        0xCD, 0x85, // int 0x85 (retire for good)
    ];

    let mut program = [0u8; RETIRE_BODY_OFFSET + 10];
    program[task0_entry_offset..task0_entry_offset + 16].copy_from_slice(&prologue0);
    program[task1_entry_offset..task1_entry_offset + 16].copy_from_slice(&prologue1);
    program[task2_entry_offset..task2_entry_offset + 16].copy_from_slice(&prologue2);
    program[YIELD_BODY_OFFSET..YIELD_BODY_OFFSET + 26].copy_from_slice(&yield_body);
    program[RETIRE_BODY_OFFSET..RETIRE_BODY_OFFSET + 10].copy_from_slice(&retire_body);

    unsafe {
        core::ptr::copy_nonoverlapping(program.as_ptr(), code_addr as *mut u8, program.len());
        core::ptr::write_bytes(sig_addr as *mut u8, 0, 4);
    }

    let task0_entry = code_addr + task0_entry_offset as u64;
    let task1_entry = code_addr + task1_entry_offset as u64;
    let task2_entry = code_addr + task2_entry_offset as u64;
    let task0_stack_top = code_addr + 2048;
    let task1_stack_top = code_addr + 3072;
    let task2_stack_top = code_addr + 4096;

    let mut kernel_stack0 = alloc::boxed::Box::new([0u8; 16 * 1024]);
    let mut kernel_stack1 = alloc::boxed::Box::new([0u8; 16 * 1024]);
    let mut kernel_stack2 = alloc::boxed::Box::new([0u8; 16 * 1024]);
    let k0_top = unsafe { kernel_stack0.as_mut_ptr().add(16 * 1024) };
    let k1_top = unsafe { kernel_stack1.as_mut_ptr().add(16 * 1024) };
    let k2_top = unsafe { kernel_stack2.as_mut_ptr().add(16 * 1024) };

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
    let rsp2 = unsafe {
        prepare_ring3_initial_stack(
            k2_top,
            task2_entry,
            user_cs,
            user_ss,
            task2_stack_top,
            RING3_TEST_RFLAGS,
        )
    };

    unsafe {
        let tasks = core::ptr::addr_of_mut!(RING3_COOP_TASK_RSP);
        (*tasks)[0] = rsp0;
        (*tasks)[1] = rsp1;
        (*tasks)[2] = rsp2;
        let prios = core::ptr::addr_of_mut!(RING3_COOP_TASK_PRIORITY);
        (*prios)[0] = Priority::Normal as u8;
        (*prios)[1] = Priority::Normal as u8;
        (*prios)[2] = Priority::Normal as u8;
        let done = core::ptr::addr_of_mut!(RING3_COOP_TASK_DONE);
        for i in 0..RING3_COOP_MAX_TASKS {
            (*done)[i] = false;
        }
        *core::ptr::addr_of_mut!(RING3_COOP_ACTIVE_TASKS) = 3;
        *core::ptr::addr_of_mut!(RING3_COOP_CURRENT) = 0;
    }

    kprintln!(
        "[RING3] Testing voluntary early exit in the cooperative-yield mechanism (task1 retires via int 0x85 instead of yielding)..."
    );
    serial_println!(
        "[RING3] early_exit_cooperative_test starting: task0=Normal(yields) task1=Normal(retires early) task2=Normal(yields) sig_addr={:#x}",
        sig_addr
    );

    unsafe {
        crate::scheduler::context_switch::switch_to(
            core::ptr::addr_of_mut!(RING3_TASK_CALLER_RSP),
            rsp0,
        );
    }
    // Resumes HERE once task 0 exits via int 0x82, after its own real
    // round trip through task 1 (retired early) and task 2 and back.

    let exit_code = unsafe { core::ptr::addr_of!(RING3_TASK_EXIT_CODE).read() };
    let mut sig = [0u8; 4];
    unsafe {
        sig[0] = core::ptr::read_volatile(sig_addr as *const u8);
        sig[1] = core::ptr::read_volatile((sig_addr + 1) as *const u8);
        sig[2] = core::ptr::read_volatile((sig_addr + 2) as *const u8);
        sig[3] = core::ptr::read_volatile((sig_addr + 3) as *const u8);
    }
    let (task0_done, task1_done, task2_done) = unsafe {
        let done: &[bool; RING3_COOP_MAX_TASKS] = &*core::ptr::addr_of!(RING3_COOP_TASK_DONE);
        (done[0], done[1], done[2])
    };

    drop(kernel_stack0);
    drop(kernel_stack1);
    drop(kernel_stack2);

    kprintln!(
        "[RING3] Back in ring-0 - early_exit_cooperative_test exit_code={} signature={:02x?} task0_done={} task1_done={} task2_done={} (expected signature=[41, 42, 43, 58], task1_done=true, others false)",
        exit_code,
        sig,
        task0_done,
        task1_done,
        task2_done
    );
    serial_println!(
        "[RING3] early_exit_cooperative_test exit_code={} signature={:02x?} task0_done={} task1_done={} task2_done={}",
        exit_code,
        sig,
        task0_done,
        task1_done,
        task2_done
    );

    (exit_code, sig, task0_done, task1_done, task2_done)
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

/// Fase 93: entry point for vector 0x84 - a `scheduler::ring3_mt`-
/// scheduled NON-task-0 task's voluntary "I'm done, stop scheduling me"
/// signal. Byte-for-byte the SAME naked-asm shape as `interrupts::
/// timer_interrupt_entry_asm` (push all 15 GPRs, pass the base pointer to
/// a Rust helper, adopt whatever RSP it returns, pop all 15, `iretq`) -
/// deliberately reused verbatim rather than re-derived, since this
/// vector's whole point is interoperating with `scheduler::ring3_mt`'s
/// own 160-byte full-GPR context format, the exact same format the timer
/// stub already produces and consumes. The one real difference: this
/// fires from a DELIBERATE `int 0x84`, not an asynchronous hardware tick,
/// so there is no `caller_rpl` to isolate - the caller is always ring-3
/// by construction, since only a ring-3 program built to execute this
/// specific instruction can ever reach it at all.
#[unsafe(naked)]
pub(crate) extern "C" fn ring3_mt_task_done_entry_asm() {
    naked_asm!(
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
        "mov rdi, rsp",
        "call {helper}",
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
        helper = sym crate::scheduler::ring3_mt::task_done,
    );
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

/// How one of `build_ring3_mt_checksum_program`'s own tiny programs ends,
/// once past its own checksum computation - 3 genuinely different
/// endings now that Fase 93 adds a second voluntary one:
enum ProgramEnding {
    /// `int 0x81` (Fase 73) - task 0's own original exit vector, ending
    /// the WHOLE `run_multitasking` session. Task 0 only.
    ExitViaInt81,
    /// `int 0x84` (Fase 93) - a NON-task-0 task voluntarily retiring:
    /// stops being scheduled, without affecting anyone else.
    RetireViaInt84,
    /// `jmp $` - spins forever, harmless and critically non-destructive
    /// of `eax` (Fase 86/91's own original "other task" behavior).
    SpinForever,
}

/// Builds one of `run_ring3_mt_test`'s own tiny, self-checking ring-3
/// programs: load 5 "caller-saved" registers with a distinct, checkable
/// pattern, spin `loop_count` times, XOR them into `eax`, then end via
/// whichever `ProgramEnding` the caller chose. Shared here (Fase 91)
/// instead of hand-copied per task now that there are more than 2 of
/// them.
fn build_ring3_mt_checksum_program(
    register_prefix: u32,
    loop_count: u32,
    ending: ProgramEnding,
) -> [u8; 46] {
    let mut program = [0u8; 46];
    program[0] = 0xB8;
    program[1..5].copy_from_slice(&register_prefix.to_le_bytes());
    program[5] = 0xB9;
    program[6..10].copy_from_slice(&(register_prefix + 1).to_le_bytes());
    program[10] = 0xBA;
    program[11..15].copy_from_slice(&(register_prefix + 2).to_le_bytes());
    program[15] = 0xBE;
    program[16..20].copy_from_slice(&(register_prefix + 3).to_le_bytes());
    program[20] = 0xBF;
    program[21..25].copy_from_slice(&(register_prefix + 4).to_le_bytes());
    program[25] = 0x41;
    program[26] = 0xB8;
    program[27..31].copy_from_slice(&loop_count.to_le_bytes());
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
    match ending {
        ProgramEnding::ExitViaInt81 => {
            program[44] = 0xCD;
            program[45] = 0x81; // int 0x81 (Fase 73's own exit vector)
        }
        ProgramEnding::RetireViaInt84 => {
            program[44] = 0xCD;
            program[45] = 0x84; // int 0x84 (Fase 93's own retire vector)
        }
        ProgramEnding::SpinForever => {
            program[44] = 0xEB;
            program[45] = 0xFE; // jmp $ (never exits - see this fn's own doc)
        }
    }
    program
}

/// Fase 86 proved genuine involuntary multi-task ring-3 scheduling
/// works AT ALL (`ring3_preempt`'s own module doc named this the
/// necessary follow-on), deliberately hardcoded to exactly 2 tasks.
/// **Fase 91 generalizes it to 3** - not an arbitrary bigger number, but
/// the smallest scale that actually proves the mechanism isn't secretly
/// still a hardcoded pair, the same "one more than 2, not an arbitrary
/// large N" reasoning this session's own GGUF/quantization Fases
/// already applied one variant at a time. See `scheduler::ring3_mt`'s
/// own module doc for the now-generalized round-robin mechanism; this
/// function builds all 3 ring-3 programs and drives the experiment end
/// to end.
///
/// **Task 0** (entered normally via the existing, unmodified
/// `enter_ring3`/`int 0x81` exit mechanism - EXACTLY Fase 85's own test
/// program, register pattern prefixed `0x1...` so a wrong-task-resumed
/// bug would show up as an immediately wrong, distinguishable value):
/// spins Fase 79/85's own already-proven `LOOP_COUNT=150_000_000`
/// (reliably spanning multiple real tick periods) before exiting.
///
/// **Tasks 1 and 2** (entered ONLY by `scheduler::ring3_mt`'s own
/// round-robin switching, cold, via `prepare_ring3_mt_task_ctx` below -
/// neither ever runs via `enter_ring3` at all): the IDENTICAL shape with
/// `0x2...`/`0x3...` prefixes instead, and MUCH smaller loop counts, so
/// each reliably finishes its own checksum early and spends the rest of
/// the experiment just spinning rather than racing task 0's own much
/// larger loop. Neither ever exits on its own - there is no way for
/// either to signal completion the way task 0 does, since more than one
/// task racing for the single existing `int 0x81`/`RING3_RETURN_RSP`
/// slot would break the "the kernel regains control exactly once, when
/// task 0 itself decides to" invariant every earlier ring3 test already
/// relies on. Instead, verification reads each one's own LAST-saved
/// `eax` directly out of its dedicated context buffer (`scheduler::
/// ring3_mt::run_multitasking`'s own return value) - valid precisely
/// because `jmp $` never touches `eax`, so whatever value is sitting
/// there the LAST time a task happens to be preempted (virtually
/// certain to be well after it finished its own tiny loop, given how
/// much smaller both are than task 0's) is its real, final checksum.
///
/// Deliberately does NOT prove priorities or independent per-task exit -
/// every task here is treated identically by the scheduler (plain
/// round-robin, no priority tiers) and only task 0 can ever signal
/// completion. Both remain real, separate, further follow-on work.
pub fn run_ring3_mt_test() -> (u64, alloc::vec::Vec<u32>, usize) {
    let info = gdt::ring3_info();
    let user_cs = info.user_code_selector as u64;
    let user_ss = info.user_data_selector as u64;

    let code_addr = USER_TEST_PAGE_ADDR;
    let task0_entry = code_addr;
    let task1_entry = code_addr + 128;
    let task2_entry = code_addr + 256;
    let task0_stack_top = code_addr + 2048;
    let task1_stack_top = code_addr + 3072;
    let task2_stack_top = code_addr + 4096;

    const TASK0_EXPECTED: u32 = 0x1000_0004;
    const TASK1_EXPECTED: u32 = 0x2000_0004;
    const TASK2_EXPECTED: u32 = 0x3000_0004;
    const TASK0_LOOP_COUNT: u32 = 150_000_000;
    const TASK1_LOOP_COUNT: u32 = 10_000_000;
    const TASK2_LOOP_COUNT: u32 = 5_000_000;

    let task0 =
        build_ring3_mt_checksum_program(0x1000_0000, TASK0_LOOP_COUNT, ProgramEnding::ExitViaInt81);
    let task1 =
        build_ring3_mt_checksum_program(0x2000_0000, TASK1_LOOP_COUNT, ProgramEnding::SpinForever);
    let task2 =
        build_ring3_mt_checksum_program(0x3000_0000, TASK2_LOOP_COUNT, ProgramEnding::SpinForever);

    unsafe {
        core::ptr::copy_nonoverlapping(task0.as_ptr(), task0_entry as *mut u8, task0.len());
        core::ptr::copy_nonoverlapping(task1.as_ptr(), task1_entry as *mut u8, task1.len());
        core::ptr::copy_nonoverlapping(task2.as_ptr(), task2_entry as *mut u8, task2.len());
    }

    let task1_ctx = prepare_ring3_mt_task_ctx(
        task1_entry,
        user_cs,
        user_ss,
        task1_stack_top,
        RING3_TEST_RFLAGS,
    );
    let task2_ctx = prepare_ring3_mt_task_ctx(
        task2_entry,
        user_cs,
        user_ss,
        task2_stack_top,
        RING3_TEST_RFLAGS,
    );

    kprintln!(
        "[RING3] Attempting genuine multi-task ring-3 scheduling - 3 DIFFERENT programs alternating via involuntary timer ticks..."
    );
    serial_println!(
        "[RING3] ring3_mt_test task0_entry={:#x} task1_entry={:#x} task2_entry={:#x} task0_loop={} task1_loop={} task2_loop={}",
        task0_entry,
        task1_entry,
        task2_entry,
        TASK0_LOOP_COUNT,
        TASK1_LOOP_COUNT,
        TASK2_LOOP_COUNT
    );

    // Fase 92: passes Priority::Normal for all 3 tasks explicitly,
    // preserving this test's exact existing round-robin behavior byte
    // for byte (real priority reduces to round-robin exactly when every
    // active task shares one tier) - mirroring how Fase 87 updated
    // `scheduler::preemptive::start_demo`'s own existing callers the
    // same way when priority was added there.
    use crate::scheduler::process::Priority;
    let (task0_exit_code, other_last_eax, _other_done, switch_count) =
        crate::scheduler::ring3_mt::run_multitasking(
            Priority::Normal,
            &[(Priority::Normal, task1_ctx), (Priority::Normal, task2_ctx)],
            || unsafe {
                enter_ring3(
                    task0_entry,
                    user_cs,
                    user_ss,
                    task0_stack_top,
                    RING3_TEST_RFLAGS,
                )
            },
        );
    let task1_last_eax = other_last_eax[0];
    let task2_last_eax = other_last_eax[1];

    kprintln!(
        "[RING3] Back in ring-0 - mt_test task0_checksum={:#x} (expected {:#x}) task1_last_eax={:#x} (expected {:#x}) task2_last_eax={:#x} (expected {:#x}) switch_count={}",
        task0_exit_code,
        TASK0_EXPECTED,
        task1_last_eax,
        TASK1_EXPECTED,
        task2_last_eax,
        TASK2_EXPECTED,
        switch_count
    );
    serial_println!(
        "[RING3] ring3_mt_test task0_checksum={:#x} task1_last_eax={:#x} task2_last_eax={:#x} switch_count={}",
        task0_exit_code,
        task1_last_eax,
        task2_last_eax,
        switch_count
    );

    (task0_exit_code, other_last_eax, switch_count)
}

/// Fase 92: proves `scheduler::ring3_mt`'s own real priority is
/// genuine, not cosmetic - the ring-3 equivalent of `scheduler::
/// preemptive::run_priority_preemptive_test` (Fase 87). Deliberately
/// does NOT test starvation from task 0's own side: unlike the ring-0
/// preemptive demo, `run_multitasking` has no independent tick budget
/// of its own - it only ever returns once task 0 itself reaches
/// `int 0x81`. If some OTHER task could out-prioritize task 0 forever,
/// task 0 would never run again and this call would never return,
/// hanging the entire boot with no safety net at all (see `scheduler::
/// ring3_mt`'s own module doc for this exact reasoning). So task 0 gets
/// the TOP priority here (`Priority::High`) - guaranteed to keep
/// winning every comparison and complete normally - while tasks 1 and 2
/// (`Priority::Background`) are the ones being tested for starvation.
///
/// Both background tasks get a real, safe, distinguishable program
/// written to memory (`0x5.../0x6...` patterns, distinct from `run_
/// ring3_mt_test`'s own `0x1/2/3...` prefixes so the two tests' log
/// lines are never ambiguous) even though NEITHER is expected to
/// execute even once - a defensive choice, not a formality: if a real
/// bug ever let one of them run, jumping into a genuine, harmless
/// checksum-then-spin program is far safer than jumping into
/// whatever-happened-to-be-there memory. The actual proof is negative:
/// each background task's own dedicated context starts at `ring3::
/// prepare_ring3_mt_task_ctx`'s own zeroed GPR slots (`eax = 0`) and
/// NEVER gets touched if the priority scheduler correctly never once
/// selects it - so `task1_last_eax == 0 && task2_last_eax == 0` is only
/// possible if both were genuinely starved for task 0's entire run, not
/// merely "finished early like in the round-robin test." `switch_count`
/// staying positive confirms real ticks kept landing and being
/// processed by this exact mechanism throughout - proving the zero
/// isn't just "the timer never fired," but "the timer fired repeatedly
/// and correctly kept choosing task 0 every time."
pub fn run_priority_ring3_mt_test() -> (u64, u32, u32, usize) {
    use crate::scheduler::process::Priority;

    let info = gdt::ring3_info();
    let user_cs = info.user_code_selector as u64;
    let user_ss = info.user_data_selector as u64;

    let code_addr = USER_TEST_PAGE_ADDR;
    let task0_entry = code_addr;
    let task1_entry = code_addr + 128;
    let task2_entry = code_addr + 256;
    let task0_stack_top = code_addr + 2048;
    let task1_stack_top = code_addr + 3072;
    let task2_stack_top = code_addr + 4096;

    const TASK0_EXPECTED: u32 = 0x1000_0004;
    const TASK0_LOOP_COUNT: u32 = 150_000_000;
    const TASK1_LOOP_COUNT: u32 = 5_000_000;
    const TASK2_LOOP_COUNT: u32 = 5_000_000;

    let task0 =
        build_ring3_mt_checksum_program(0x1000_0000, TASK0_LOOP_COUNT, ProgramEnding::ExitViaInt81);
    let task1 =
        build_ring3_mt_checksum_program(0x5000_0000, TASK1_LOOP_COUNT, ProgramEnding::SpinForever);
    let task2 =
        build_ring3_mt_checksum_program(0x6000_0000, TASK2_LOOP_COUNT, ProgramEnding::SpinForever);

    unsafe {
        core::ptr::copy_nonoverlapping(task0.as_ptr(), task0_entry as *mut u8, task0.len());
        core::ptr::copy_nonoverlapping(task1.as_ptr(), task1_entry as *mut u8, task1.len());
        core::ptr::copy_nonoverlapping(task2.as_ptr(), task2_entry as *mut u8, task2.len());
    }

    let task1_ctx = prepare_ring3_mt_task_ctx(
        task1_entry,
        user_cs,
        user_ss,
        task1_stack_top,
        RING3_TEST_RFLAGS,
    );
    let task2_ctx = prepare_ring3_mt_task_ctx(
        task2_entry,
        user_cs,
        user_ss,
        task2_stack_top,
        RING3_TEST_RFLAGS,
    );

    kprintln!(
        "[RING3] Testing REAL priority in ring3_mt (task0=High vs task1/task2=Background, genuinely unequal)..."
    );
    serial_println!(
        "[RING3] priority_mt_test starting: task0=High task1=Background task2=Background"
    );

    let (task0_exit_code, other_last_eax, _other_done, switch_count) =
        crate::scheduler::ring3_mt::run_multitasking(
            Priority::High,
            &[
                (Priority::Background, task1_ctx),
                (Priority::Background, task2_ctx),
            ],
            || unsafe {
                enter_ring3(
                    task0_entry,
                    user_cs,
                    user_ss,
                    task0_stack_top,
                    RING3_TEST_RFLAGS,
                )
            },
        );
    let task1_last_eax = other_last_eax[0];
    let task2_last_eax = other_last_eax[1];

    kprintln!(
        "[RING3] Back in ring-0 - priority_mt_test task0_checksum={:#x} (expected {:#x}) task1_last_eax={:#x} task2_last_eax={:#x} switch_count={} - both background tasks getting exactly zero proves task0 (High) genuinely monopolized the CPU.",
        task0_exit_code,
        TASK0_EXPECTED,
        task1_last_eax,
        task2_last_eax,
        switch_count
    );
    serial_println!(
        "[RING3] priority_mt_test task0_checksum={:#x} task1_last_eax={:#x} task2_last_eax={:#x} switch_count={}",
        task0_exit_code,
        task1_last_eax,
        task2_last_eax,
        switch_count
    );

    (
        task0_exit_code,
        task1_last_eax,
        task2_last_eax,
        switch_count,
    )
}

/// Fase 93: proves a NON-task-0 ring3_mt task can voluntarily retire
/// early - stop being scheduled for good, mid-session - rather than
/// spinning forever being the only alternative to task 0's own `int
/// 0x81` exit (a limitation Fase 86 and 91 both explicitly left open).
/// Deliberately isolates this ONE new variable: every task here shares
/// `Priority::Normal` (Fase 91's own round-robin test, unchanged), so
/// nothing about priority selection is under test - only the new
/// retirement path.
///
/// **Task 0** (`0x1...` pattern, unchanged `LOOP_COUNT=150_000_000`,
/// `ExitViaInt81`): identical to every earlier ring3_mt test. **Task 1**
/// (`0x2...` pattern, a small loop) is the ONE thing that's new:
/// `ProgramEnding::RetireViaInt84` instead of spinning forever - once it
/// finishes its own checksum, it voluntarily tells the scheduler it's
/// done and is never resumed again. **Task 2** (`0x3...` pattern, its own
/// distinct small loop) keeps `ProgramEnding::SpinForever` UNCHANGED - a
/// direct control proving the OTHER background task's old behavior is
/// completely unaffected by task 1's new one.
///
/// Verification is stronger than just reading back a checksum: `scheduler
/// ::ring3_mt::run_multitasking`'s own `other_done` return value directly
/// reports whether each task retired, straight out of the same done-mask
/// `task_done` sets - so `task1_done=true` is real, independent proof the
/// new vector fired and was recognized, not merely inferred from task 1's
/// own correct checksum (a bug that left it merely un-resumed after one
/// last correct tick could produce the same checksum without the new
/// mechanism actually working). `task2_done=false` proves the unrelated
/// control task's own path is untouched.
pub fn run_early_exit_ring3_mt_test() -> (u64, u32, bool, u32, bool, usize) {
    use crate::scheduler::process::Priority;

    let info = gdt::ring3_info();
    let user_cs = info.user_code_selector as u64;
    let user_ss = info.user_data_selector as u64;

    let code_addr = USER_TEST_PAGE_ADDR;
    let task0_entry = code_addr;
    let task1_entry = code_addr + 128;
    let task2_entry = code_addr + 256;
    let task0_stack_top = code_addr + 2048;
    let task1_stack_top = code_addr + 3072;
    let task2_stack_top = code_addr + 4096;

    const TASK0_EXPECTED: u32 = 0x1000_0004;
    const TASK1_EXPECTED: u32 = 0x2000_0004;
    const TASK2_EXPECTED: u32 = 0x3000_0004;
    const TASK0_LOOP_COUNT: u32 = 150_000_000;
    const TASK1_LOOP_COUNT: u32 = 5_000_000;
    const TASK2_LOOP_COUNT: u32 = 6_000_000;

    let task0 =
        build_ring3_mt_checksum_program(0x1000_0000, TASK0_LOOP_COUNT, ProgramEnding::ExitViaInt81);
    let task1 = build_ring3_mt_checksum_program(
        0x2000_0000,
        TASK1_LOOP_COUNT,
        ProgramEnding::RetireViaInt84,
    );
    let task2 =
        build_ring3_mt_checksum_program(0x3000_0000, TASK2_LOOP_COUNT, ProgramEnding::SpinForever);

    unsafe {
        core::ptr::copy_nonoverlapping(task0.as_ptr(), task0_entry as *mut u8, task0.len());
        core::ptr::copy_nonoverlapping(task1.as_ptr(), task1_entry as *mut u8, task1.len());
        core::ptr::copy_nonoverlapping(task2.as_ptr(), task2_entry as *mut u8, task2.len());
    }

    let task1_ctx = prepare_ring3_mt_task_ctx(
        task1_entry,
        user_cs,
        user_ss,
        task1_stack_top,
        RING3_TEST_RFLAGS,
    );
    let task2_ctx = prepare_ring3_mt_task_ctx(
        task2_entry,
        user_cs,
        user_ss,
        task2_stack_top,
        RING3_TEST_RFLAGS,
    );

    kprintln!(
        "[RING3] Testing voluntary early exit in ring3_mt (task1 retires via int 0x84 instead of spinning forever)..."
    );
    serial_println!(
        "[RING3] early_exit_mt_test starting: task0=Normal(exits) task1=Normal(retires early) task2=Normal(spins forever)"
    );

    let (task0_exit_code, other_last_eax, other_done, switch_count) =
        crate::scheduler::ring3_mt::run_multitasking(
            Priority::Normal,
            &[(Priority::Normal, task1_ctx), (Priority::Normal, task2_ctx)],
            || unsafe {
                enter_ring3(
                    task0_entry,
                    user_cs,
                    user_ss,
                    task0_stack_top,
                    RING3_TEST_RFLAGS,
                )
            },
        );
    let task1_last_eax = other_last_eax[0];
    let task2_last_eax = other_last_eax[1];
    let task1_done = other_done[0];
    let task2_done = other_done[1];

    kprintln!(
        "[RING3] Back in ring-0 - early_exit_mt_test task0_checksum={:#x} (expected {:#x}) task1_last_eax={:#x} (expected {:#x}) task1_done={} task2_last_eax={:#x} (expected {:#x}) task2_done={} switch_count={}",
        task0_exit_code,
        TASK0_EXPECTED,
        task1_last_eax,
        TASK1_EXPECTED,
        task1_done,
        task2_last_eax,
        TASK2_EXPECTED,
        task2_done,
        switch_count
    );
    serial_println!(
        "[RING3] early_exit_mt_test task0_checksum={:#x} task1_last_eax={:#x} task1_done={} task2_last_eax={:#x} task2_done={} switch_count={}",
        task0_exit_code,
        task1_last_eax,
        task1_done,
        task2_last_eax,
        task2_done,
        switch_count
    );

    (
        task0_exit_code,
        task1_last_eax,
        task1_done,
        task2_last_eax,
        task2_done,
        switch_count,
    )
}

// ---- Fase 98: a ring-3 program whose machine code comes from a real
// FAT12 file, not a kernel-source byte array ----
//
// Every ring-3 "program" this entire arc has ever run (Fase 71 through
// 97) has been a `[u8; N]` constant compiled directly into this kernel's
// own binary, `copy_nonoverlapping`'d straight into `USER_TEST_PAGE_ADDR`.
// That has been the right call every time so far - each Fase needed to
// isolate ONE new scheduling/privilege mechanism, and loading real files
// would have been an unrelated, orthogonal risk to take on at the same
// time. But it also means this kernel has never actually proven it CAN
// run code that came from outside its own source - the real, substantial
// gap standing between everything built so far and an actual "run this
// program from disk" capability a real OS needs.
//
// This test closes that gap in the smallest possible way: writes the
// EXACT SAME 15-byte program `run_ring3_exit_test` already uses (proven
// correct since Fase 73) to a real file via `fat12::create_file`, reads
// it back via `fat12::read_file` - the same write/read/delete round trip
// `main.rs`'s own GGUF disk-load tests already established (Fase 41) -
// and copies THOSE returned bytes, not the original constant, into the
// user page before entering ring-3. If the disk round trip and the
// ring-3 execution both produce exactly the results direct embedding
// already does, that is real, independent proof that machine code
// genuinely survives a trip through this kernel's own filesystem intact
// and executable, not just that reading files and running ring-3 code
// each separately still work (both already proven, separately, many
// Fases ago).
//
// Deliberately does NOT attempt to build any kind of general program
// loader (parsing an executable format, choosing where to place
// multiple segments, ...) - that remains real, substantially larger,
// separate follow-on work. This Fase only proves the one new fact
// everything else would depend on: bytes read back from a real file
// are byte-for-byte what was written, and the CPU treats them as valid
// ring-3 code when copied into an executable page.
//
// Fase 99 update: runs on USER_DISK_PROGRAM_PAGE_ADDR, a SECOND,
// independent page (see memory::user_page's own doc for its
// PML4-distinctness proof) - not USER_TEST_PAGE_ADDR, which every
// OTHER ring-3 test in this file still shares. This is real, if modest,
// proof that entering ring-3 was never implicitly hardwired to the one
// historical shared address: the exact same enter_ring3 mechanism, the
// exact same GDT/TSS setup, now runs a program from a completely
// different address and still produces the identical exit_code=42.
//
// Fase 100 update: stack_top now points at USER_DISK_PROGRAM_STACK_ADDR
// (a THIRD independent page) instead of USER_DISK_PROGRAM_PAGE_ADDR +
// 4096 - code and stack no longer share a page's tail, unlike every
// ring-3 test in this kernel's history before this one. enter_ring3
// needed no changes: user_rsp was already an independent parameter,
// never assumed adjacent to entry.
pub fn run_ring3_disk_loaded_test() -> (bool, u64) {
    const PROGRAM: [u8; 15] = [
        0xB8, 0x01, 0x00, 0x00, 0x00, 0xCD, 0x80, 0xB8, 0x2A, 0x00, 0x00, 0x00, 0xCD, 0x81, 0xFA,
    ];

    let loaded = match shell::find_fat_partition() {
        Ok(partition) => match fat12::read_bpb(&partition) {
            Ok(mut fs) => {
                let write_ok = fs.create_file("RING3.BIN", &PROGRAM).is_ok();
                let read_back = fs.read_file("RING3.BIN").ok();
                let _ = fs.delete_file("RING3.BIN");
                if write_ok {
                    read_back
                } else {
                    None
                }
            }
            Err(_) => None,
        },
        Err(_) => None,
    };

    let roundtrip_ok = loaded.as_deref() == Some(PROGRAM.as_slice());
    if !roundtrip_ok {
        // No real destination address to distrust here - simply never
        // enter ring-3 at all rather than execute whatever stale bytes
        // happen to already be sitting in the shared test page.
        serial_println!(
            "[RING3] ring3_disk_loaded_test roundtrip_ok=false - skipping ring-3 entry"
        );
        return (false, 0);
    }
    let loaded = loaded.unwrap();

    let info = gdt::ring3_info();
    let user_cs = info.user_code_selector as u64;
    let user_ss = info.user_data_selector as u64;
    let code_addr = USER_DISK_PROGRAM_PAGE_ADDR;
    let stack_top = USER_DISK_PROGRAM_STACK_ADDR + 4096;

    unsafe {
        core::ptr::copy_nonoverlapping(loaded.as_ptr(), code_addr as *mut u8, loaded.len());
    }

    kprintln!(
        "[KERNEL INIT] Testing a ring-3 program loaded from a real FAT12 file instead of a kernel-embedded array..."
    );
    let exit_code =
        unsafe { enter_ring3(code_addr, user_cs, user_ss, stack_top, RING3_TEST_RFLAGS) };

    serial_println!(
        "[RING3] ring3_disk_loaded_test roundtrip_ok=true exit_code={}",
        exit_code
    );

    (true, exit_code)
}
