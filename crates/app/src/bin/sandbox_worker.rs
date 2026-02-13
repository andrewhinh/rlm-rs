use std::env;
use std::io::{self, BufRead, Write};

use app::protocol::{SandboxRunRequest, SandboxRunResult, WorkerRequest, WorkerResponse};
use rlm::prompts::DEFAULT_QUERY;
use rlm::rlm::{RlmConfig, RlmRepl};
use rlm::utils::context_from_value;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let config = worker_config_from_env()?;
    let mut repl = RlmRepl::new(config)?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build()?;

    let stdin = io::stdin();
    let mut stdout = io::stdout();
    for line in stdin.lock().lines() {
        let line = match line {
            Ok(line) => line,
            Err(err) => {
                let _ = emit(
                    &mut stdout,
                    &WorkerResponse::Error {
                        message: format!("stdin read failed: {err}"),
                    },
                );
                continue;
            }
        };
        if line.trim().is_empty() {
            continue;
        }
        let request = match serde_json::from_str::<WorkerRequest>(&line) {
            Ok(request) => request,
            Err(err) => {
                let _ = emit(
                    &mut stdout,
                    &WorkerResponse::Error {
                        message: format!("invalid request: {err}"),
                    },
                );
                continue;
            }
        };
        match request {
            WorkerRequest::Ping => emit(&mut stdout, &WorkerResponse::Pong)?,
            WorkerRequest::Shutdown => {
                emit(&mut stdout, &WorkerResponse::Ack)?;
                break;
            }
            WorkerRequest::Run(request) => match run_request(&runtime, &mut repl, request) {
                Ok(result) => emit(&mut stdout, &WorkerResponse::RunResult(result))?,
                Err(err) => emit(&mut stdout, &WorkerResponse::Error { message: err })?,
            },
        }
    }
    Ok(())
}

fn run_request(
    runtime: &tokio::runtime::Runtime,
    repl: &mut RlmRepl,
    request: SandboxRunRequest,
) -> Result<SandboxRunResult, String> {
    let query = if request.query.is_empty() {
        DEFAULT_QUERY.to_owned()
    } else {
        request.query
    };

    if request.initialize {
        let context = context_from_value(request.context);
        if let Some(code) = request.code {
            runtime
                .block_on(repl.setup_context(context, Some(&query)))
                .map_err(|err| err.to_string())?;
            let result = runtime
                .block_on(repl.execute_code(&code))
                .map_err(|err| err.to_string())?;
            return Ok(SandboxRunResult {
                response: None,
                stdout: Some(result.stdout),
                stderr: Some(result.stderr),
            });
        }
        let response = runtime
            .block_on(repl.completion(context, Some(&query)))
            .map_err(|err| err.to_string())?;
        return Ok(SandboxRunResult {
            response: Some(response),
            stdout: None,
            stderr: None,
        });
    }

    if let Some(code) = request.code {
        let result = runtime
            .block_on(repl.execute_code(&code))
            .map_err(|err| err.to_string())?;
        return Ok(SandboxRunResult {
            response: None,
            stdout: Some(result.stdout),
            stderr: Some(result.stderr),
        });
    }

    let response = runtime
        .block_on(repl.completion_with_existing(Some(&query)))
        .map_err(|err| err.to_string())?;
    Ok(SandboxRunResult {
        response: Some(response),
        stdout: None,
        stderr: None,
    })
}

fn worker_config_from_env() -> Result<RlmConfig, String> {
    let api_key = env::var("OPENAI_API_KEY")
        .map_err(|_| "OPENAI_API_KEY is required for sandbox worker".to_owned())?;
    Ok(RlmConfig {
        api_key: Some(api_key),
        base_url: "https://api.openai.com/v1".to_owned(),
        model: "gpt-5".to_owned(),
        recursive_model: "gpt-5-mini".to_owned(),
        max_iterations: 20,
        depth: 1,
        enable_logging: false,
        disable_recursive: false,
    })
}

fn emit(stdout: &mut impl Write, response: &WorkerResponse) -> Result<(), String> {
    let payload = serde_json::to_string(response).map_err(|err| err.to_string())?;
    stdout
        .write_all(payload.as_bytes())
        .map_err(|err| format!("stdout write failed: {err}"))?;
    stdout
        .write_all(b"\n")
        .map_err(|err| format!("stdout write failed: {err}"))?;
    stdout
        .flush()
        .map_err(|err| format!("stdout flush failed: {err}"))
}
