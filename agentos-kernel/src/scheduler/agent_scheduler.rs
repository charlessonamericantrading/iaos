use super::process::{Priority, ProcessControlBlock, ProcessState};
use lazy_static::lazy_static;
use spin::Mutex;

const MAX_PROCESSES: usize = 32;

pub struct NativeAgentScheduler {
    processes: [Option<ProcessControlBlock>; MAX_PROCESSES],
    current_pid: u32,
    next_pid: u32,
}

impl NativeAgentScheduler {
    pub const fn new() -> Self {
        const EMPTY_PROC: Option<ProcessControlBlock> = None;
        NativeAgentScheduler {
            processes: [EMPTY_PROC; MAX_PROCESSES],
            current_pid: 0,
            next_pid: 1,
        }
    }

    pub fn spawn(&mut self, name: &'static str, priority: Priority, quota: usize) -> Option<u32> {
        for slot in self.processes.iter_mut() {
            if slot.is_none() {
                let pid = self.next_pid;
                self.next_pid += 1;
                *slot = Some(ProcessControlBlock::new(pid, name, priority, quota));
                return Some(pid);
            }
        }
        None
    }

    pub fn schedule_next(&mut self) -> Option<u32> {
        let mut best_index: Option<usize> = None;
        let mut highest_prio = 99;

        for (idx, slot) in self.processes.iter().enumerate() {
            if let Some(ref proc) = slot {
                if proc.state == ProcessState::Ready {
                    let p_val = proc.priority as u8;
                    if p_val < highest_prio {
                        highest_prio = p_val;
                        best_index = Some(idx);
                    }
                }
            }
        }

        if let Some(idx) = best_index {
            if let Some(ref mut proc) = self.processes[idx] {
                proc.state = ProcessState::Running;
                self.current_pid = proc.pid;
                return Some(proc.pid);
            }
        }
        None
    }

    pub fn get_active_count(&self) -> usize {
        self.processes.iter().filter(|p| p.is_some()).count()
    }

    /// Read-only iteration over live processes, for the `ps` shell command -
    /// keeps the fixed-size `processes` array a private implementation
    /// detail instead of exposing it directly.
    pub fn for_each_process<F: FnMut(&ProcessControlBlock)>(&self, mut f: F) {
        for slot in self.processes.iter().flatten() {
            f(slot);
        }
    }

    /// Updates an existing process's scheduling state and last-known stack
    /// pointer by pid - lets a real task scheduler (currently the
    /// cooperative one in `context_switch.rs`) keep this `ps`-visible table
    /// honest as tasks actually run/yield/finish, instead of every entry
    /// only ever reflecting whatever `spawn` set once at boot.
    pub fn update_process(&mut self, pid: u32, state: ProcessState, stack_pointer: usize) {
        for slot in self.processes.iter_mut().flatten() {
            if slot.pid == pid {
                slot.state = state;
                slot.stack_pointer = stack_pointer;
                return;
            }
        }
    }

    /// Records which real KV-cache block `pid`'s own process now owns -
    /// the same narrowly-scoped, find-by-pid shape `update_process`
    /// already established, kept separate rather than folding into that
    /// function since most `update_process` callers have no block id to
    /// report at all. Fase 143: `kv_block_id` existed on
    /// `ProcessControlBlock` since before this session's own start, but
    /// nothing ever called a setter for it - the same "defined but never
    /// wired up" gap Fase 55/136/138 already found and closed elsewhere.
    pub fn set_kv_block_id(&mut self, pid: u32, block_id: u32) {
        for slot in self.processes.iter_mut().flatten() {
            if slot.pid == pid {
                slot.kv_block_id = Some(block_id);
                return;
            }
        }
    }
}

lazy_static! {
    pub static ref SCHEDULER: Mutex<NativeAgentScheduler> = Mutex::new(NativeAgentScheduler::new());
}
