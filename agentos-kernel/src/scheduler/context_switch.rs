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

use crate::{kprintln, serial_println};
use alloc::boxed::Box;
use core::arch::naked_asm;

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
pub(crate) unsafe extern "C" fn switch_to(old_rsp: *mut u64, new_rsp: u64) {
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
pub(crate) unsafe fn prepare_initial_stack(stack_top: *mut u8, entry: extern "C" fn() -> !) -> u64 {
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
        kprintln!(
            "[WORKER] iteration {}/3 (running on its own heap-allocated stack)",
            i
        );
        serial_println!("[WORKER] iteration {}/3 (own stack)", i);
    }

    let mut discard: u64 = 0;
    unsafe {
        switch_to(&mut discard as *mut u64, MAIN_RSP);
    }
    unreachable!("switch_to just moved execution to main's stack for good");
}

// ---- N-way priority-scheduled cooperative tasks ----
//
// Builds on the exact same `switch_to`/`prepare_initial_stack` primitives
// as the single-worker demo above (that demo is left as-is - a minimal,
// independent regression test for the raw switch mechanism). This layer
// adds the part that's actually scheduler-shaped: registering more than
// one task and picking *which* one runs next by priority. Still
// cooperative - a task only ever changes on a voluntary `yield_now()` -
// nothing here is wired to the timer interrupt yet.

use crate::scheduler::agent_scheduler::SCHEDULER;
use crate::scheduler::process::{Priority, ProcessState};

const MAX_COOP_TASKS: usize = 4;
const TASK_STACK_SIZE: usize = 16 * 1024;

struct CoopTask {
    priority: Priority,
    done: bool,
    saved_rsp: u64,
    /// Real PCB pid, registered with `SCHEDULER` at spawn time - this is
    /// what makes this task show up in the `ps` shell command instead of
    /// only existing inside this module's own private task table.
    pid: u32,
    _stack: Box<[u8]>,
}

/// # Safety invariant
/// Touched only from cooperative task/kernel code, one context running at
/// a time on a single core - the same invariant `MAIN_RSP` above relies
/// on. Every access goes through a raw pointer from `addr_of_mut!`/
/// `addr_of!` rather than `&mut COOP_TASKS` etc. directly, so a long-lived
/// reference into the static is never created (keeps this clean under the
/// `static_mut_refs` lint, which `-D warnings` in CI would otherwise
/// reject).
static mut COOP_TASKS: [Option<CoopTask>; MAX_COOP_TASKS] = {
    const EMPTY: Option<CoopTask> = None;
    [EMPTY; MAX_COOP_TASKS]
};
static mut COOP_CURRENT: Option<usize> = None;
static mut COOP_KERNEL_RSP: u64 = 0;

/// Registers a new cooperative task on its own heap-backed stack, and as a
/// real entry in the `ps`-visible `SCHEDULER` PCB table (`Ready`, so it
/// shows up correctly even before its first `yield_now`-driven switch).
/// Call from the kernel context before the first `yield_now()` - spawning
/// from inside an already-running task isn't supported yet. Returns
/// `false` if all `MAX_COOP_TASKS` slots are taken, or if the PCB table
/// itself is full (shares `MAX_PROCESSES` with every other PCB spawn
/// source, so a lot of unrelated spawning elsewhere could exhaust it).
pub fn spawn_cooperative(
    name: &'static str,
    priority: Priority,
    entry: extern "C" fn() -> !,
) -> bool {
    unsafe {
        let tasks: *mut [Option<CoopTask>; MAX_COOP_TASKS] = core::ptr::addr_of_mut!(COOP_TASKS);
        for slot in (*tasks).iter_mut() {
            if slot.is_none() {
                // Token quota 0: these are cooperative-scheduler plumbing
                // tasks, not LLM-agent work, so the token-budget field
                // NativeAgentScheduler otherwise tracks doesn't apply.
                let Some(pid) = SCHEDULER.lock().spawn(name, priority, 0) else {
                    return false;
                };
                let mut stack = Box::new([0u8; TASK_STACK_SIZE]);
                let stack_top = stack.as_mut_ptr().add(TASK_STACK_SIZE);
                let saved_rsp = prepare_initial_stack(stack_top, entry);
                *slot = Some(CoopTask {
                    priority,
                    done: false,
                    saved_rsp,
                    pid,
                    _stack: stack,
                });
                return true;
            }
        }
        false
    }
}

