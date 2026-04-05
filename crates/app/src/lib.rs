use std::sync::Arc;

use rlm::lambda_rlm::LambdaOptions;
use rlm::rlm::RlmMethod;

pub mod client;
pub mod env;
pub mod launcher;
pub mod pool;
pub mod protocol;
pub mod session;

use protocol::{SandboxRunRequest, SandboxRunResult};

#[derive(Debug, Clone)]
pub struct SandboxWorkerConfig {
    pub method: RlmMethod,
    pub api_key: String,
    pub base_url: String,
    pub model: String,
    pub recursive_model: String,
    pub max_iterations: usize,
    pub depth: usize,
    pub enable_logging: bool,
    pub disable_recursive: bool,
    pub lambda_options: LambdaOptions,
}

#[derive(Debug, Clone)]
pub struct SandboxLaunchConfig {
    pub worker: SandboxWorkerConfig,
}

pub trait SandboxHandle: Send {
    fn run(&mut self, request: SandboxRunRequest) -> Result<SandboxRunResult, String>;
    fn reset(&mut self) -> Result<(), String>;
    fn terminate(&mut self);
    fn identifier(&self) -> String;
}

pub trait SandboxLauncher: Send + Sync {
    fn launch(&self) -> Result<Box<dyn SandboxHandle>, String>;
}

pub type SharedSandboxLauncher = Arc<dyn SandboxLauncher>;
