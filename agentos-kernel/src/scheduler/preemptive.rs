//! Real timer-driven preemption: two tasks that never call `yield_now`
//! themselves - the only reason either one ever stops running is the
//! timer interrupt forcing it. Built on the exact same `switch_to`/
//! `prepare_initial_stack` primitives as `context_switch.rs`'s cooperative
//! scheduler; the only new part is *who* decides to switch (the timer
//! handler, not the task itself).
//!
//! ## Why this is safe without any extra locking
//! A switch here only ever happens from inside `timer_interrupt_handler`,
//! and IDT interrupt-gate entries (the default `set_handler_fn` uses)
//! automatically clear the CPU's interrupt flag on entry and restore it
//! from the saved RFLAGS on `iretq`. So every switch performed here runs
//! with interrupts already disabled by the CPU itself, and a task resumed
//! via a *previous* switch naturally continues with interrupts re-enabled
//! once its original interrupt frame is eventually `iretq`'d back to -
//! there is nothing extra to disable/enable by hand, unlike a switch
//! initiated from ordinary (non-interrupt) code.
//!
//! One real gotcha this relies on getting right: a task's *first* entry
//! happens via `switch_to`'s plain `ret`, not an `iretq` - so there's no
//! saved RFLAGS to restore its interrupt-enabled state from. Left alone,
//! a freshly spawned task would run with interrupts permanently off (and
//! since the timer can't fire without them, it could never be preempted
//! again - a silent, total hang). Each task's entry point calls
//! `interrupts::enable()` itself as its very first action to fix this.
//!
//! ## `ps`-visible PCB sync: `try_lock`, never `lock`, inside `tick()`
//! Unlike everything above, this module *does* now touch one piece of
//! extra locking from interrupt context: `tick()` optionally updates each
//! task's entry in the `ps`-visible `SCHEDULER` table (a `spin::Mutex`),
//! the same unification `context_switch.rs`'s cooperative scheduler
//! already does from normal code. Taking that lock the ordinary
//! (blocking) way from *interrupt* context would be a real bug on this
//! single core: if the timer fires while some normal-context caller
//! (`ps`, `spawn_cooperative`, ...) is mid-update with the lock held, that
//! caller cannot possibly release it until this very interrupt handler
//! returns - and this handler would be spinning forever waiting for a
//! lock it made unreleasable, a guaranteed permanent deadlock rather than
//! just a slowdown. `tick()` uses `try_lock()` and simply skips the PCB
//! update for that one tick if contended; the actual task switch below
//! never depends on it, and the next tick (roughly 55ms later) retries.
//! `ps` showing one tick's worth of stale state in the vanishingly rare
//! contended case is a fully acceptable, self-correcting trade-off for a
//! diagnostic-only view - blocking here would not be.

use super::context_switch::{prepare_initial_stack, switch_to};
use crate::scheduler::agent_scheduler::SCHEDULER;
use crate::scheduler::process::{Priority, ProcessState};
use crate::{kprintln, serial_println};
use alloc::boxed::Box;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

const TASK_STACK_SIZE: usize = 16 * 1024;
const DEMO_TICKS: u64 = 50; // ~2.7s at the PIT's default ~18.2Hz

struct PreemptTask {
    saved_rsp: u64,
    /// Real PCB pid (see module doc's new "ps unification" section) -
    /// registered from `run_preemptive_demo` (normal context, safe to
    /// block on `SCHEDULER.lock()` there); `tick()` itself only ever uses
    /// `try_lock()` on it, never the blocking form.
    pid: u32,
    _stack: Box<[u8]>,
}

