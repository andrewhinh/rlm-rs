use std::collections::VecDeque;

use crate::{SandboxHandle, SandboxLauncher};

pub struct SandboxPool {
    launcher: Box<dyn SandboxLauncher>,
    idle: VecDeque<Box<dyn SandboxHandle>>,
    target_idle: usize,
}

impl SandboxPool {
    pub fn new(launcher: Box<dyn SandboxLauncher>, target_idle: usize) -> Result<Self, String> {
        let mut pool = Self {
            launcher,
            idle: VecDeque::new(),
            target_idle,
        };
        pool.refill_strict()?;
        Ok(pool)
    }

    pub fn acquire(&mut self) -> Result<Box<dyn SandboxHandle>, String> {
        let handle = if let Some(handle) = self.idle.pop_front() {
            handle
        } else {
            self.launcher.launch()?
        };
        self.refill_best_effort();
        Ok(handle)
    }

    pub fn retire(&mut self, mut handle: Box<dyn SandboxHandle>) {
        handle.terminate();
        self.refill_best_effort();
    }

    pub fn idle_len(&self) -> usize {
        self.idle.len()
    }

    fn refill_strict(&mut self) -> Result<(), String> {
        while self.idle.len() < self.target_idle {
            self.idle.push_back(self.launcher.launch()?);
        }
        Ok(())
    }

    fn refill_best_effort(&mut self) {
        while self.idle.len() < self.target_idle {
            match self.launcher.launch() {
                Ok(handle) => self.idle.push_back(handle),
                Err(_) => break,
            }
        }
    }
}
