pub mod client;
pub mod launcher;
pub mod pool;
pub mod protocol;

use protocol::{SandboxRunRequest, SandboxRunResult};

#[derive(Debug, Clone)]
pub struct SandboxWorkerConfig {
    pub api_key: String,
}

#[derive(Debug, Clone)]
pub struct SandboxLaunchConfig {
    pub worker: SandboxWorkerConfig,
}

pub trait SandboxHandle: Send {
    fn run(&mut self, request: SandboxRunRequest) -> Result<SandboxRunResult, String>;
    fn terminate(&mut self);
    fn identifier(&self) -> String;
}

pub trait SandboxLauncher: Send {
    fn launch(&self) -> Result<Box<dyn SandboxHandle>, String>;
}
