//! Fase 86: genuine multi-task ring-3 scheduling via the timer - actually
//! running a DIFFERENT ring-3 program in the gap a preempted one leaves
//! behind, not just resuming the SAME one the way `scheduler::ring3_
//! preempt` (Fase 85) deliberately, minimally proved first. Mirrors
//! `scheduler::preemptive`'s own ring-0 round-robin shape almost
//! exactly (tasks alternating on every tick), but switching between
//! FULL 160-byte (15-GPR + 5-field `iretq` frame) ring-3 contexts
//! instead of ring-0's much smaller 6-callee-saved-register `switch_to`
//! convention, for the exact same reason Fase 85 itself needed the
//! bigger shape: a real, involuntary timer tick can land at any
//! arbitrary point in a ring-3 program's own execution, so nothing less
//! than the FULL register set Fase 84's naked stub already captures is
//! enough to resume it correctly later.
//!
//! **Fase 91 generalizes this from exactly 2 hardcoded tasks to a real
//! N-task round-robin** (still bounded by a fixed `RING3_MT_MAX_TASKS`
//! headroom, not a dynamically unbounded count - a genuine limit, not a
//! shortcut: `tick()` itself runs in interrupt context, where growing a
//! heap-backed collection mid-flight would be a real hazard this
//! codebase's own established discipline avoids). `tick()`'s own
//! round-robin arithmetic was ALREADY written generically
//! (`(current + 1) % active_count`) back in Fase 86 - the only things
//! that were genuinely hardcoded to 2 were the array's own compile-time
//! size and `run_multitasking`'s signature (exactly one "other" task
//! context, exactly one "other" task's reported checksum). Both are
//! generalized here.
//!
//! **Fase 92 adds real priority, mirroring `scheduler::preemptive::
//! tick`'s own top-priority-tier-then-round-robin algorithm exactly**
//! (Fase 87's own ring-0 equivalent), generalized from a hardcoded pair
//! to `active_tasks` slots the same way the round-robin arithmetic
//! itself was generalized in Fase 91. One real difference from the
//! ring-0 case shapes this module's OWN self-test design, not the
//! mechanism here: `scheduler::preemptive`'s own demo has an
//! INDEPENDENT, fixed tick budget (`DEMO_TICKS`) controlling when it
//! returns, regardless of what its own tasks do - `run_multitasking`
//! here has no such independent exit. It only ever returns when task 0
//! itself voluntarily reaches `int 0x81`. A priority test that let ANY
//! task other than task 0 out-prioritize task 0 forever would hang the
//! ENTIRE BOOT with no safety net at all - so every real priority test
//! built on this module MUST give task 0 the top priority, proving
//! starvation from the OTHER side (a lower-priority task gets nothing)
//! rather than risking task 0 itself never being scheduled. See
//! `ring3::run_priority_ring3_mt_test`'s own doc for the actual test.
//!
//! **Fase 93 lets a NON-task-0 task voluntarily retire early** instead of
//! spinning forever, via a new dedicated vector (`interrupts::RING3_MT_
//! TASK_DONE_INT_VECTOR`, 0x84) calling `task_done` below. `RING3_MT_
//! TASK_DONE` tracks which slots have retired; both `tick` and `task_
//! done` share one selection helper (`select_next`) that skips done
//! slots the same way it already skips lower-priority ones. Task 0
//! itself NEVER uses this new path - it still exits exclusively via the
//! original, unmodified `RING3_EXIT_INT_VECTOR` mechanism (see `run_
//! multitasking`'s own doc for why this is what keeps the selection
//! search from ever coming up empty).
//!
//! **Fase 97 moves `select_next` itself out to `scheduler::tier_select`**,
//! shared with `ring3::coop_select_next`'s own former twin - the two were
//! byte-for-byte identical, only ever parameterized on this module's own
//! `RING3_MT_MAX_TASKS` vs `ring3.rs`'s separate `RING3_COOP_MAX_TASKS`.
//! `tick` and `task_done` below call the shared version exactly as they
//! called the old local one; no behavior changes here at all.

use crate::scheduler::process::Priority;
use crate::scheduler::tier_select::select_next;
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

