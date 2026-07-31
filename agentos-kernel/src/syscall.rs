use crate::kprintln;
use crate::memory::kv_allocator::KV_MANAGER;
use crate::scheduler::agent_scheduler::SCHEDULER;
use crate::scheduler::process::Priority;
use crate::serial_println;

pub const SYS_SERIAL_PRINT: u64 = 1;
pub const SYS_AGENT_SPAWN: u64 = 2;
pub const SYS_KV_ALLOC: u64 = 3;
pub const SYS_TENSOR_EVAL: u64 = 4;

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
        _ => {
            kprintln!("[SYSCALL ERROR] Unknown System Call Number: {}", sys_nr);
            u64::MAX
        }
    }
}
