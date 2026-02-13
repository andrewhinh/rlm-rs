use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SandboxRunRequest {
    pub initialize: bool,
    pub query: String,
    pub context: Option<Value>,
    pub code: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SandboxRunResult {
    pub response: Option<String>,
    pub stdout: Option<String>,
    pub stderr: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WorkerRequest {
    Ping,
    Run(SandboxRunRequest),
    Shutdown,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WorkerResponse {
    Pong,
    Ack,
    RunResult(SandboxRunResult),
    Error { message: String },
}
