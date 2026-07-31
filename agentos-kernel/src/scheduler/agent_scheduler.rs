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
}

lazy_static! {
    pub static ref SCHEDULER: Mutex<NativeAgentScheduler> = Mutex::new(NativeAgentScheduler::new());
}
