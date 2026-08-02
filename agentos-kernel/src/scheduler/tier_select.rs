//! Fase 97: the ONE real piece of pure scheduling logic Fase 92/93 (on
//! `scheduler::ring3_mt`) and Fase 95/96 (on `ring3_coop`, in `ring3.rs`)
//! ended up hand-porting into two separate, byte-for-byte identical
//! copies - `ring3_mt::select_next` and `ring3::coop_select_next` - since
//! both mechanisms independently needed "find the top-priority tier
//! among active, not-done slots, then round-robin within it starting
//! after `current`". Unlike the two mechanisms' own context-save shapes
//! (160-byte full-GPR vs 96-byte callee-saved - genuinely different,
//! deliberately kept separate) or `scheduler::preemptive::tick`'s own
//! version (a fixed 2-slot `Option<PreemptTask>` array, not parallel
//! `prios`/`done` arrays - a real structural difference, not just a
//! renamed constant), THESE two were never actually different: same
//! parameter shapes, same body, same algorithm, just parameterized on
//! two differently-NAMED (but so far identically-VALUED) `MAX_TASKS`
//! constants in two different modules. A const generic collapses that
//! into one real function both call, deleting the duplication instead
//! of merely documenting it.
//!
//! This is deliberately a SMALL, safe slice of the larger "unify the two
//! ring-3 scheduling mechanisms" item flagged as open since Fase 96 -
//! the asm entry stubs, the context formats, and the per-mechanism
//! static state all stay exactly as they were. Only the pure-Rust tier-
//! search itself moves.

/// Finds the highest actual priority present among ACTIVE, NOT-DONE
/// slots (lowest value wins, matching `scheduler::process::Priority as
/// u8`'s own convention), then round-robins only within that top tier,
/// starting right after `current` and wrapping. Identical to what was
/// `ring3_mt::select_next` and `ring3::coop_select_next` before this
/// Fase - callers are responsible for the same guarantee both call
/// sites already relied on: at least one non-`done` slot must exist
/// among the first `active_tasks` entries, or this loops forever.
pub fn select_next<const N: usize>(
    active_tasks: usize,
    current: usize,
    prios: &[u8; N],
    done: &[bool; N],
) -> usize {
    let mut top = u8::MAX;
    for i in 0..active_tasks {
        if !done[i] && prios[i] < top {
            top = prios[i];
        }
    }
    let mut next = (current + 1) % active_tasks;
    while done[next] || prios[next] != top {
        next = (next + 1) % active_tasks;
    }
    next
}
