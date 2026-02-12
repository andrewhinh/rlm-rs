use std::borrow::Cow;
use std::collections::{HashMap, VecDeque};
use std::env;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use app::launcher::build_launcher;
use app::pool::SandboxPool;
use app::protocol::SandboxRunRequest;
use app::{SandboxHandle, SandboxLaunchConfig, SandboxWorkerConfig};
use axum::Json;
use axum::Router;
use axum::extract::{DefaultBodyLimit, Request, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use rlm::prompts::DEFAULT_QUERY;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::{mpsc, oneshot};
use tower::limit::ConcurrencyLimitLayer;
use uuid::Uuid;

#[derive(Clone)]
struct AppConfig {
    api_key: String,
    model: String,
    max_sessions: usize,
    max_inflight: usize,
    sandbox_pool_size: usize,
}

const DEFAULT_MAX_SESSIONS: usize = 128;
const DEFAULT_MAX_INFLIGHT: usize = 32;
const DEFAULT_SANDBOX_POOL_SIZE: usize = 4;
const MAX_SESSION_ID_LEN: usize = 64;
const OPENAI_MAX_INPUT_STRING_BYTES: usize = 10_485_760;
const MAX_LLM_BODY_LIMIT_BYTES: usize = 11 * 1024 * 1024;

impl AppConfig {
    fn to_worker_config(&self) -> SandboxWorkerConfig {
        SandboxWorkerConfig {
            api_key: self.api_key.clone(),
        }
    }

    fn to_launch_config(&self) -> SandboxLaunchConfig {
        SandboxLaunchConfig {
            worker: self.to_worker_config(),
        }
    }
}

#[derive(Clone)]
struct AppState {
    sender: mpsc::UnboundedSender<SessionRequest>,
    config: AppConfig,
}

#[derive(Debug, Serialize)]
struct ReplResponse {
    session_id: String,
    response: Option<String>,
    stdout: Option<String>,
    stderr: Option<String>,
}

#[derive(Debug, Deserialize)]
struct OpenAiChatCompletionsRequest {
    messages: Vec<OpenAiChatMessage>,
    model: Option<String>,
    stream: Option<bool>,
    max_tokens: Option<u32>,
    max_completion_tokens: Option<u32>,
    reset: Option<bool>,
}

#[derive(Debug, Deserialize)]
struct OpenAiChatMessage {
    role: String,
    content: Value,
}

#[derive(Debug, Serialize)]
struct OpenAiChatCompletionsResponse {
    id: String,
    object: String,
    created: u64,
    model: String,
    choices: Vec<OpenAiChatChoice>,
    usage: OpenAiUsage,
}

#[derive(Debug, Serialize)]
struct OpenAiChatChoice {
    index: usize,
    message: OpenAiAssistantMessage,
    finish_reason: String,
}

#[derive(Debug, Serialize)]
struct OpenAiAssistantMessage {
    role: String,
    content: String,
}

#[derive(Debug, Serialize)]
struct OpenAiUsage {
    prompt_tokens: usize,
    completion_tokens: usize,
    total_tokens: usize,
}

struct SessionRequest {
    session_id: String,
    reset: bool,
    query: String,
    context: Option<Value>,
    code: Option<String>,
    respond_to: oneshot::Sender<Result<ReplResponse, String>>,
}

struct SessionTask {
    session_id: String,
    reset: bool,
    query: String,
    context: Option<Value>,
    code: Option<String>,
}

struct SessionSandbox {
    handle: Box<dyn SandboxHandle>,
    initialized: bool,
}

async fn healthcheck() -> StatusCode {
    StatusCode::OK
}

async fn log_request_response(request: Request, next: Next) -> Response {
    let method = request.method().clone();
    let uri = request.uri().clone();
    let start = Instant::now();
    println!("request: {method} {uri}");
    let response = next.run(request).await;
    println!(
        "response: {method} {uri} status={} latency_ms={}",
        response.status(),
        start.elapsed().as_millis()
    );
    response
}

async fn openai_chat_completions_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(payload): Json<OpenAiChatCompletionsRequest>,
) -> Result<Response, (StatusCode, String)> {
    let OpenAiChatCompletionsRequest {
        messages,
        model,
        stream,
        max_tokens,
        max_completion_tokens,
        reset,
    } = payload;

    if stream.unwrap_or(false) {
        return Err((
            StatusCode::BAD_REQUEST,
            "stream=true unsupported; use stream=false".to_owned(),
        ));
    }
    if messages.is_empty() {
        return Err((StatusCode::BAD_REQUEST, "messages required".to_owned()));
    }
    validate_openai_input(&messages)?;

    let model = model.unwrap_or_else(|| state.config.model.clone());
    if model != state.config.model {
        return Err((
            StatusCode::BAD_REQUEST,
            format!(
                "model override unsupported; expected {}",
                state.config.model
            ),
        ));
    }
    let _ = max_completion_tokens.or(max_tokens);

    let session_id =
        session_id_from_transport(&headers)?.unwrap_or_else(|| Uuid::new_v4().to_string());
    let reset = reset.unwrap_or(false) || header_bool(&headers, "x-rlm-reset")?;
    let query = openai_query_from_messages(&messages);
    let context = Some(openai_context_from_messages(messages));

    let (respond_to, response) = oneshot::channel();
    state
        .sender
        .send(SessionRequest {
            session_id: session_id.clone(),
            reset,
            query,
            context,
            code: None,
            respond_to,
        })
        .map_err(internal_error)?;
    let response = response
        .await
        .map_err(internal_error)?
        .map_err(internal_error)?;
    let content = response
        .response
        .ok_or_else(|| internal_error("missing assistant response"))?;

    let created = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(internal_error)?
        .as_secs();
    let body = OpenAiChatCompletionsResponse {
        id: format!("chatcmpl-{}", Uuid::new_v4().simple()),
        object: "chat.completion".to_owned(),
        created,
        model,
        choices: vec![OpenAiChatChoice {
            index: 0,
            message: OpenAiAssistantMessage {
                role: "assistant".to_owned(),
                content,
            },
            finish_reason: "stop".to_owned(),
        }],
        usage: OpenAiUsage {
            prompt_tokens: 0,
            completion_tokens: 0,
            total_tokens: 0,
        },
    };

    let mut response = Json(body).into_response();
    set_session_response_headers(&mut response, &session_id)?;
    Ok(response)
}

