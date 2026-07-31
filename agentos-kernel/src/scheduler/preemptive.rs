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

use super::context_switch::{prepare_initial_stack, switch_to};
use crate::{kprintln, serial_println};
use alloc::boxed::Box;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

const TASK_STACK_SIZE: usize = 16 * 1024;
const DEMO_TICKS: u64 = 50; // ~2.7s at the PIT's default ~18.2Hz

struct PreemptTask {
    saved_rsp: u64,
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
pub fn tick() {
    unsafe {
        if !is_enabled() {
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
    loop {
        PREEMPT_COUNTERS[id].fetch_add(1, Ordering::Relaxed);
    }
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

    unsafe {
        let tasks: *mut [Option<PreemptTask>; 2] = core::ptr::addr_of_mut!(PREEMPT_TASKS);

        let mut stack0 = Box::new([0u8; TASK_STACK_SIZE]);
        let top0 = stack0.as_mut_ptr().add(TASK_STACK_SIZE);
        let rsp0 = prepare_initial_stack(top0, preempt_task_entry_0);
        (*tasks)[0] = Some(PreemptTask {
            saved_rsp: rsp0,
            _stack: stack0,
        });

        let mut stack1 = Box::new([0u8; TASK_STACK_SIZE]);
        let top1 = stack1.as_mut_ptr().add(TASK_STACK_SIZE);
        let rsp1 = prepare_initial_stack(top1, preempt_task_entry_1);
        (*tasks)[1] = Some(PreemptTask {
            saved_rsp: rsp1,
            _stack: stack1,
        });

        *core::ptr::addr_of_mut!(PREEMPT_TICKS_REMAINING) = DEMO_TICKS;
    }
    PREEMPT_ENABLED.store(true, Ordering::Relaxed);

    // Parks here (via hlt, woken by each interrupt) until `tick()` above
    // has forced its way through DEMO_TICKS ticks and switched back.
    while is_enabled() {
        x86_64::instructions::hlt();
    }

    let c0 = PREEMPT_COUNTERS[0].load(Ordering::Relaxed);
    let c1 = PREEMPT_COUNTERS[1].load(Ordering::Relaxed);
    kprintln!(
        "[PREEMPT] task0={} task1={} - both > 0 with neither ever yielding proves real preemption.",
        c0,
        c1
    );
    serial_println!("[PREEMPT] task0={} task1={}", c0, c1);

    // Both tasks are permanently abandoned mid-loop at this point (forced
    // off by the last tick, never to be switched to again) - safe to
    // reclaim their stacks now rather than leak them.
    unsafe {
        let tasks: *mut [Option<PreemptTask>; 2] = core::ptr::addr_of_mut!(PREEMPT_TASKS);
        for slot in (*tasks).iter_mut() {
            *slot = None;
        }
    }
}