/// # Safety invariant
/// Same as `context_switch.rs`'s `COOP_TASKS`: only ever touched from
/// `timer_interrupt_handler` (see module doc) or, for `PREEMPT_TASKS`/
/// `PREEMPT_TICKS_REMAINING`, briefly from `run_preemptive_demo` *before*
/// enabling preemption or *after* it has confirmed (by observing
/// `PREEMPT_ENABLED == false`) preemption has stopped - never
/// concurrently with the timer handler touching them. `addr_of_mut!`/
/// `addr_of!` throughout, never `&mut STATIC` directly, to stay clean
/// under `static_mut_refs`.
static mut PREEMPT_TASKS: [Option<PreemptTask>; 2] = [None, None];
static mut PREEMPT_CURRENT: Option<usize> = None; // None = kernel/idle context
static mut PREEMPT_IDLE_RSP: u64 = 0;
static mut PREEMPT_TICKS_REMAINING: u64 = 0;

/// Unlike the fields above, this one genuinely is polled in a tight loop
/// by `run_preemptive_demo` (normal code) while `tick()` (interrupt
/// context) writes it asynchronously from that loop's perspective. A
/// plain `static mut bool` here is a real bug, not just a style issue: in
/// a release+LTO build the compiler is free to treat repeated reads of a
/// never-(as-far-as-it-can-tell)-written local as loop-invariant and hoist
/// it out entirely - which is exactly what happened the first time this
/// was a `static mut` (`while is_enabled() { hlt() }` never noticed
/// `tick()` had cleared it, and hung forever). `AtomicBool` is immune to
/// that: atomicity itself, not the ordering, is what forces a real load
/// every time. `Ordering::Relaxed` is enough - single core, so there's no
/// cross-CPU visibility concern, only "don't let the compiler cache this".
static PREEMPT_ENABLED: AtomicBool = AtomicBool::new(false);

fn is_enabled() -> bool {
    PREEMPT_ENABLED.load(Ordering::Relaxed)
}

/// Called from `timer_interrupt_handler` on every real timer tick, after
/// EOI. A no-op unless `run_preemptive_demo` has enabled preemption.
///
/// **Fase 80: `interrupted_ring3` must be checked BEFORE anything else
/// below.** Every switch this function performs relies on `switch_to`'s
/// own trick of unwinding back up through the interrupted call stack to
/// `timer_interrupt_handler`'s own compiler-generated epilogue, which
/// then `iretq`s using THAT SPECIFIC invocation's own captured interrupt
/// frame - correct only because every ring-0 task here is always
/// resumed from the exact same nested position. A tick that instead
/// interrupted RING-3 code has an entirely different, arbitrary register
/// state and privilege level at an arbitrary point - `switch_to` cannot
/// safely swap that stack out for a different ring-0 task's, since
/// nothing would ever correctly resume it afterward (its own `iretq`
/// frame would never get unwound to). This was a real, previously
/// untested gap: ring-3 execution and ring-0 preemption had never
/// actually been exercised concurrently before Fase 80's own test -
/// exposed only by reasoning through what Fase 79's new ring-3-
/// detection capability made newly possible to check. The fix is to
/// treat a tick that interrupts ring-3 as entirely invisible to this
/// scheduler: it doesn't decrement the remaining tick budget, doesn't
/// alternate tasks, doesn't touch the `ps`-visible PCB table - as if,
/// from THIS scheduler's own perspective, no time passed at all while
/// ring-3 code was running. `TIMER_TICKS` (the raw, unconditional
/// counter) and the ring-3 program's own execution are both entirely
/// unaffected either way; only the ring-0-only alternation logic below
/// is what needs to stay out of a ring-3-interrupted tick's way.
pub fn tick(interrupted_ring3: bool) {
    unsafe {
        if !is_enabled() || interrupted_ring3 {
            return;
        }

        let remaining_ptr = core::ptr::addr_of_mut!(PREEMPT_TICKS_REMAINING);
        *remaining_ptr = remaining_ptr.read().saturating_sub(1);

        let current_ptr = core::ptr::addr_of_mut!(PREEMPT_CURRENT);
        let from = *current_ptr;

        let next: Option<usize> = if *remaining_ptr == 0 {
            // Demo window elapsed - force one last switch back to the
            // kernel/idle context and stop ticking after this.
            PREEMPT_ENABLED.store(false, Ordering::Relaxed);
            None
        } else {
            match from {
                None => Some(0),
                Some(0) => Some(1),
                Some(1) => Some(0),
                Some(_) => unreachable!("only 2 preemptive task slots exist"),
            }
        };

        if next == from {
            // Only reachable if DEMO_TICKS were 0 (nothing ever started).
            return;
        }

        let tasks: *mut [Option<PreemptTask>; 2] = core::ptr::addr_of_mut!(PREEMPT_TASKS);

        // Best-effort ps-visible PCB sync - see the module doc's `try_lock`
        // section for why this must never be the blocking `.lock()`.
        if let Some(mut sched) = SCHEDULER.try_lock() {
            if let Some(i) = from {
                if let Some(t) = (*tasks)[i].as_ref() {
                    sched.update_process(t.pid, ProcessState::Ready, t.saved_rsp as usize);
                }
            }
            if let Some(i) = next {
                if let Some(t) = (*tasks)[i].as_ref() {
                    sched.update_process(t.pid, ProcessState::Running, t.saved_rsp as usize);
                }
            }
        }

        let old_rsp_ptr: *mut u64 = match from {
            Some(i) => {
                let t = (*tasks)[i].as_mut().unwrap();
                &mut t.saved_rsp
            }
            None => core::ptr::addr_of_mut!(PREEMPT_IDLE_RSP),
        };
        let new_rsp: u64 = match next {
            Some(i) => (*tasks)[i].as_ref().unwrap().saved_rsp,
            None => *core::ptr::addr_of!(PREEMPT_IDLE_RSP),
        };

        *current_ptr = next;
        switch_to(old_rsp_ptr, new_rsp);
    }
}