fn internal_error<E: std::fmt::Display>(err: E) -> (StatusCode, String) {
    (StatusCode::INTERNAL_SERVER_ERROR, err.to_string())
}

fn validate_openai_input(messages: &[OpenAiChatMessage]) -> Result<(), (StatusCode, String)> {
    for (idx, message) in messages.iter().enumerate() {
        if message.role.trim().is_empty() {
            return Err((
                StatusCode::BAD_REQUEST,
                format!("messages[{idx}].role required"),
            ));
        }
        let content_len = openai_message_text(message).len();
        if content_len > OPENAI_MAX_INPUT_STRING_BYTES {
            return Err((
                StatusCode::PAYLOAD_TOO_LARGE,
                format!(
                    "messages[{idx}].content too large; max {} bytes",
                    OPENAI_MAX_INPUT_STRING_BYTES
                ),
            ));
        }
    }
    Ok(())
}

fn extract_cookie_value(headers: &HeaderMap, name: &str) -> Option<String> {
    for header_value in headers.get_all(header::COOKIE).iter() {
        let cookie_str = match header_value.to_str() {
            Ok(value) => value,
            Err(_) => continue,
        };
        for pair in cookie_str.split(';') {
            let mut parts = pair.trim().splitn(2, '=');
            let key = parts.next()?.trim();
            let value = parts.next().unwrap_or("").trim();
            if key == name && !value.is_empty() {
                return Some(value.to_owned());
            }
        }
    }
    None
}

fn validate_session_id(value: &str) -> Option<String> {
    let mut value = value.trim();
    value = value.trim_matches('"');
    value = value.trim_matches('\'');
    if value.is_empty() || value.len() > MAX_SESSION_ID_LEN || !value.is_ascii() {
        return None;
    }
    Uuid::parse_str(value).ok()?;
    Some(value.to_owned())
}

fn session_id_from_headers(headers: &HeaderMap) -> Option<String> {
    let value = extract_cookie_value(headers, "rlm_session")?;
    validate_session_id(&value)
}

