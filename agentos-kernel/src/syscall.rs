use crate::kprintln;
use crate::memory::kv_allocator::KV_MANAGER;
use crate::scheduler::agent_scheduler::SCHEDULER;
use crate::scheduler::process::Priority;
use crate::serial_println;
use crate::tensor_engine::TensorEngine;

pub const SYS_SERIAL_PRINT: u64 = 1;
pub const SYS_AGENT_SPAWN: u64 = 2;
pub const SYS_KV_ALLOC: u64 = 3;
pub const SYS_TENSOR_EVAL: u64 = 4;

/// Real arguments for `SYS_TENSOR_EVAL` - a forward-pass layer evaluation
/// (`Y = ReLU(W*X + B)`), matching `TensorEngine::matmul_layer`'s own
/// parameters exactly. Passed as a single pointer via `arg1` rather than
/// packed into raw `u64` registers directly: `matmul_layer` needs four
/// slices plus two dimensions, genuinely more real parameters than 3
/// scalar syscall args could ever encode - the same reason real syscall
/// ABIs commonly pass a pointer to a request struct once a call needs
/// more than a couple of plain numbers.
#[repr(C)]
pub struct TensorEvalArgs {
    pub weights: *const f32,
    pub weights_len: usize,
    pub inputs: *const f32,
    pub inputs_len: usize,
    pub bias: *const f32,
    pub bias_len: usize,
    pub outputs: *mut f32,
    pub outputs_len: usize,
    pub in_dim: usize,
    pub out_dim: usize,
}

/// A zero-length slice's pointer is never actually dereferenced by
/// `core::slice::from_raw_parts` - skipping the check in that case avoids
/// rejecting a legitimate empty slice whose pointer value happens to be
/// a dangling-but-well-aligned sentinel rather than real, mapped memory
/// (a normal, sound Rust convention, not a malformed argument).
///
/// `require_user_accessible` (Fase 76) is what a ring-3-originated call
/// needs and a ring-0 one must NOT be held to: this kernel's own
/// self-tests legitimately pass pointers to ordinary, non-user-
/// accessible heap/stack memory (see `memory/user_page.rs`'s own doc),
/// so requiring `USER_ACCESSIBLE` unconditionally would reject them.
/// Only meaningful because `dispatch_syscall` can now tell the two
/// cases apart for real (Fase 75's own `caller_rpl`).
///
/// `len_bytes` (Fase 77) is the REAL byte length of whatever `ptr` will
/// be turned into a slice over - every page the resulting range
/// `[ptr, ptr + len_bytes)` touches is checked, not just the first one.
/// A slice can start on a real, valid page and still run off the end
/// into an unmapped (or non-user-accessible) one if its stated length
/// is wrong or hostile - a gap `range_is_mapped`'s own predecessor
/// (`pointer_is_mapped`, Fase 74/76) couldn't catch, since it only ever
/// looked at the starting address.
fn pointer_is_mapped_checked(
    name: &str,
    ptr: u64,
    len_bytes: u64,
    require_user_accessible: bool,
) -> bool {
    if len_bytes == 0 {
        return true;
    }
    let ok = if require_user_accessible {
        crate::memory::paging::range_is_user_accessible(ptr, len_bytes)
    } else {
        crate::memory::paging::range_is_mapped(ptr, len_bytes)
    };
    if ok {
        true
    } else {
        kprintln!(
            "[SYSCALL ERROR] SYS_TENSOR_EVAL: {} range {:#x}+{:#x} rejected (require_user_accessible={})",
            name,
            ptr,
            len_bytes,
            require_user_accessible
        );
        false
    }
}

