//! A minimal, textbook (xv6-style) cooperative context switch: swap the
//! stack pointer and the 6 callee-saved registers between two execution
//! contexts. This is *cooperative*, not preemptive - a task must call
//! `switch_to` itself to yield. Nothing here hooks the timer interrupt;
//! that (real preemption) is separate, future work - see README.md.
//!
//! Only the 6 callee-saved registers (rbp, rbx, r12-r15) need saving here.
//! Everything else is caller-saved, meaning the Rust compiler already
//! preserves it around the call site that invokes `switch_to`, exactly as
//! it would for any other function call - that's what makes this minimal
//! save/restore set correct and not just "seems to work".

use alloc::boxed::Box;
use core::arch::naked_asm;
use crate::{kprintln, serial_println};

const WORKER_STACK_SIZE: usize = 16 * 1024;

/// Holds the "main" context's saved stack pointer between the two switches
/// in `run_demo`. A single shared slot is enough for this one-worker proof
/// of concept; a real N-task scheduler would keep one such slot per task.
static mut MAIN_RSP: u64 = 0;

/// Saves the caller's callee-saved registers and RSP into `*old_rsp`, then
/// loads RSP from `new_rsp` and restores callee-saved registers from
/// *that* stack before returning - "returning" into whatever the new stack
/// was left at, which may be a previous caller of `switch_to`, or a stack
/// freshly laid out by `prepare_initial_stack`.
///
/// # Safety
/// `old_rsp` must be a valid, writable `*mut u64`. `new_rsp` must be either
/// a value previously written by this same function into some old_rsp
/// slot, or the return value of `prepare_initial_stack` - any other value
/// is an arbitrary jump and stack-corrupting undefined behavior.
#[unsafe(naked)]
unsafe extern "C" fn switch_to(old_rsp: *mut u64, new_rsp: u64) {
    naked_asm!(
        "push rbp",
        "push rbx",
        "push r12",
        "push r13",
        "push r14",
        "push r15",
        "mov [rdi], rsp",
        "mov rsp, rsi",
        "pop r15",
        "pop r14",
        "pop r13",
        "pop r12",
        "pop rbx",
        "pop rbp",
        "ret",
    );
}

/// Lays out a fresh stack so the *first* `switch_to` into it "returns" into
/// `entry` as if `entry` had been called normally from `switch_to`'s `ret`.
///
/// Memory layout built downward from `stack_top` (7 quadwords, matching
/// `switch_to`'s pop order): entry address on top, then six zeroed
/// placeholders for rbp/rbx/r12/r13/r14/r15 - their real values don't
/// matter yet since `entry` hasn't run any code that depends on them.
///
/// The `- 8` alignment adjustment matters: after `switch_to`'s final `ret`
/// lands at `entry`, RSP must be ≡ 8 (mod 16) - the same invariant the
/// System V ABI guarantees right after any normal `call` instruction
/// (which pushes one 8-byte return address onto a 16-aligned stack).
/// `entry`'s compiler-generated prologue assumes that invariant; getting
/// this off by 8 produces a stack that *looks* fine until something does
/// an aligned SSE access and takes a #GP fault.
unsafe fn prepare_initial_stack(stack_top: *mut u8, entry: extern "C" fn() -> !) -> u64 {
    let top = ((stack_top as u64) & !0xf) - 8;
    let mut sp = top;

    sp -= 8;
    *(sp as *mut u64) = entry as usize as u64;

    for _ in 0..6 {
        sp -= 8;
        *(sp as *mut u64) = 0;
    }

    sp
}

/// Proof-of-concept: switches from the calling stack onto a fresh
/// heap-allocated one, lets `worker_entry` run there and print a few
/// lines, then the worker switches back and this call returns normally -
/// demonstrating a genuine stack/register swap across an arbitrary call
/// boundary, not a simulation of one.
pub fn run_demo() {
    kprintln!("[KERNEL INIT] Testing cooperative context switch (real stack/register swap)...");

    let mut stack = Box::new([0u8; WORKER_STACK_SIZE]);
    // as_mut_ptr(), not as_ptr(): the stack gets written through this
    // pointer (via prepare_initial_stack and then the worker itself), and
    // a pointer derived from a shared (`&`) reference doesn't have valid
    // provenance for that under Rust's aliasing rules, even after an `as
    // *mut` cast - only cosmetically different from as_ptr() today, but
    // as_mut_ptr() is the version that's actually sound to write through.
    let stack_top = unsafe { stack.as_mut_ptr().add(WORKER_STACK_SIZE) };
    let worker_rsp = unsafe { prepare_initial_stack(stack_top, worker_entry) };

    kprintln!("[CTXSWITCH] main -> worker");
    serial_println!("[CTXSWITCH] main -> worker");

    unsafe {
        switch_to(core::ptr::addr_of_mut!(MAIN_RSP), worker_rsp);
    }

    // Execution resumes exactly here once worker_entry's switch_to call
    // hands control back - proof that this line, and everything on this
    // (unrelated, since-untouched) stack, survived the round trip intact.
    kprintln!("[CTXSWITCH] worker -> main (registers/stack correctly restored)");
    serial_println!("[CTXSWITCH] worker -> main: demo OK");

    drop(stack); // must outlive the worker's last use of it, which just happened
}

extern "C" fn worker_entry() -> ! {
    for i in 1..=3 {
        kprintln!("[WORKER] iteration {}/3 (running on its own heap-allocated stack)", i);
        serial_println!("[WORKER] iteration {}/3 (own stack)", i);
    }

    let mut discard: u64 = 0;
    unsafe {
        switch_to(&mut discard as *mut u64, MAIN_RSP);
    }
    unreachable!("switch_to just moved execution to main's stack for good");
}