// Same reasoning as `PREEMPT_ENABLED` above, on the write side this time:
// a plain `static mut` counter, incremented in a loop that (as far as
// LLVM's optimizer can tell from this function's own control flow alone)
// never returns and is never observed, has no proven side effect - which
// makes the whole loop, or at least these writes, eligible to be
// optimized away or never actually committed to memory. It's only "read"
// by a completely different function, reached only via the interrupt
// hijacking this loop mid-flight via inline asm - invisible to that
// analysis. Atomics make every increment a real, unremovable operation.
static PREEMPT_COUNTERS: [AtomicU64; 2] = [AtomicU64::new(0), AtomicU64::new(0)];

extern "C" fn preempt_task_entry_0() -> ! {
    preempt_task_body(0)
}

extern "C" fn preempt_task_entry_1() -> ! {
    preempt_task_body(1)
}

fn preempt_task_body(id: usize) -> ! {
    // See the module doc: mandatory for a freshly-entered task, or it
    // (and the whole system, since only it can re-arm the timer) would
    // run with interrupts off forever.
    x86_64::instructions::interrupts::enable();
    if id == 0 {
        // Proof this task's first-ever entry already has a real PCB
        // record behind it: by the time execution reaches here, the
        // `tick()` call that switched into this task has already run its
        // try_lock'd PCB update - task-preempt-0 should show RUNNING here
        // (with a real stack pointer), task-preempt-1 still READY (spawned
        // but not yet switched to).
        crate::shell::dispatch_command("ps");
    }
    loop {
        PREEMPT_COUNTERS[id].fetch_add(1, Ordering::Relaxed);
    }
}

