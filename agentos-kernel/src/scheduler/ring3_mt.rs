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

/// Called from `interrupts::handle_timer_tick` for every tick that
/// interrupts ring-3 code, chained immediately after `ring3_preempt::
/// tick` - a true no-op whenever THIS mechanism isn't the one currently
/// enabled, so the various ring-3 self-tests can never interfere with
/// each other even though all are reached via the same call site (only
/// one is ever active at a time in practice). Saves the CURRENTLY
/// scheduled task's full context into ITS OWN dedicated slot, advances
/// to the next task round-robin (wrapping at the REAL active count for
/// this run, not the fixed maximum), and returns the NEW task's own
/// dedicated context - the one genuine mechanical difference from Fase
/// 85's own `tick`, which always hands back the SAME task it just saved.
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
        let next = (current + 1) % active_tasks;
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
/// then enables round-robin timer-driven switching across all of them
/// for exactly the duration `f` runs - disabled again before returning,
/// so this mechanism can never affect any OTHER ring-3 self-test
/// elsewhere in the boot sequence, mirroring `ring3_preempt::run_
/// intercepting`'s own enable-only-around-the-call discipline.
///
/// `other_task_ctxs` holds every task BESIDES task 0 (which is entered
/// normally, by `f` itself calling `enter_ring3`) - `other_task_ctxs.
/// len() + 1` must not exceed `RING3_MT_MAX_TASKS`, asserted explicitly
/// since silently writing past the fixed slot array would otherwise
/// corrupt adjacent static memory rather than fail cleanly.
pub fn run_multitasking<F: FnOnce() -> u64>(
    other_task_ctxs: &[[u8; 160]],
    f: F,
) -> (u64, alloc::vec::Vec<u32>, usize) {
    let active_tasks = other_task_ctxs.len() + 1;
    assert!(
        active_tasks <= RING3_MT_MAX_TASKS,
        "run_multitasking: {active_tasks} tasks requested, only {RING3_MT_MAX_TASKS} slots exist"
    );

    let mut bufs: alloc::vec::Vec<alloc::boxed::Box<[u8; 160]>> =
        alloc::vec::Vec::with_capacity(active_tasks);
    bufs.push(alloc::boxed::Box::new([0u8; 160]));
    for ctx in other_task_ctxs {
        bufs.push(alloc::boxed::Box::new(*ctx));
    }
    unsafe {
        let slots = core::ptr::addr_of_mut!(RING3_MT_TASK_CTX);
        for (i, buf) in bufs.iter_mut().enumerate() {
            (*slots)[i] = buf.as_mut_ptr() as u64;
        }
    }
    RING3_MT_ACTIVE_TASKS.store(active_tasks, Ordering::Relaxed);
    RING3_MT_CURRENT.store(0, Ordering::Relaxed);
    RING3_MT_SWITCH_COUNT.store(0, Ordering::Relaxed);
    RING3_MT_ENABLED.store(true, Ordering::Relaxed);

    let exit_code = f();

    RING3_MT_ENABLED.store(false, Ordering::Relaxed);
    let switches = RING3_MT_SWITCH_COUNT.load(Ordering::Relaxed);
    // None of the other tasks ever voluntarily exit (see `ring3::run_
    // ring3_mt_test`'s own doc) - reading each one's own LAST-saved
    // `eax` (offset 0 of its 160-byte context, the same layout `ring3_
    // preempt` already relies on) directly out of its dedicated buffer,
    // right here before it's dropped, is the only way to observe
    // whether it genuinely ran its own loop and reached its own
    // post-loop checksum.
    let other_last_eax: alloc::vec::Vec<u32> = bufs[1..]
        .iter()
        .map(|buf| u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]))
        .collect();
    drop(bufs);
    (exit_code, other_last_eax, switches)
}