/// Real, fixed headroom for how many ring-3 tasks this mechanism can
/// track at once - NOT the number actually active in any given run
/// (that's `RING3_MT_ACTIVE_TASKS`, set fresh by each `run_multitasking`
/// call). Comfortably above the 3 tasks Fase 91's own self-test proves,
/// the same "prove generality at a small but genuinely-more-than-2
/// scale, not an arbitrary large one" reasoning this session's own
/// GGUF/quantization Fases already applied one variant at a time.
const RING3_MT_MAX_TASKS: usize = 4;

static RING3_MT_ENABLED: AtomicBool = AtomicBool::new(false);
static RING3_MT_ACTIVE_TASKS: AtomicUsize = AtomicUsize::new(0);
static RING3_MT_CURRENT: AtomicUsize = AtomicUsize::new(0);
static RING3_MT_SWITCH_COUNT: AtomicUsize = AtomicUsize::new(0);

/// Each task's own FIXED, dedicated 160-byte parking buffer address -
/// NEVER shared or reused across tasks, the exact lesson Fase 83's own
/// RSP0-reuse bug taught this codebase the hard way (see that Fase's
/// own module doc in `ring3.rs`). Every slot up to `RING3_MT_ACTIVE_
/// TASKS` needs a real, valid destination address before this mechanism
/// is ever enabled, even though slot 0's own CONTENTS are only ever
/// written before they're first read (task 0's first preemption fills
/// it before any other slot is ever switched away from and back) -
/// `run_multitasking` allocates a real buffer for slot 0 too, not just
/// the other tasks' own fresh bootstrap contexts, precisely because
/// `tick()`'s own `copy_nonoverlapping` needs a genuinely valid ADDRESS
/// to write into regardless of whether the bytes already there matter
/// (an address of `0`, this codebase's own first attempt back in Fase
/// 86, faults immediately - a real, first-boot-caught bug, not a
/// hypothetical).
static mut RING3_MT_TASK_CTX: [u64; RING3_MT_MAX_TASKS] = [0; RING3_MT_MAX_TASKS];

/// Each slot's own priority, as `Priority as u8` (lower value = actually
/// higher priority, matching `scheduler::preemptive`'s own convention
/// exactly - the two schedulers deliberately share one `Priority` enum
/// rather than each defining its own). Set fresh by every `run_
/// multitasking` call, read by `tick()`'s own priority comparison.
static mut RING3_MT_TASK_PRIORITY: [u8; RING3_MT_MAX_TASKS] = [0; RING3_MT_MAX_TASKS];

/// Fase 93: which slots have voluntarily retired via `task_done` below -
/// task 0's own slot (index 0) is NEVER set here, since task 0 always
/// exits through the separate, unmodified `RING3_EXIT_INT_VECTOR` path
/// instead. Reset to all-`false` by every `run_multitasking` call.
static mut RING3_MT_TASK_DONE: [bool; RING3_MT_MAX_TASKS] = [false; RING3_MT_MAX_TASKS];

/// Called from `interrupts::handle_timer_tick` for every tick that
/// interrupts ring-3 code, chained immediately after `ring3_preempt::
/// tick` - a true no-op whenever THIS mechanism isn't the one currently
/// enabled, so the various ring-3 self-tests can never interfere with
/// each other even though all are reached via the same call site (only
/// one is ever active at a time in practice). Saves the CURRENTLY
/// scheduled task's full context into ITS OWN dedicated slot, then picks
/// the NEXT task to resume and returns ITS dedicated context - the one
/// genuine mechanical difference from Fase 85's own `tick`, which always
/// hands back the SAME task it just saved.
///
/// Fase 92: the "next task" pick is now real priority selection, not
/// blind round-robin - find the highest actual priority present among
/// the active slots (lowest `Priority` enum value wins), then round-
/// robin ONLY among slots at that top tier, starting right after
/// `current` and wrapping. This is `scheduler::preemptive::tick`'s own
/// algorithm, generalized from exactly 2 slots to `active_tasks` -
/// reduces to the OLD blind alternation byte-for-byte whenever every
/// active task shares one priority (Fase 91's own test still passes
/// `Priority::Normal` for all 3, preserving its exact behavior), while a
/// genuinely higher-priority task now wins every comparison and
/// monopolizes the CPU completely.
pub fn tick(saved_ctx_ptr: u64) -> u64 {
    if !RING3_MT_ENABLED.load(Ordering::Relaxed) {
        return saved_ctx_ptr;
    }
    let active_tasks = RING3_MT_ACTIVE_TASKS.load(Ordering::Relaxed);
    let current = RING3_MT_CURRENT.load(Ordering::Relaxed);
    unsafe {
        let slots = core::ptr::addr_of!(RING3_MT_TASK_CTX);
        let dest = (*slots)[current];
        core::ptr::copy_nonoverlapping(saved_ctx_ptr as *const u8, dest as *mut u8, 160);
        // Same deliberate poisoning `ring3_preempt::tick` already uses -
        // real proof the NEXT resume below reads from the dedicated
        // copy, not a stale, about-to-be-reused transient location.
        core::ptr::write_bytes(saved_ctx_ptr as *mut u8, 0xAA, 160);

        let prios: &[u8; RING3_MT_MAX_TASKS] = &*core::ptr::addr_of!(RING3_MT_TASK_PRIORITY);
        let done: &[bool; RING3_MT_MAX_TASKS] = &*core::ptr::addr_of!(RING3_MT_TASK_DONE);
        let next = select_next(active_tasks, current, prios, done);

        RING3_MT_CURRENT.store(next, Ordering::Relaxed);
        RING3_MT_SWITCH_COUNT.fetch_add(1, Ordering::Relaxed);
        (*slots)[next]
    }
}