/// Starts the 2 non-yielding tasks and enables preemption for `ticks`
/// real timer ticks, WITHOUT blocking until it finishes - unlike
/// `run_preemptive_demo`, which calls this and then immediately waits.
/// Split out (Fase 80) so a caller can start this running in the
/// background, then do something else (e.g. enter ring-3) while it's
/// active, driven entirely by the timer interrupt from here on -
/// exactly what `ring3::run_ring3_concurrent_preemption_test` needs to
/// prove ring-0 preemption and a running ring-3 program coexist safely.
///
/// Real PCB entries, same as `spawn_cooperative` - normal (non-interrupt)
/// context, so the ordinary blocking `.lock()` is fine here even though
/// `tick()` itself must only ever use `try_lock()`. Neither task has a
/// real priority relationship (the scheduler just alternates strictly
/// 0/1/0/1, ignoring priority entirely), so Normal for both is the
/// honest choice, not a placeholder.
fn start_demo(ticks: u64) {
    let pid0 = SCHEDULER
        .lock()
        .spawn("preempt-task-0", Priority::Normal, 0)
        .expect("PCB table full");
    let pid1 = SCHEDULER
        .lock()
        .spawn("preempt-task-1", Priority::Normal, 0)
        .expect("PCB table full");

    unsafe {
        let tasks: *mut [Option<PreemptTask>; 2] = core::ptr::addr_of_mut!(PREEMPT_TASKS);

        let mut stack0 = Box::new([0u8; TASK_STACK_SIZE]);
        let top0 = stack0.as_mut_ptr().add(TASK_STACK_SIZE);
        let rsp0 = prepare_initial_stack(top0, preempt_task_entry_0);
        (*tasks)[0] = Some(PreemptTask {
            saved_rsp: rsp0,
            pid: pid0,
            _stack: stack0,
        });

        let mut stack1 = Box::new([0u8; TASK_STACK_SIZE]);
        let top1 = stack1.as_mut_ptr().add(TASK_STACK_SIZE);
        let rsp1 = prepare_initial_stack(top1, preempt_task_entry_1);
        (*tasks)[1] = Some(PreemptTask {
            saved_rsp: rsp1,
            pid: pid1,
            _stack: stack1,
        });

        *core::ptr::addr_of_mut!(PREEMPT_TICKS_REMAINING) = ticks;
    }
    PREEMPT_ENABLED.store(true, Ordering::Relaxed);
}

/// Real remaining tick budget for whichever demo `start_demo` began (0
/// once it's finished or if none is running). Used internally by
/// `run_concurrent_ring3_preemption_test` (Fase 80) to directly observe
/// whether entering ring-3 in between "paused" the countdown - see
/// `tick()`'s own doc for why a tick that interrupts ring-3 code must
/// NOT decrement this.
fn ticks_remaining() -> u64 {
    unsafe { *core::ptr::addr_of!(PREEMPT_TICKS_REMAINING) }
}

/// Each task's own real, monotonic tick counter (Fase 80). Used
/// internally to compare a BEFORE/AFTER snapshot around a test window
/// and check the DELTA increased, since these never reset between
/// separate calls to `start_demo` within the same boot (e.g.
/// `run_preemptive_demo`'s own run already leaves both above 0 by the
/// time a later caller's own window begins).
fn task_counters() -> (u64, u64) {
    (
        PREEMPT_COUNTERS[0].load(Ordering::Relaxed),
        PREEMPT_COUNTERS[1].load(Ordering::Relaxed),
    )
}

/// Blocks (via `hlt`, woken by each interrupt) until whatever
/// `start_demo` began has run its course - `tick()` forced its way
/// through the whole tick budget and switched back to this context -
/// then cleans up both tasks' PCB entries and returns their final,
/// absolute counters. The second half of `run_preemptive_demo`'s own
/// logic, split out (Fase 80) so a caller can do other work (enter
/// ring-3) in between calling `start_demo` and this.
///
/// Both tasks are permanently abandoned mid-loop by the time this
/// returns (forced off by the demo's own last tick, never to be
/// switched to again) - safe to reclaim their stacks here rather than
/// leak them. Unlike the cooperative demo's tasks, these never call
/// anything like `finish_current_task` (they're infinite loops with
/// zero self-awareness of the demo ending) - so marking their PCB
/// entries Terminated has to happen here, from normal context, or `ps`
/// would keep showing two long-abandoned tasks as perpetually
/// READY/RUNNING.
fn wait_for_demo_and_cleanup() -> (u64, u64) {
    while is_enabled() {
        x86_64::instructions::hlt();
    }

    let (c0, c1) = task_counters();

    unsafe {
        let tasks: *mut [Option<PreemptTask>; 2] = core::ptr::addr_of_mut!(PREEMPT_TASKS);
        for slot in (*tasks).iter_mut() {
            if let Some(task) = slot.take() {
                SCHEDULER.lock().update_process(
                    task.pid,
                    ProcessState::Terminated,
                    task.saved_rsp as usize,
                );
            }
        }
    }

    (c0, c1)
}

