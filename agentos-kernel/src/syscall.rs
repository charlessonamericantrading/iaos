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

pub fn dispatch_syscall(sys_nr: u64, arg1: u64, arg2: u64, _arg3: u64) -> u64 {
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
            let args_ptr = arg1 as *const TensorEvalArgs;
            if args_ptr.is_null() {
                kprintln!("[SYSCALL ERROR] SYS_TENSOR_EVAL: null args pointer");
                return u64::MAX;
            }
            // SAFETY: this kernel has no user/kernel memory separation
            // yet (see gdt.rs - no user-mode segments, no ring-3
            // transition exists at all), so a syscall argument pointer
            // is trusted at exactly the same level every other raw
            // pointer already used throughout this codebase is (e.g.
            // e1000's DMA buffers) - not a new category of risk, just
            // this module's first occurrence of it. The caller (today,
            // only this kernel's own self-test) is responsible for
            // `args_ptr` pointing at a real, live `TensorEvalArgs` whose
            // own slice fields are valid for their stated lengths.
            unsafe {
                let args = &*args_ptr;
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