/// Fase 93: called from `ring3_mt_task_done_entry_asm` (`ring3.rs`) when
/// a NON-task-0 ring-3 task voluntarily retires via `int 0x84` instead of
/// spinning forever - the same full-15-GPR-context shape `tick` already
/// handles (this vector's own asm stub is timer_interrupt_entry_asm's
/// shape verbatim, just reached voluntarily rather than by the timer),
/// so the save/poison step is identical to `tick`'s own. The one genuine
/// difference: the CURRENT task is marked `done` before selecting the
/// next one, so neither `tick` nor a later `task_done` call will ever
/// resume it again - it voluntarily forfeits the rest of its slot for
/// good, rather than merely losing this one turn.
///
/// Never called for task 0 (see this module's own doc for why task 0's
/// slot must never be marked done) - by construction, only a ring-3
/// PROGRAM built to execute `int 0x84` can ever reach this at all, and
/// no test in this codebase puts that instruction into task 0's own
/// program.
pub fn task_done(saved_ctx_ptr: u64) -> u64 {
    let active_tasks = RING3_MT_ACTIVE_TASKS.load(Ordering::Relaxed);
    let current = RING3_MT_CURRENT.load(Ordering::Relaxed);
    unsafe {
        let slots = core::ptr::addr_of!(RING3_MT_TASK_CTX);
        let dest = (*slots)[current];
        core::ptr::copy_nonoverlapping(saved_ctx_ptr as *const u8, dest as *mut u8, 160);
        core::ptr::write_bytes(saved_ctx_ptr as *mut u8, 0xAA, 160);

        let done_slots = core::ptr::addr_of_mut!(RING3_MT_TASK_DONE);
        (*done_slots)[current] = true;

        let prios: &[u8; RING3_MT_MAX_TASKS] = &*core::ptr::addr_of!(RING3_MT_TASK_PRIORITY);
        let done: &[bool; RING3_MT_MAX_TASKS] = &*core::ptr::addr_of!(RING3_MT_TASK_DONE);
        let next = select_next(active_tasks, current, prios, done);

        RING3_MT_CURRENT.store(next, Ordering::Relaxed);
        RING3_MT_SWITCH_COUNT.fetch_add(1, Ordering::Relaxed);
        (*slots)[next]
    }
}

