use std::collections::VecDeque;

use crate::{SandboxHandle, SharedSandboxLauncher};

pub struct SandboxPool {
    launcher: SharedSandboxLauncher,
    idle: VecDeque<Box<dyn SandboxHandle>>,
    target_idle: usize,
}

impl SandboxPool {
    pub fn new(launcher: SharedSandboxLauncher, target_idle: usize) -> Result<Self, String> {
        let mut pool = Self {
            launcher,
            idle: VecDeque::new(),
            target_idle,
        };
        pool.refill_strict()?;
        Ok(pool)
    }

    pub fn acquire_idle(&mut self) -> Option<Box<dyn SandboxHandle>> {
        self.idle.pop_front()
    }

    pub fn add_idle(&mut self, handle: Box<dyn SandboxHandle>) {
        self.idle.push_back(handle);
    }

    pub fn idle_len(&self) -> usize {
        self.idle.len()
    }

    pub fn target_idle(&self) -> usize {
        self.target_idle
    }

    pub fn launcher(&self) -> SharedSandboxLauncher {
        self.launcher.clone()
    }

    fn refill_strict(&mut self) -> Result<(), String> {
        while self.idle.len() < self.target_idle {
            self.idle.push_back(self.launcher.launch()?);
        }
        Ok(())
    }
}
