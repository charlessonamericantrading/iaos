//! Fase 86: genuine multi-task ring-3 scheduling via the timer - actually
//! running a DIFFERENT ring-3 program in the gap a preempted one leaves
//! behind, not just resuming the SAME one the way `scheduler::ring3_
//! preempt` (Fase 85) deliberately, minimally proved first. Mirrors
//! `scheduler::preemptive`'s own ring-0 round-robin shape almost
//! exactly (two tasks, alternating on every tick), but switching
//! between two FULL 160-byte (15-GPR + 5-field `iretq` frame) ring-3
//! contexts instead of ring-0's much smaller 6-callee-saved-register
//! `switch_to` convention, for the exact same reason Fase 85 itself
//! needed the bigger shape: a real, involuntary timer tick can land at
//! any arbitrary point in a ring-3 program's own execution, so nothing
//! less than the FULL register set Fase 84's naked stub already
//! captures is enough to resume it correctly later.
//!
//! **Deliberately hardcoded to exactly 2 tasks, alternating
//! unconditionally** - the same simplification `ring3::run_ring3_
//! cooperative_test` (Fase 83) already chose for the voluntary-yield
//! case, for the same reason: a real, general N-task, priority-aware
//! ring-3 scheduler is separate, substantially larger follow-on work,
//! and proving genuine involuntary interleaving works AT ALL is worth
//! isolating from that harder problem first.

use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

const RING3_MT_TASK_COUNT: usize = 2;

static RING3_MT_ENABLED: AtomicBool = AtomicBool::new(false);
static RING3_MT_CURRENT: AtomicUsize = AtomicUsize::new(0);
static RING3_MT_SWITCH_COUNT: AtomicUsize = AtomicUsize::new(0);

/// Each task's own FIXED, dedicated 160-byte parking buffer address -
/// NEVER shared or reused across tasks, the exact lesson Fase 83's own
/// RSP0-reuse bug taught this codebase the hard way (see that Fase's
/// own module doc in `ring3.rs`). BOTH slots need a real, valid
/// destination address before this mechanism is ever enabled, even
/// though slot 0's own CONTENTS are only ever written before they're
/// first read (task 0's first preemption fills it before task 1's own
/// slot is ever switched away from and back) - `run_multitasking`
/// allocates a real buffer for slot 0 too, not just slot 1's fresh
/// bootstrap context, precisely because `tick()`'s own `copy_
/// nonoverlapping` needs a genuinely valid ADDRESS to write into
/// regardless of whether the bytes already there matter (an address of
/// `0`, this codebase's own first attempt, faults immediately - a real,
/// first-boot-caught bug, not a hypothetical).
static mut RING3_MT_TASK_CTX: [u64; RING3_MT_TASK_COUNT] = [0; RING3_MT_TASK_COUNT];

/// Called from `interrupts::handle_timer_tick` for every tick that
/// interrupts ring-3 code, chained immediately after `ring3_preempt::
/// tick` - a true no-op whenever THIS mechanism isn't the one currently
/// enabled, so the two self-tests can never interfere with each other
/// even though both are reached via the same call site (only one is
/// ever active at a time in practice). Saves the CURRENTLY scheduled
/// task's full context into ITS OWN dedicated slot, advances to the
/// next task round-robin, and returns the NEW task's own dedicated
/// context - the one genuine mechanical difference from Fase 85's own
/// `tick`, which always hands back the SAME task it just saved.
pub fn tick(saved_ctx_ptr: u64) -> u64 {
    if !RING3_MT_ENABLED.load(Ordering::Relaxed) {
        return saved_ctx_ptr;
    }
    let current = RING3_MT_CURRENT.load(Ordering::Relaxed);
    unsafe {
        let slots = core::ptr::addr_of!(RING3_MT_TASK_CTX);
        let dest = (*slots)[current];
        core::ptr::copy_nonoverlapping(saved_ctx_ptr as *const u8, dest as *mut u8, 160);
        // Same deliberate poisoning `ring3_preempt::tick` already uses -
        // real proof the NEXT resume below reads from the dedicated
        // copy, not a stale, about-to-be-reused transient location.
        core::ptr::write_bytes(saved_ctx_ptr as *mut u8, 0xAA, 160);
        let next = (current + 1) % RING3_MT_TASK_COUNT;
        RING3_MT_CURRENT.store(next, Ordering::Relaxed);
        RING3_MT_SWITCH_COUNT.fetch_add(1, Ordering::Relaxed);
        (*slots)[next]
    }
}

/// Arms task 1's own fresh bootstrap context (built by the caller via
/// `ring3::prepare_ring3_mt_task_ctx`, since laying out that 160-byte
/// shape is ring-3-program-specific and belongs with the rest of this
/// codebase's ring-3 test-authoring logic, not the scheduling mechanism
/// itself) and enables round-robin timer-driven switching for exactly
/// the duration `f` runs - disabled again before returning, so this
/// mechanism can never affect any OTHER ring-3 self-test elsewhere in
/// the boot sequence, mirroring `ring3_preempt::run_intercepting`'s own
/// enable-only-around-the-call discipline.
pub fn run_multitasking<F: FnOnce() -> u64>(task1_ctx: [u8; 160], f: F) -> (u64, u32, usize) {
    let mut buf0 = alloc::boxed::Box::new([0u8; 160]);
    let mut buf1 = alloc::boxed::Box::new(task1_ctx);
    unsafe {
        *core::ptr::addr_of_mut!(RING3_MT_TASK_CTX) =
            [buf0.as_mut_ptr() as u64, buf1.as_mut_ptr() as u64];
    }
    RING3_MT_CURRENT.store(0, Ordering::Relaxed);
    RING3_MT_SWITCH_COUNT.store(0, Ordering::Relaxed);
    RING3_MT_ENABLED.store(true, Ordering::Relaxed);

    let exit_code = f();

    RING3_MT_ENABLED.store(false, Ordering::Relaxed);
    let switches = RING3_MT_SWITCH_COUNT.load(Ordering::Relaxed);
    // Task 1 never voluntarily exits (see `ring3::run_ring3_mt_test`'s
    // own doc) - reading its own LAST-saved `eax` (offset 0 of its
    // 160-byte context, the same layout `ring3_preempt` already relies
    // on) directly out of its dedicated buffer, right here before it's
    // dropped, is the only way to observe whether it genuinely ran its
    // own loop and reached its own post-loop checksum.
    let task1_last_eax = u32::from_le_bytes([buf1[0], buf1[1], buf1[2], buf1[3]]);
    drop(buf1);
    drop(buf0);
    (exit_code, task1_last_eax, switches)
}
