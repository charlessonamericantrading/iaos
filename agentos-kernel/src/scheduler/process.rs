#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProcessState {
    Ready,
    Running,
    Blocked,
    Terminated,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Priority {
    KernelCritical = 0,
    High = 1,
    Normal = 2,
    Background = 3,
}

pub struct ProcessControlBlock {
    pub pid: u32,
    pub name: &'static str,
    pub state: ProcessState,
    pub priority: Priority,
    pub token_quota: usize,
    pub tokens_used: usize,
    pub kv_block_id: Option<u32>,
    pub stack_pointer: usize,
}

impl ProcessControlBlock {
    pub fn new(pid: u32, name: &'static str, priority: Priority, token_quota: usize) -> Self {
        ProcessControlBlock {
            pid,
            name,
            state: ProcessState::Ready,
            priority,
            token_quota,
            tokens_used: 0,
            kv_block_id: None,
            stack_pointer: 0,
        }
    }
}
