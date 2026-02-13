use std::io::{BufRead, BufReader, BufWriter, Write};
use std::process::{Child, ChildStdin, ChildStdout};

use crate::SandboxHandle;
use crate::protocol::{SandboxRunRequest, SandboxRunResult, WorkerRequest, WorkerResponse};

pub struct SandboxClient {
    child: Child,
    stdin: BufWriter<ChildStdin>,
    stdout: BufReader<ChildStdout>,
}

impl SandboxClient {
    pub fn new(mut child: Child) -> Result<Self, String> {
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| "sandbox worker missing stdin".to_owned())?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| "sandbox worker missing stdout".to_owned())?;
        Ok(Self {
            child,
            stdin: BufWriter::new(stdin),
            stdout: BufReader::new(stdout),
        })
    }

    pub fn ping(&mut self) -> Result<(), String> {
        match self.send_request(&WorkerRequest::Ping)? {
            WorkerResponse::Pong => Ok(()),
            WorkerResponse::Error { message } => Err(message),
            other => Err(format!("unexpected ping response: {other:?}")),
        }
    }

    fn send_request(&mut self, request: &WorkerRequest) -> Result<WorkerResponse, String> {
        let line = serde_json::to_string(request).map_err(|err| err.to_string())?;
        self.stdin
            .write_all(line.as_bytes())
            .map_err(|err| format!("sandbox worker write failed: {err}"))?;
        self.stdin
            .write_all(b"\n")
            .map_err(|err| format!("sandbox worker write failed: {err}"))?;
        self.stdin
            .flush()
            .map_err(|err| format!("sandbox worker flush failed: {err}"))?;

        let mut response_line = String::new();
        let read = self
            .stdout
            .read_line(&mut response_line)
            .map_err(|err| format!("sandbox worker read failed: {err}"))?;
        if read == 0 {
            return Err("sandbox worker closed stdout".to_owned());
        }
        serde_json::from_str(response_line.trim_end())
            .map_err(|err| format!("sandbox worker invalid response: {err}"))
    }

    fn shutdown_graceful(&mut self) {
        let _ = self.send_request(&WorkerRequest::Shutdown);
    }
}

impl SandboxHandle for SandboxClient {
    fn run(&mut self, request: SandboxRunRequest) -> Result<SandboxRunResult, String> {
        match self.send_request(&WorkerRequest::Run(request))? {
            WorkerResponse::RunResult(result) => Ok(result),
            WorkerResponse::Error { message } => Err(message),
            other => Err(format!("unexpected run response: {other:?}")),
        }
    }

    fn terminate(&mut self) {
        self.shutdown_graceful();
        let _ = self.child.kill();
        let _ = self.child.wait();
    }

    fn identifier(&self) -> String {
        format!("pid:{}", self.child.id())
    }
}

impl Drop for SandboxClient {
    fn drop(&mut self) {
        self.terminate();
    }
}
