//! Fase 85: the first genuine, INVOLUNTARY (timer-driven) ring-3
//! preemption this kernel has ever attempted - a real hardware tick,
//! landing at an arbitrary point a ring-3 program did NOT choose,
//! captures that program's COMPLETE register state (all 15 general-
//! purpose registers Fase 84's own naked timer stub now saves, not just
//! the 6 callee-saved ones `switch_to`/Fase 83's own voluntary yield
//! already handle) and correctly restores it - proving the mechanism
//! Fase 84 laid the groundwork for actually works, before attempting the
//! harder "switch the CPU to something ELSE while preempted" step (real,
//! separate, larger follow-on work).
//!
//! **Deliberately the SIMPLEST possible proof.** This does NOT yet run
//! anything else while the ring-3 program is preempted - it copies the
//! intercepted snapshot into dedicated memory and immediately resumes
//! FROM THAT COPY (not the original, transient stack location the timer
//! stub built it on), proving the save+restore round trip itself is
//! byte-correct for the FULL register set - the necessary prerequisite
//! before genuine "run something else in between" scheduling can be
//! trusted at all. Mirrors this whole arc's own established discipline:
//! Fase 79 proved detection before Fase 80 used it; Fase 81 proved the
//! switch_to-bootstrap entry before Fase 83 used it for cooperative
//! multi-tasking; this Fase proves the full-context save/restore round
//! trip before any later Fase attempts real interleaving with it.

use core::sync::atomic::{AtomicBool, Ordering};

static RING3_FULL_PREEMPT_ENABLED: AtomicBool = AtomicBool::new(false);
static RING3_FULL_PREEMPT_HIT: AtomicBool = AtomicBool::new(false);

/// The dedicated 160-byte (20-quadword: 15 GPRs + 5-field iretq frame)
/// parking buffer's own address - set once per `run_intercepting` call,
/// read by `tick()` below. `0` means "not currently armed."
static mut RING3_FULL_TASK_CTX: u64 = 0;

/// Called from `interrupts::handle_timer_tick` whenever a tick
/// interrupts ring-3 code, with `saved_ctx_ptr` pointing at the base of
/// the naked timer stub's own 15-GPR save block (the CPU's own 5-field
/// iretq frame sits directly above it - all 20 quadwords / 160 bytes
/// contiguous, see `interrupts.rs`'s own doc for the exact shape).
/// Returns the RSP the stub's own epilogue should actually resume
/// from: `saved_ctx_ptr` itself, UNCHANGED, whenever this mechanism
/// isn't active - a real no-op, byte-identical to Fase 84's own
/// pure-refactor behavior - or the dedicated copy's own address once
/// this test claims its one-and-only interception.
pub fn tick(saved_ctx_ptr: u64) -> u64 {
    if !RING3_FULL_PREEMPT_ENABLED.load(Ordering::Relaxed) {
        return saved_ctx_ptr;
    }
    if RING3_FULL_PREEMPT_HIT.swap(true, Ordering::Relaxed) {
        // Already claimed this run's one-and-only interception - a
        // second tick landing before the ring-3 program finishes must
        // NOT re-copy over the dedicated buffer or re-poison the (now
        // already-resumed-from) original location; the first
        // interception is already enough to prove the mechanism, and
        // this keeps every LATER tick during the same ring-3 run a
        // genuine no-op instead of repeated churn.
        return saved_ctx_ptr;
    }
    unsafe {
        let dest = core::ptr::addr_of!(RING3_FULL_TASK_CTX).read();
        core::ptr::copy_nonoverlapping(saved_ctx_ptr as *const u8, dest as *mut u8, 160);
        // Deliberately poison the ORIGINAL location - real proof the
        // resume below reads from the DEDICATED COPY, not merely "the
        // registers happened to survive because nothing touched them" -
        // the exact class of bug Fase 83's own cooperative yield caught
        // the hard way (reusing a shared, soon-overwritten stack instead
        // of copying out).
        core::ptr::write_bytes(saved_ctx_ptr as *mut u8, 0xAA, 160);
        dest
    }
}

/// Arms a dedicated 160-byte parking buffer and enables interception for
/// exactly the duration `f` runs - disabled again before returning, so
/// this still-narrow mechanism can never affect any OTHER ring-3
/// self-test elsewhere in the boot sequence (every one of which assumes
/// today's Fase-80 "ticks during ring-3 are invisible" behavior,
/// unrelated to this new one). Returns `f`'s own return value alongside
/// whether an interception genuinely happened - a real run could
/// (rarely) complete with zero ticks landing during `f`'s own ring-3
/// window, and the caller needs to tell that apart from a genuine,
/// verified round trip.
pub fn run_intercepting<F: FnOnce() -> u64>(f: F) -> (u64, bool) {
    let mut ctx_buf = alloc::boxed::Box::new([0u8; 160]);
    unsafe {
        *core::ptr::addr_of_mut!(RING3_FULL_TASK_CTX) = ctx_buf.as_mut_ptr() as u64;
    }
    RING3_FULL_PREEMPT_HIT.store(false, Ordering::Relaxed);
    RING3_FULL_PREEMPT_ENABLED.store(true, Ordering::Relaxed);

    let exit_code = f();

    RING3_FULL_PREEMPT_ENABLED.store(false, Ordering::Relaxed);
    let hit = RING3_FULL_PREEMPT_HIT.load(Ordering::Relaxed);
    drop(ctx_buf);
    (exit_code, hit)
}
