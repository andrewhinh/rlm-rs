use std::env;
use std::io::{self, BufRead, Write};

use app::env as app_env;
use app::protocol::{SandboxRunRequest, SandboxRunResult, WorkerRequest, WorkerResponse};
use rlm::lambda_rlm::LambdaOptions;
use rlm::prompts::DEFAULT_QUERY;
use rlm::rlm::{RlmConfig, RlmMethod, RlmRepl};
use rlm::utils::context_from_value;

#[cfg(feature = "mimalloc")]
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build()?;

    let stdin = io::stdin();
    let mut stdout = io::stdout();
    let worker_config = worker_config_from_env();
    let (mut repl, mut startup_error) = match worker_config
        .clone()
        .and_then(|config| RlmRepl::new(config).map_err(|err| err.to_string()))
    {
        Ok(repl) => (Some(repl), None),
        Err(err) => (None, Some(format!("sandbox worker init failed: {err}"))),
    };
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
            WorkerRequest::Ping => {
                if let Some(message) = startup_error.as_ref() {
                    emit(
                        &mut stdout,
                        &WorkerResponse::Error {
                            message: message.clone(),
                        },
                    )?;
                } else {
                    emit(&mut stdout, &WorkerResponse::Pong)?;
                }
            }
            WorkerRequest::Reset => {
                if let Some(repl) = repl.as_mut() {
                    repl.reset();
                    startup_error = None;
                    emit(&mut stdout, &WorkerResponse::Ack)?;
                    continue;
                }

                match worker_config
                    .clone()
                    .and_then(|config| RlmRepl::new(config).map_err(|err| err.to_string()))
                {
                    Ok(next_repl) => {
                        repl = Some(next_repl);
                        startup_error = None;
                        emit(&mut stdout, &WorkerResponse::Ack)?;
                    }
                    Err(err) => {
                        let message = format!("sandbox worker reset failed: {err}");
                        repl = None;
                        startup_error = Some(message.clone());
                        emit(&mut stdout, &WorkerResponse::Error { message })?;
                    }
                }
            }
            WorkerRequest::Shutdown => {
                emit(&mut stdout, &WorkerResponse::Ack)?;
                break;
            }
            WorkerRequest::Run(request) => {
                if let Some(message) = startup_error.as_ref() {
                    emit(
                        &mut stdout,
                        &WorkerResponse::Error {
                            message: message.clone(),
                        },
                    )?;
                } else {
                    match run_request(
                        &runtime,
                        repl.as_mut().expect("repl initialized at startup"),
                        request,
                    ) {
                        Ok(result) => emit(&mut stdout, &WorkerResponse::RunResult(result))?,
                        Err(err) => emit(&mut stdout, &WorkerResponse::Error { message: err })?,
                    }
                }
            }
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
    let lambda_defaults = LambdaOptions::default();
    Ok(RlmConfig {
        method: app_env::method("RLM_METHOD", RlmMethod::Rlm)?,
        api_key: Some(api_key),
        base_url: app_env::string("RLM_BASE_URL", "https://api.openai.com/v1"),
        model: app_env::string("RLM_MODEL", "gpt-5"),
        recursive_model: app_env::string("RLM_RECURSIVE_MODEL", "gpt-5-nano"),
        lambda_options: app_env::lambda_options(&lambda_defaults)?,
        max_iterations: app_env::usize("RLM_MAX_ITERATIONS", 10)?,
        depth: app_env::usize("RLM_DEPTH", 0)?,
        enable_logging: app_env::bool("RLM_ENABLE_LOGGING", false)?,
        disable_recursive: app_env::bool("RLM_DISABLE_RECURSIVE", false)?,
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