/// Proof of *real* preemption: two tasks that never call `yield_now` at
/// all - an infinite loop apiece. Under pure cooperative scheduling
/// neither would ever run past the first one picked. Here, the timer
/// forces them to alternate for `DEMO_TICKS` real ticks with zero
/// cooperation from either task, then hands control back to the kernel.
/// Both counters ending up nonzero is the whole proof.
pub fn run_preemptive_demo() {
    kprintln!("[PREEMPT] Testing real timer-driven preemption (2 tasks, neither ever yields)...");
    serial_println!("[PREEMPT] starting: timer will force-switch 2 non-yielding tasks");

    start_demo(DEMO_TICKS);
    let (c0, c1) = wait_for_demo_and_cleanup();

    kprintln!(
        "[PREEMPT] task0={} task1={} - both > 0 with neither ever yielding proves real preemption.",
        c0,
        c1
    );
    serial_println!("[PREEMPT] task0={} task1={}", c0, c1);
}

/// Runs a preemptive ring-0 demo AND a real ring-3 program CONCURRENTLY
/// for the first time (Fase 80) - proving they coexist safely, not just
/// that each works in isolation (every prior test of either ran
/// sequentially, never overlapping). Starts a shorter demo (fewer ticks
/// than the standalone one above - this test's own real-time footprint
/// already includes the ring-3 program's own spin window on top of it),
/// captures the tick budget and both tasks' own counters, enters a real
/// ring-3 program that spins long enough for real timer ticks to land
/// mid-execution (the exact same hand-encoded loop shape
/// `ring3::run_ring3_timer_tick_test` already established, Fase 79),
/// then checks the tick budget again before finally waiting for the
/// demo to finish naturally and checking both counters advanced.
///
/// Returns `(ring3_exit_code, budget_untouched_during_ring3,
/// tasks_advanced_after)` - `budget_untouched_during_ring3` is the
/// direct proof `tick()`'s own ring-3 guard (this Fase's actual fix)
/// works: without it, ticks landing during the ring-3 window would
/// decrement the budget same as any other tick, and `switch_to` would
/// have been invoked against an interrupted ring-3 stack frame it
/// cannot safely swap out - `tasks_advanced_after` confirms ring-0
/// preemption still genuinely works once ring-3 exits, not just that
/// nothing crashed.
pub fn run_concurrent_ring3_preemption_test(
    enter_ring3_spin_loop: impl FnOnce() -> u64,
) -> (u64, bool, bool) {
    const CONCURRENT_DEMO_TICKS: u64 = 30;

    let (c0_before, c1_before) = task_counters();
    start_demo(CONCURRENT_DEMO_TICKS);

    let ticks_before_ring3 = ticks_remaining();
    let exit_code = enter_ring3_spin_loop();
    let ticks_after_ring3 = ticks_remaining();
    let budget_untouched_during_ring3 = ticks_before_ring3 == ticks_after_ring3;

    let (c0_after, c1_after) = wait_for_demo_and_cleanup();
    let tasks_advanced_after = c0_after > c0_before && c1_after > c1_before;

    (
        exit_code,
        budget_untouched_during_ring3,
        tasks_advanced_after,
    )
}