/// `caller_is_ring3` (Fase 75/76): whether the CPU's own interrupt frame
/// showed `CS.RPL == 3` at the moment this syscall's `int 0x80` fired -
/// real, hardware-reported information, not a claim the caller makes
/// about itself. Used below to decide how strictly `SYS_TENSOR_EVAL`'s
/// pointer arguments are checked; every other syscall number ignores it
/// for now.
pub fn dispatch_syscall(
    sys_nr: u64,
    arg1: u64,
    arg2: u64,
    _arg3: u64,
    caller_is_ring3: bool,
) -> u64 {
    match sys_nr {
        SYS_SERIAL_PRINT => {
            serial_println!("[SYSCALL PRINT] Direct Kernel Syscall executed.");
            kprintln!("[SYSCALL] System Call 1 executed: PRINT");
            0
        }
        SYS_AGENT_SPAWN => {
            let mut sched = SCHEDULER.lock();
            let pid = sched.spawn("userspace-agent-sys", Priority::Normal, arg1 as usize);
            match pid {
                Some(p) => {
                    kprintln!("[SYSCALL] Agent spawned via Syscall: PID {}", p);
                    p as u64
                }
                None => 0,
            }
        }
        SYS_KV_ALLOC => {
            let mut kv = KV_MANAGER.lock();
            let block_id = kv.allocate_kv_block(arg1 as u32, arg2 as usize);
            match block_id {
                Some(b) => {
                    kprintln!("[SYSCALL] Allocated KV Cache Block #{}", b);
                    b as u64
                }
                None => 0,
            }
        }
        SYS_TENSOR_EVAL => {
            // Fase 74: this kernel's ring-3 arc is no longer hypothetical
            // (Fase 68-73 built a real GDT/TSS, a DPL=3 syscall gate that
            // reads genuine ring-3 registers, and a safe ring-3->ring-0
            // return path) - a syscall argument pointer arriving here can
            // genuinely have come from ring-3 code now, not just this
            // kernel's own trusted self-tests. `pointer_is_mapped_checked`
            // checks every pointer this arm dereferences BEFORE it ever
            // touches one, so a bad pointer (accidental or deliberate)
            // becomes a clean `u64::MAX` instead of an unhandled `#PF`
            // that halts the whole kernel via the existing
            // `page_fault_handler` - see that function's own doc for the
            // full reasoning. Fase 76: when `caller_is_ring3` is true,
            // this now ALSO requires `USER_ACCESSIBLE` on every pointer -
            // real, present kernel-only memory (the heap, in particular)
            // is no longer good enough for a ring-3 caller to point at,
            // closing the "confused deputy" gap `pointer_is_mapped` alone
            // couldn't (kernel memory legitimately passes that weaker
            // check). Ring-0 callers - this kernel's own self-tests,
            // which legitimately use ordinary heap/stack memory - are
            // unaffected, since `caller_is_ring3` is false for them.
            //
            // Fase 77: every length below is now a real BYTE count
            // (`len * size_of::<f32>()`, `saturating_mul` since a
            // hostile `len` could otherwise overflow the multiplication
            // and wrap to a small, wrongly-passing value), and
            // `range_is_mapped`/`range_is_user_accessible` check EVERY
            // page that byte range touches, not just its first one - a
            // slice starting on a valid page can still run off the end
            // into an unmapped or kernel-only one if `len` is wrong.
            const F32_SIZE: u64 = core::mem::size_of::<f32>() as u64;
            if !pointer_is_mapped_checked(
                "args",
                arg1,
                core::mem::size_of::<TensorEvalArgs>() as u64,
                caller_is_ring3,
            ) {
                return u64::MAX;
            }
            let args_ptr = arg1 as *const TensorEvalArgs;
            // SAFETY: args_ptr's own page was just confirmed present
            // above - safe to read the struct's fields (though not yet
            // whatever THEY point to, each checked individually next).
            let args = unsafe { &*args_ptr };
            if !pointer_is_mapped_checked(
                "weights",
                args.weights as u64,
                (args.weights_len as u64).saturating_mul(F32_SIZE),
                caller_is_ring3,
            ) || !pointer_is_mapped_checked(
                "inputs",
                args.inputs as u64,
                (args.inputs_len as u64).saturating_mul(F32_SIZE),
                caller_is_ring3,
            ) || !pointer_is_mapped_checked(
                "bias",
                args.bias as u64,
                (args.bias_len as u64).saturating_mul(F32_SIZE),
                caller_is_ring3,
            ) || !pointer_is_mapped_checked(
                "outputs",
                args.outputs as u64,
                (args.outputs_len as u64).saturating_mul(F32_SIZE),
                caller_is_ring3,
            ) {
                return u64::MAX;
            }
            // SAFETY: every pointer above was just confirmed to point at
            // real, present memory - trusted at exactly the level every
            // other raw pointer already used throughout this codebase is
            // (e.g. e1000's DMA buffers), now with an actual check behind
            // that trust instead of an unconditional assumption. Fase 77
            // closed the one gap an earlier version of this comment used
            // to flag here: `pointer_is_mapped_checked` above was passed
            // each pointer's REAL byte length, and checks every page the
            // resulting range touches, not just the first one - a slice
            // cannot run off the end into unmapped memory without that
            // being caught above, before any of these four are built.
            unsafe {
                let weights = core::slice::from_raw_parts(args.weights, args.weights_len);
                let inputs = core::slice::from_raw_parts(args.inputs, args.inputs_len);
                let bias = core::slice::from_raw_parts(args.bias, args.bias_len);
                let outputs = core::slice::from_raw_parts_mut(args.outputs, args.outputs_len);
                TensorEngine::matmul_layer(
                    weights,
                    inputs,
                    bias,
                    outputs,
                    args.in_dim,
                    args.out_dim,
                );
            }
            kprintln!("[SYSCALL] Tensor evaluation executed via syscall (SYS_TENSOR_EVAL)");
            serial_println!("[SYSCALL] tensor_eval executed");
            0
        }
        _ => {
            kprintln!("[SYSCALL ERROR] Unknown System Call Number: {}", sys_nr);
            u64::MAX
        }
    }
}
