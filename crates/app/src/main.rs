use std::borrow::Cow;
use std::env;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use app::launcher::build_launcher;
use app::session::{
    SessionConfig, SessionError, SessionErrorKind, SessionManagerHandle, SessionRequest,
    spawn_session_manager,
};
use app::{SandboxLaunchConfig, SandboxWorkerConfig};
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
use tokio::sync::oneshot;
use tower::ServiceBuilder;
use tower::limit::ConcurrencyLimitLayer;
use tower_http::compression::CompressionLayer;
use tower_http::timeout::TimeoutLayer;
use uuid::Uuid;

#[cfg(feature = "mimalloc")]
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

#[derive(Clone)]
struct AppConfig {
    api_key: String,
    model: String,
    max_sessions: usize,
    max_inflight: usize,
    ingress_capacity: usize,
    sandbox_pool_size: usize,
}

const DEFAULT_MAX_SESSIONS: usize = 256;
const DEFAULT_MAX_INFLIGHT: usize = 128;
const DEFAULT_INGRESS_CAPACITY: usize = 2048;
const DEFAULT_SANDBOX_POOL_SIZE: usize = 8;
const DEFAULT_REQUEST_TIMEOUT_SECONDS: u64 = 1800;

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
    sessions: SessionManagerHandle,
    config: AppConfig,
}

#[derive(Debug, Deserialize)]
struct OpenAiChatCompletionsRequest {
    #[serde(default)]
    messages: Vec<OpenAiChatMessage>,
    model: Option<String>,
    stream: Option<bool>,
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

#[derive(Debug, Serialize)]
struct OpenAiErrorEnvelope {
    error: OpenAiErrorBody,
}

#[derive(Debug, Serialize)]
struct OpenAiErrorBody {
    message: String,
    #[serde(rename = "type")]
    error_type: String,
    param: Option<String>,
}

async fn healthcheck() -> Response {
    let mut response = StatusCode::OK.into_response();
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
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
) -> Response {
    let OpenAiChatCompletionsRequest {
        messages,
        model,
        stream,
        reset,
    } = payload;
    if stream.unwrap_or(false) {
        return openai_error_response(
            StatusCode::BAD_REQUEST,
            "stream=true unsupported; use stream=false",
            "invalid_request_error",
        );
    }
    if messages.is_empty() {
        return openai_error_response(
            StatusCode::BAD_REQUEST,
            "messages required",
            "invalid_request_error",
        );
    }
    if let Err((status, message)) = validate_openai_input(&messages) {
        return openai_error_response(status, &message, "invalid_request_error");
    }

    let model = model.unwrap_or_else(|| state.config.model.clone());
    if model != state.config.model {
        return openai_error_response(
            StatusCode::BAD_REQUEST,
            &format!(
                "model override unsupported; expected {}",
                state.config.model
            ),
            "invalid_request_error",
        );
    }
    let session_id = match session_id_from_transport(&headers) {
        Ok(Some(session_id)) => session_id,
        Ok(None) => Uuid::new_v4().to_string(),
        Err((status, message)) => {
            return openai_error_response(status, &message, "invalid_request_error");
        }
    };
    let reset = match header_bool(&headers, "x-rlm-reset") {
        Ok(header_reset) => reset.unwrap_or(false) || header_reset,
        Err((status, message)) => {
            return openai_error_response(status, &message, "invalid_request_error");
        }
    };
    let (query, context) = (
        openai_query_from_messages(&messages),
        Some(openai_context_from_messages(messages)),
    );

    let (respond_to, response_rx) = oneshot::channel();
    if let Err(err) = state.sessions.try_dispatch(SessionRequest {
        session_id: session_id.clone(),
        reset,
        query,
        context,
        code: None,
        respond_to,
    }) {
        return session_error_response(err);
    }
    let response = match response_rx.await {
        Ok(Ok(response)) => response,
        Ok(Err(err)) => return session_error_response(err),
        Err(_) => {
            return openai_error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "session response channel closed",
                "server_error",
            );
        }
    };
    let content = match response.response {
        Some(content) => content,
        None => {
            return openai_error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "missing assistant response",
                "server_error",
            );
        }
    };

    let created = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs());
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
    if let Err((status, message)) = set_session_response_headers(&mut response, &session_id) {
        return openai_error_response(status, &message, "server_error");
    }
    response
}

fn internal_error<E: std::fmt::Display>(err: E) -> (StatusCode, String) {
    (StatusCode::INTERNAL_SERVER_ERROR, err.to_string())
}

fn session_error_response(err: SessionError) -> Response {
    match err.kind {
        SessionErrorKind::Overloaded => openai_error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            &err.message,
            "server_error",
        ),
        SessionErrorKind::Internal => openai_error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            &err.message,
            "server_error",
        ),
    }
}

fn openai_error_response(status: StatusCode, message: &str, error_type: &str) -> Response {
    let mut response = Json(OpenAiErrorEnvelope {
        error: OpenAiErrorBody {
            message: message.to_owned(),
            error_type: error_type.to_owned(),
            param: None,
        },
    })
    .into_response();
    *response.status_mut() = status;
    response
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

fn main() -> Result<(), Box<dyn std::error::Error>> {
    dotenvy::dotenv().ok();
    let api_key =
        env::var("OPENAI_API_KEY").map_err(|_| "OPENAI_API_KEY is required for the RLM server")?;
    let config = AppConfig {
        api_key,
        model: "gpt-5".to_owned(),
        max_sessions: DEFAULT_MAX_SESSIONS,
        max_inflight: DEFAULT_MAX_INFLIGHT,
        ingress_capacity: DEFAULT_INGRESS_CAPACITY,
        sandbox_pool_size: DEFAULT_SANDBOX_POOL_SIZE,
    };

    let launcher = build_launcher(config.to_launch_config());
    let sessions = spawn_session_manager(
        SessionConfig {
            max_sessions: config.max_sessions,
            ingress_capacity: config.ingress_capacity,
            sandbox_pool_size: config.sandbox_pool_size,
        },
        launcher,
    )
    .map_err(|err| format!("failed to initialize session manager: {err}"))?;
    let state = AppState { sessions, config };

    let host = "0.0.0.0";
    let port = 3000;
    let addr = format!("{host}:{port}");

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build()?;
    rt.block_on(async move {
        let chat_timeout = Duration::from_secs(DEFAULT_REQUEST_TIMEOUT_SECONDS);
        let app = Router::new()
            .route("/healthz", get(healthcheck))
            .route(
                "/v1/chat/completions",
                post(openai_chat_completions_handler).layer(
                    ServiceBuilder::new()
                        .layer(DefaultBodyLimit::max(MAX_LLM_BODY_LIMIT_BYTES))
                        .layer(TimeoutLayer::with_status_code(
                            StatusCode::REQUEST_TIMEOUT,
                            chat_timeout,
                        )),
                ),
            )
            .layer(CompressionLayer::new())
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