/// Arms task 0's own empty parking slot plus every OTHER task's fresh
/// bootstrap context (each built by the caller via `ring3::prepare_
/// ring3_mt_task_ctx`, since laying out that 160-byte shape is
/// ring-3-program-specific and belongs with the rest of this codebase's
/// ring-3 test-authoring logic, not the scheduling mechanism itself),
/// records every task's own priority, then enables priority-aware
/// timer-driven switching across all of them for exactly the duration
/// `f` runs - disabled again before returning, so this mechanism can
/// never affect any OTHER ring-3 self-test elsewhere in the boot
/// sequence, mirroring `ring3_preempt::run_intercepting`'s own
/// enable-only-around-the-call discipline.
///
/// `task0_priority` is task 0's own priority (task 0 is entered
/// normally, by `f` itself calling `enter_ring3`, never through this
/// module's own scheduling). `other_tasks` holds every task BESIDES
/// task 0 as `(priority, bootstrap_context)` pairs - counting task 0
/// itself, the total active task count must not exceed `RING3_MT_MAX_
/// TASKS`, asserted explicitly since silently writing past the fixed
/// slot array would otherwise corrupt adjacent static memory rather
/// than fail cleanly.
///
/// **Give task 0 the highest priority in any real test built on this** -
/// see this module's own doc for why: unlike `scheduler::preemptive`'s
/// own demo, there is no independent tick budget here, only task 0's
/// own eventual `int 0x81`. If some OTHER task could out-prioritize task
/// 0 forever, task 0 would never run again and this call would never
/// return - hanging the entire boot with no safety net.
pub fn run_multitasking<F: FnOnce() -> u64>(
    task0_priority: Priority,
    other_tasks: &[(Priority, [u8; 160])],
    f: F,
) -> (u64, alloc::vec::Vec<u32>, alloc::vec::Vec<bool>, usize) {
    let active_tasks = other_tasks.len() + 1;
    assert!(
        active_tasks <= RING3_MT_MAX_TASKS,
        "run_multitasking: {active_tasks} tasks requested, only {RING3_MT_MAX_TASKS} slots exist"
    );

    let mut bufs: alloc::vec::Vec<alloc::boxed::Box<[u8; 160]>> =
        alloc::vec::Vec::with_capacity(active_tasks);
    bufs.push(alloc::boxed::Box::new([0u8; 160]));
    for (_, ctx) in other_tasks {
        bufs.push(alloc::boxed::Box::new(*ctx));
    }
    unsafe {
        let slots = core::ptr::addr_of_mut!(RING3_MT_TASK_CTX);
        for (i, buf) in bufs.iter_mut().enumerate() {
            (*slots)[i] = buf.as_mut_ptr() as u64;
        }
        let prios = core::ptr::addr_of_mut!(RING3_MT_TASK_PRIORITY);
        (*prios)[0] = task0_priority as u8;
        for (i, (p, _)) in other_tasks.iter().enumerate() {
            (*prios)[i + 1] = *p as u8;
        }
        let done = core::ptr::addr_of_mut!(RING3_MT_TASK_DONE);
        for i in 0..RING3_MT_MAX_TASKS {
            (*done)[i] = false;
        }
    }
    RING3_MT_ACTIVE_TASKS.store(active_tasks, Ordering::Relaxed);
    RING3_MT_CURRENT.store(0, Ordering::Relaxed);
    RING3_MT_SWITCH_COUNT.store(0, Ordering::Relaxed);
    RING3_MT_ENABLED.store(true, Ordering::Relaxed);

    let exit_code = f();

    RING3_MT_ENABLED.store(false, Ordering::Relaxed);
    let switches = RING3_MT_SWITCH_COUNT.load(Ordering::Relaxed);
    // Reading each other task's own LAST-saved `eax` (offset 0 of its
    // 160-byte context, the same layout `ring3_preempt` already relies
    // on) directly out of its dedicated buffer, right here before it's
    // dropped, is the only way to observe whether it genuinely ran its
    // own loop and reached its own post-loop checksum - whether it
    // retired early via `task_done` (Fase 93), never ran at all under
    // real priority starvation (Fase 92, reading back whatever its own
    // cold bootstrap context started with - 0, per `ring3::prepare_
    // ring3_mt_task_ctx`'s own zeroed GPR slots), or simply spun forever
    // and was last observed mid-loop or post-checksum (Fase 86/91's own
    // original tasks).
    let other_last_eax: alloc::vec::Vec<u32> = bufs[1..]
        .iter()
        .map(|buf| u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]))
        .collect();
    // Fase 93: which of the OTHER tasks voluntarily retired via `task_
    // done` - read directly out of the static done-mask (index 0 is
    // task 0's own slot, always false, so this always skips it).
    let other_done: alloc::vec::Vec<bool> = unsafe {
        let done: &[bool; RING3_MT_MAX_TASKS] = &*core::ptr::addr_of!(RING3_MT_TASK_DONE);
        done[1..active_tasks].to_vec()
    };
    drop(bufs);
    (exit_code, other_last_eax, other_done, switches)
}