/// Switches away from the current context (kernel, if this is the first
/// call, or whichever task last ran) to the highest-priority not-yet-
/// finished task - ties broken by lowest slot index, the same convention
/// `NativeAgentScheduler::schedule_next` uses. Falls back to the original
/// kernel context once every task has finished.
///
/// A no-op if the highest-priority runnable task is already the caller: a
/// real switch in that case would restore `saved_rsp` as of the *last*
/// time this exact task was switched away from, not "now" - the value
/// about to be overwritten one line into `switch_to` - corrupting its
/// stack. Skipping the switch entirely is both correct and cheaper.
pub fn yield_now() {
    unsafe {
        let tasks: *mut [Option<CoopTask>; MAX_COOP_TASKS] = core::ptr::addr_of_mut!(COOP_TASKS);

        let mut best: Option<usize> = None;
        for idx in 0..MAX_COOP_TASKS {
            if let Some(task) = &(*tasks)[idx] {
                if !task.done {
                    let is_better = match best {
                        None => true,
                        Some(b) => {
                            let current_best = (*tasks)[b].as_ref().unwrap();
                            (task.priority as u8) < (current_best.priority as u8)
                        }
                    };
                    if is_better {
                        best = Some(idx);
                    }
                }
            }
        }

        let current_ptr = core::ptr::addr_of_mut!(COOP_CURRENT);
        let from = *current_ptr;

        if best == from {
            return;
        }

        // Keep the ps-visible PCB table honest. The incoming task's PCB
        // stack_pointer is refreshed from saved_rsp here - guaranteed
        // current, since the *last* switch_to that moved this same task
        // off-CPU is exactly what wrote it. The outgoing task (if any, and
        // if it isn't finishing for good - finish_current_task already
        // marked that case Terminated, `!done` skips re-marking it Ready)
        // goes back to Ready; its own stack_pointer is left as-is here
        // (one yield-cycle stale until it next becomes "best" above,
        // which is harmless - `ps` is diagnostic-only, nothing schedules
        // off this value).
        if let Some(i) = from {
            if let Some(task) = (*tasks)[i].as_ref() {
                if !task.done {
                    SCHEDULER.lock().update_process(
                        task.pid,
                        ProcessState::Ready,
                        task.saved_rsp as usize,
                    );
                }
            }
        }
        if let Some(i) = best {
            let task = (*tasks)[i].as_ref().unwrap();
            SCHEDULER.lock().update_process(
                task.pid,
                ProcessState::Running,
                task.saved_rsp as usize,
            );
        }

        let old_rsp_ptr: *mut u64 = match from {
            Some(i) => {
                let task = (*tasks)[i].as_mut().unwrap();
                &mut task.saved_rsp
            }
            None => core::ptr::addr_of_mut!(COOP_KERNEL_RSP),
        };

        let new_rsp: u64 = match best {
            Some(i) => (*tasks)[i].as_ref().unwrap().saved_rsp,
            None => *core::ptr::addr_of!(COOP_KERNEL_RSP),
        };

        *current_ptr = best;
        switch_to(old_rsp_ptr, new_rsp);
    }
}

/// Marks the calling task finished (so it's never selected again) and
/// yields away - never returns, since nothing will ever resume this
/// task's stack again.
pub fn finish_current_task() -> ! {
    unsafe {
        let tasks: *mut [Option<CoopTask>; MAX_COOP_TASKS] = core::ptr::addr_of_mut!(COOP_TASKS);
        if let Some(i) = *core::ptr::addr_of!(COOP_CURRENT) {
            let task = (*tasks)[i].as_mut().unwrap();
            task.done = true;
            SCHEDULER.lock().update_process(
                task.pid,
                ProcessState::Terminated,
                task.saved_rsp as usize,
            );
        }
    }
    yield_now();
    unreachable!("yield_now switched away from a finished task for good");
}

/// Proof that this is a real priority scheduler, not just round-robin:
/// three tasks (High/Normal/Background) each print twice, yielding
/// between prints. Because the highest-priority not-done task always wins
/// the scan in `yield_now` (including against itself, via the no-op
/// self-switch case above), each task runs to completion before the next
/// lower-priority one gets a single instruction - the expected serial
/// order is fully deterministic: alpha x2, then bravo x2, then charlie x2.
pub fn run_cooperative_demo() {
    kprintln!(
        "[COOP] Spawning 3 cooperative tasks: alpha(High), bravo(Normal), charlie(Background)..."
    );
    serial_println!("[COOP] spawn alpha(High) bravo(Normal) charlie(Background)");

    spawn_cooperative("task-alpha", Priority::High, task_alpha_entry);
    spawn_cooperative("task-bravo", Priority::Normal, task_bravo_entry);
    spawn_cooperative("task-charlie", Priority::Background, task_charlie_entry);

    yield_now();

    // Resumes here only once every task has called finish_current_task()
    // and the last one fell back to the kernel context.
    kprintln!("[COOP] All cooperative tasks finished - back in kernel context.");
    serial_println!("[COOP] all tasks finished, back in kernel");

    // Every finished task's stack (3 x 16 KiB) would otherwise stay
    // allocated forever - harmless in isolation, but no reason to hold
    // onto it either. Safe to free now: nothing will ever switch to a
    // `done` task's saved_rsp again.
    unsafe {
        let tasks: *mut [Option<CoopTask>; MAX_COOP_TASKS] = core::ptr::addr_of_mut!(COOP_TASKS);
        for slot in (*tasks).iter_mut() {
            *slot = None;
        }
    }
}

extern "C" fn task_alpha_entry() -> ! {
    coop_task_body("alpha")
}

extern "C" fn task_bravo_entry() -> ! {
    coop_task_body("bravo")
}

extern "C" fn task_charlie_entry() -> ! {
    coop_task_body("charlie")
}

fn coop_task_body(name: &str) -> ! {
    for i in 1..=2 {
        kprintln!("[TASK {}] iteration {}/2", name, i);
        serial_println!("[TASK {}] iteration {}/2", name, i);
        if name == "alpha" && i == 1 {
            // Proof this isn't just a same-boot-time coincidence: `ps`
            // dispatched from *inside* a running cooperative task, reading
            // the exact same SCHEDULER `spawn_cooperative` just registered
            // into. At this precise point alpha must show RUNNING (it's
            // the one calling this) and bravo/charlie READY (spawned, not
            // yet switched to) - real, live scheduler state, not a fixed
            // boot-time table `ps` used to only ever show.
            crate::shell::dispatch_command("ps");
        }
        yield_now();
    }
    finish_current_task();
}
