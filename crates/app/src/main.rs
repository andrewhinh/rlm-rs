use std::collections::HashMap;
use std::env;
use std::sync::Arc;

use axum::Json;
use axum::Router;
use axum::extract::State;
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use rlm::llm::Message;
use rlm::prompts::DEFAULT_QUERY;
use rlm::rlm::{RlmConfig, RlmRepl};
use rlm::utils::ContextInput;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::sync::mpsc;
use std::sync::mpsc::Sender;
use tokio::sync::oneshot;
use uuid::Uuid;

#[derive(Clone)]
struct AppConfig {
    api_key: String,
    base_url: String,
    model: String,
    recursive_model: String,
    max_iterations: usize,
    depth: usize,
    enable_logging: bool,
    disable_recursive: bool,
}

impl AppConfig {
    fn to_rlm_config(&self) -> RlmConfig {
        RlmConfig {
            api_key: Some(self.api_key.clone()),
            base_url: self.base_url.clone(),
            model: self.model.clone(),
            recursive_model: self.recursive_model.clone(),
            max_iterations: self.max_iterations,
            depth: self.depth,
            enable_logging: self.enable_logging,
            disable_recursive: self.disable_recursive,
        }
    }
}

#[derive(Clone)]
struct AppState {
    sender: Arc<std::sync::Mutex<Sender<SessionRequest>>>,
}

#[derive(Debug, Deserialize)]
struct ReplRequest {
    context: Option<Value>,
    query: Option<String>,
    reset: Option<bool>,
    code: Option<String>,
}

#[derive(Debug, Serialize)]
struct ReplResponse {
    session_id: String,
    response: Option<String>,
    stdout: Option<String>,
    stderr: Option<String>,
}

struct SessionRequest {
    session_id: String,
    reset: bool,
    query: String,
    context: ContextInput,
    code: Option<String>,
    respond_to: oneshot::Sender<Result<ReplResponse, String>>,
}

async fn healthcheck() -> StatusCode {
    StatusCode::OK
}

async fn repl_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(payload): Json<ReplRequest>,
) -> Result<Response, (StatusCode, String)> {
    let mut is_new_session = false;
    let session_id = if let Some(cookie) = extract_cookie_value(&headers, "rlm_session") {
        cookie
    } else {
        is_new_session = true;
        Uuid::new_v4().to_string()
    };

    let reset = payload.reset.unwrap_or(false);
    let context_input = context_from_value(payload.context);
    let query = payload.query.as_deref().unwrap_or(DEFAULT_QUERY).to_owned();
    let code = payload.code;
    let (respond_to, response) = oneshot::channel();
    let sender = state.sender.clone();
    drop(state);
    sender
        .lock()
        .map_err(internal_error)?
        .send(SessionRequest {
            session_id: session_id.clone(),
            reset,
            query,
            context: context_input,
            code,
            respond_to,
        })
        .map_err(internal_error)?;

    let response = response
        .await
        .map_err(internal_error)?
        .map_err(internal_error)?;

    let mut response = Json(response).into_response();
    if is_new_session {
        let cookie_value = format!("rlm_session={session_id}; Path=/; HttpOnly; SameSite=Lax");
        let header_value = HeaderValue::from_str(&cookie_value).map_err(internal_error)?;
        response
            .headers_mut()
            .insert(header::SET_COOKIE, header_value);
    }
    Ok(response)
}

fn context_from_value(value: Option<Value>) -> ContextInput {
    match value {
        None => ContextInput::Text(String::new()),
        Some(Value::String(text)) => ContextInput::Text(text),
        Some(Value::Array(items)) => {
            if let Some(strings) = array_to_strings(&items) {
                return ContextInput::Strings(strings);
            }
            if let Some(messages) = array_to_messages(&items) {
                return ContextInput::Messages(messages);
            }
            ContextInput::Json(Value::Array(items))
        }
        Some(other) => ContextInput::Json(other),
    }
}

fn array_to_strings(items: &[Value]) -> Option<Vec<String>> {
    let mut strings = Vec::with_capacity(items.len());
    for item in items {
        match item {
            Value::String(text) => strings.push(text.clone()),
            _ => return None,
        }
    }
    Some(strings)
}

fn array_to_messages(items: &[Value]) -> Option<Vec<Message>> {
    let mut messages = Vec::with_capacity(items.len());
    for item in items {
        let map = match item {
            Value::Object(map) => map,
            _ => return None,
        };
        let content_value = map.get("content")?;
        let content = match content_value {
            Value::String(text) => text.clone(),
            other => other.to_string(),
        };
        let role = map
            .get("role")
            .and_then(|value| value.as_str())
            .unwrap_or("user")
            .to_owned();
        messages.push(Message { role, content });
    }
    Some(messages)
}