fn session_id_from_transport(headers: &HeaderMap) -> Result<Option<String>, (StatusCode, String)> {
    if let Some(value) = headers.get("x-rlm-session-id") {
        let raw = value.to_str().map_err(internal_error)?;
        if let Some(validated) = validate_session_id(raw) {
            return Ok(Some(validated));
        }
        return Err((
            StatusCode::BAD_REQUEST,
            "invalid x-rlm-session-id header".to_owned(),
        ));
    }
    Ok(session_id_from_headers(headers))
}

fn set_session_response_headers(
    response: &mut Response,
    session_id: &str,
) -> Result<(), (StatusCode, String)> {
    let session_header = HeaderValue::from_str(session_id).map_err(internal_error)?;
    response
        .headers_mut()
        .insert("x-rlm-session-id", session_header);
    let cookie_value = format!("rlm_session={session_id}; Path=/; HttpOnly; SameSite=Lax");
    let header_value = HeaderValue::from_str(&cookie_value).map_err(internal_error)?;
    response
        .headers_mut()
        .insert(header::SET_COOKIE, header_value);
    Ok(())
}

fn header_bool(headers: &HeaderMap, name: &str) -> Result<bool, (StatusCode, String)> {
    let Some(value) = headers.get(name) else {
        return Ok(false);
    };
    let value = value.to_str().map_err(internal_error)?.trim();
    if value.eq_ignore_ascii_case("1")
        || value.eq_ignore_ascii_case("true")
        || value.eq_ignore_ascii_case("yes")
        || value.eq_ignore_ascii_case("on")
    {
        return Ok(true);
    }
    if value.eq_ignore_ascii_case("0")
        || value.eq_ignore_ascii_case("false")
        || value.eq_ignore_ascii_case("no")
        || value.eq_ignore_ascii_case("off")
    {
        return Ok(false);
    }
    Err((
        StatusCode::BAD_REQUEST,
        format!("invalid boolean header {name}"),
    ))
}

fn openai_message_text(message: &OpenAiChatMessage) -> Cow<'_, str> {
    match &message.content {
        Value::String(text) => Cow::Borrowed(text),
        Value::Null => Cow::Borrowed(""),
        other => Cow::Owned(other.to_string()),
    }
}

fn openai_query_from_messages(messages: &[OpenAiChatMessage]) -> String {
    for message in messages.iter().rev() {
        if message.role == "user" {
            let content = openai_message_text(message);
            if !content.is_empty() {
                return content.into_owned();
            }
        }
    }
    messages
        .last()
        .map(openai_message_text)
        .filter(|text| !text.is_empty())
        .map(Cow::into_owned)
        .unwrap_or_else(|| DEFAULT_QUERY.to_owned())
}

fn openai_context_from_messages(messages: Vec<OpenAiChatMessage>) -> Value {
    Value::Array(
        messages
            .into_iter()
            .map(|message| {
                let mut object = serde_json::Map::new();
                object.insert("role".to_owned(), Value::String(message.role));
                object.insert("content".to_owned(), message.content);
                Value::Object(object)
            })
            .collect(),
    )
}

fn touch_session(order: &mut VecDeque<String>, session_id: &str) {
    if let Some(pos) = order.iter().position(|id| id == session_id) {
        order.remove(pos);
    }
    order.push_back(session_id.to_owned());
}

fn remove_session(order: &mut VecDeque<String>, session_id: &str) {
    if let Some(pos) = order.iter().position(|id| id == session_id) {
        order.remove(pos);
    }
}

fn enforce_max_sessions(
    sessions: &mut HashMap<String, SessionSandbox>,
    order: &mut VecDeque<String>,
    max_sessions: usize,
) -> Vec<SessionSandbox> {
    let mut evicted_sessions = Vec::new();
    while order.len() > max_sessions {
        if let Some(evicted) = order.pop_front()
            && let Some(session) = sessions.remove(&evicted)
        {
            evicted_sessions.push(session);
        }
    }
    evicted_sessions
}