fn internal_error<E: std::fmt::Display>(err: E) -> (StatusCode, String) {
    (StatusCode::INTERNAL_SERVER_ERROR, err.to_string())
}

fn extract_cookie_value(headers: &HeaderMap, name: &str) -> Option<String> {
    let header_value = headers.get(header::COOKIE)?;
    let cookie_str = header_value.to_str().ok()?;
    cookie_str.split(';').find_map(|pair| {
        let mut parts = pair.trim().splitn(2, '=');
        let key = parts.next()?.trim();
        let value = parts.next()?.trim();
        if key == name {
            Some(value.to_owned())
        } else {
            None
        }
    })
}

fn spawn_session_worker(config: AppConfig) -> Arc<std::sync::Mutex<Sender<SessionRequest>>> {
    let (sender, receiver) = mpsc::channel::<SessionRequest>();
    std::thread::spawn(move || {
        let mut sessions: HashMap<String, RlmRepl> = HashMap::new();
        while let Ok(req) = receiver.recv() {
            let SessionRequest {
                session_id,
                reset,
                query,
                context,
                code,
                respond_to,
            } = req;
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                handle_session_request_inner(
                    &config,
                    &mut sessions,
                    session_id,
                    reset,
                    query,
                    context,
                    code,
                )
            }));
            let result = match result {
                Ok(r) => r,
                Err(panic) => {
                    let msg = if let Some(s) = panic.downcast_ref::<&str>() {
                        format!("session worker panicked: {s}")
                    } else if let Some(s) = panic.downcast_ref::<String>() {
                        format!("session worker panicked: {s}")
                    } else {
                        "session worker panicked (unknown payload)".to_owned()
                    };
                    eprintln!("{msg}");
                    Err(msg)
                }
            };
            let _ = respond_to.send(result);
        }
    });
    Arc::new(std::sync::Mutex::new(sender))
}

fn handle_session_request_inner(
    config: &AppConfig,
    sessions: &mut HashMap<String, RlmRepl>,
    session_id: String,
    reset: bool,
    query: String,
    context: ContextInput,
    code: Option<String>,
) -> Result<ReplResponse, String> {
    if reset {
        sessions.remove(&session_id);
    }
    let is_new_session = !sessions.contains_key(&session_id);
    if is_new_session {
        let repl = RlmRepl::new(config.to_rlm_config()).map_err(|err| err.to_string())?;
        sessions.insert(session_id.clone(), repl);
    }
    let repl = sessions
        .get_mut(&session_id)
        .ok_or_else(|| "session init failed".to_owned())?;

    if let Some(code) = code {
        if is_new_session || reset {
            repl.setup_context(context, Some(&query))
                .map_err(|err| err.to_string())?;
        }
        let result = repl.execute_code(&code).map_err(|err| err.to_string())?;
        return Ok(ReplResponse {
            session_id,
            response: None,
            stdout: Some(result.stdout),
            stderr: Some(result.stderr),
        });
    }

    let response = if is_new_session || reset {
        repl.completion(context, Some(&query))
            .map_err(|err| err.to_string())?
    } else {
        repl.completion_with_existing(Some(&query))
            .map_err(|err| err.to_string())?
    };
    Ok(ReplResponse {
        session_id,
        response: Some(response),
        stdout: None,
        stderr: None,
    })
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    dotenvy::dotenv().ok();
    let api_key = env::var("API_KEY").map_err(|_| "API_KEY is required for the RLM server")?;
    let config = AppConfig {
        api_key,
        base_url: env::var("BASE_URL").unwrap_or_else(|_| "https://api.openai.com/v1".to_owned()),
        model: "gpt-5".to_owned(),
        recursive_model: "gpt-5-mini".to_owned(),
        max_iterations: 20,
        depth: 1,
        enable_logging: false,
        disable_recursive: false,
    };

    // spawn session worker before tokio runtime so RustPython remains single-threaded (gVisor issue)
    let sender = spawn_session_worker(config);
    let state = AppState { sender };

    let host = "0.0.0.0".to_string();
    let port = 3000;
    let addr = format!("{host}:{port}");

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build()?;
    rt.block_on(async move {
        let app = Router::new()
            .route("/healthz", get(healthcheck))
            .route("/repl", post(repl_handler))
            .with_state(state);

        let listener = tokio::net::TcpListener::bind(&addr).await?;
        println!("listening on {addr}");
        axum::serve(listener, app).await?;
        Ok::<(), Box<dyn std::error::Error>>(())
    })?;
    Ok(())
}