fn spawn_session_worker(
    config: AppConfig,
) -> Result<mpsc::UnboundedSender<SessionRequest>, Box<dyn std::error::Error>> {
    let launcher = build_launcher(config.to_launch_config());
    let mut pool = SandboxPool::new(launcher, config.sandbox_pool_size)
        .map_err(|err| format!("failed to initialize sandbox pool: {err}"))?;
    let (sender, mut receiver) = mpsc::unbounded_channel::<SessionRequest>();
    std::thread::spawn(move || {
        let mut sessions: HashMap<String, SessionSandbox> = HashMap::new();
        let mut session_order: VecDeque<String> = VecDeque::new();
        while let Some(req) = receiver.blocking_recv() {
            let SessionRequest {
                session_id,
                reset,
                query,
                context,
                code,
                respond_to,
            } = req;
            let task = SessionTask {
                session_id,
                reset,
                query,
                context,
                code,
            };
            let result = handle_session_request_inner(
                &config,
                &mut pool,
                &mut sessions,
                &mut session_order,
                task,
            );
            let _ = respond_to.send(result);
        }
        for (_, session) in sessions.drain() {
            pool.retire(session.handle);
        }
    });
    Ok(sender)
}

fn handle_session_request_inner(
    config: &AppConfig,
    pool: &mut SandboxPool,
    sessions: &mut HashMap<String, SessionSandbox>,
    session_order: &mut VecDeque<String>,
    task: SessionTask,
) -> Result<ReplResponse, String> {
    let SessionTask {
        session_id,
        reset,
        query,
        context,
        code,
    } = task;
    if reset {
        if let Some(session) = sessions.remove(&session_id) {
            pool.retire(session.handle);
        }
        remove_session(session_order, &session_id);
    }
    let is_new_session = !sessions.contains_key(&session_id);
    if is_new_session {
        let handle = pool.acquire()?;
        sessions.insert(
            session_id.clone(),
            SessionSandbox {
                handle,
                initialized: false,
            },
        );
    }
    touch_session(session_order, &session_id);
    let evicted = enforce_max_sessions(sessions, session_order, config.max_sessions);
    for evicted_session in evicted {
        pool.retire(evicted_session.handle);
    }

    let run_result = {
        let session = sessions
            .get_mut(&session_id)
            .ok_or_else(|| "session init failed".to_owned())?;
        let initialize = !session.initialized;
        let request = SandboxRunRequest {
            initialize,
            query: query.clone(),
            context,
            code,
        };
        match session.handle.run(request) {
            Ok(result) => {
                if initialize {
                    session.initialized = true;
                }
                Ok(result)
            }
            Err(err) => Err(err),
        }
    };
    let run_result = match run_result {
        Ok(result) => result,
        Err(err) => {
            if let Some(session) = sessions.remove(&session_id) {
                pool.retire(session.handle);
            }
            remove_session(session_order, &session_id);
            return Err(err);
        }
    };

    Ok(ReplResponse {
        session_id,
        response: run_result.response,
        stdout: run_result.stdout,
        stderr: run_result.stderr,
    })
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    dotenvy::dotenv().ok();
    let api_key =
        env::var("OPENAI_API_KEY").map_err(|_| "OPENAI_API_KEY is required for the RLM server")?;
    let config = AppConfig {
        api_key,
        model: "gpt-5".to_owned(),
        max_sessions: DEFAULT_MAX_SESSIONS,
        max_inflight: DEFAULT_MAX_INFLIGHT,
        sandbox_pool_size: DEFAULT_SANDBOX_POOL_SIZE,
    };

    // spawn session worker before tokio runtime so RustPython remains single-threaded (gVisor issue)
    let sender = spawn_session_worker(config.clone())?;
    let state = AppState { sender, config };

    let host = "0.0.0.0";
    let port = 3000;
    let addr = format!("{host}:{port}");

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build()?;
    rt.block_on(async move {
        let app = Router::new()
            .route("/healthz", get(healthcheck))
            .route(
                "/v1/chat/completions",
                post(openai_chat_completions_handler)
                    .layer(DefaultBodyLimit::max(MAX_LLM_BODY_LIMIT_BYTES)),
            )
            .layer(ConcurrencyLimitLayer::new(state.config.max_inflight))
            .layer(middleware::from_fn(log_request_response))
            .with_state(state);

        let listener = tokio::net::TcpListener::bind(&addr).await?;
        println!("listening on {addr}");
        axum::serve(listener, app).await?;
        Ok::<(), Box<dyn std::error::Error>>(())
    })?;
    Ok(())
}
