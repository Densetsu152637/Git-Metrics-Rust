//! HTTP and WebSocket transport.
//!
//! One handler serves every path: WebSocket upgrade requests go to the
//! WebSocket flow and everything else to the HTTP flow. Authorization is
//! checked before the method is considered (except `GET /health`).

use std::future::Future;
use std::sync::Arc;

use axum::body::Body;
use axum::extract::ws::{close_code, CloseFrame, Message, WebSocket, WebSocketUpgrade};
use axum::extract::{FromRequestParts, Request, State};
use axum::http::header::{CONNECTION, CONTENT_TYPE, UPGRADE};
use axum::http::{HeaderMap, Method, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Router;
use futures_util::{SinkExt, StreamExt};
use serde::Serialize;
use serde_json::{json, Value};
use tokio::sync::{mpsc, Mutex};

use crate::process::RepositoryService;
use crate::security::headers_authorized;
use crate::text::trim;
use crate::threading::{safe_print, PROGRESS_CAPACITY};
use crate::types::RepositoryData;

/// Largest accepted request body or WebSocket message, in bytes.
pub const MAX_REQUEST_BYTES: usize = 8192;

const API_KEY_HEADER: &str = "x-git-api-key";
const INVALID_JSON: &str = "Invalid JSON format. Expected: {\"url\": \"...\"}";
const TOO_LARGE: &str = "Request body is too large.";

/// The analysis the transport layer delegates to.
pub trait Analyzer: Send + Sync + 'static {
    fn analyze(
        &self,
        url: String,
        auth_token: Option<String>,
        progress: mpsc::Sender<String>,
    ) -> impl Future<Output = Result<Arc<RepositoryData>, String>> + Send;
}

impl Analyzer for RepositoryService {
    fn analyze(
        &self,
        url: String,
        auth_token: Option<String>,
        progress: mpsc::Sender<String>,
    ) -> impl Future<Output = Result<Arc<RepositoryData>, String>> + Send {
        RepositoryService::analyze(self, url, auth_token, progress)
    }
}

/// Builds the application: every route is handled by the same dispatcher.
pub fn router<S: Analyzer>(service: Arc<S>) -> Router {
    Router::new().fallback(dispatch::<S>).with_state(service)
}

// ---------------------------------------------------------------------------
// Messages
// ---------------------------------------------------------------------------

#[derive(Debug, PartialEq, Eq)]
struct ClientMessage {
    url: String,
    auth_token: Option<String>,
}

/// Requires a JSON object with a string `url`; `auth_token` may be absent,
/// null, or a string. Other fields are ignored and the last duplicate key wins.
fn parse_client_message(bytes: &[u8]) -> Option<ClientMessage> {
    let value: Value = serde_json::from_slice(bytes).ok()?;
    let object = value.as_object()?;
    let url = object.get("url")?.as_str()?.to_owned();
    let auth_token = match object.get("auth_token") {
        None | Some(Value::Null) => None,
        Some(Value::String(token)) => Some(token.clone()),
        Some(_) => return None,
    };
    Some(ClientMessage { url, auth_token })
}

fn encode_value<T: Serialize>(label: &str, data: &T) -> String {
    let data = serde_json::to_value(data).unwrap_or(Value::Null);
    json!({ "type": label, "data": data }).to_string()
}

fn encode_error(message: &str) -> String {
    json!({ "type": "error", "message": message }).to_string()
}

fn authorized(headers: &HeaderMap) -> bool {
    headers_authorized(headers.get(API_KEY_HEADER).map(|value| value.as_bytes()))
}

// ---------------------------------------------------------------------------
// Dispatch
// ---------------------------------------------------------------------------

fn is_websocket_request(headers: &HeaderMap) -> bool {
    let upgrade = headers
        .get(UPGRADE)
        .is_some_and(|value| value.as_bytes().eq_ignore_ascii_case(b"websocket"));
    let connection = headers.get_all(CONNECTION).iter().any(|value| {
        value.to_str().is_ok_and(|text| {
            text.split(',')
                .any(|token| token.trim().eq_ignore_ascii_case("upgrade"))
        })
    });
    upgrade && connection
}

async fn dispatch<S: Analyzer>(State(service): State<Arc<S>>, request: Request) -> Response {
    if is_websocket_request(request.headers()) {
        websocket_entry(service, request).await
    } else {
        http_entry(service, request).await
    }
}

fn json_response(status: StatusCode, body: String) -> Response {
    (status, [(CONTENT_TYPE, "application/json")], body).into_response()
}

fn percent_decode(segment: &str) -> String {
    let bytes = segment.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        let escaped = (bytes[index] == b'%')
            .then(|| bytes.get(index + 1..index + 3))
            .flatten()
            .and_then(|pair| std::str::from_utf8(pair).ok())
            .and_then(|pair| u8::from_str_radix(pair, 16).ok());
        match escaped {
            Some(byte) => {
                decoded.push(byte);
                index += 3;
            }
            None => {
                decoded.push(bytes[index]);
                index += 1;
            }
        }
    }
    String::from_utf8_lossy(&decoded).into_owned()
}

fn is_health_path(path: &str) -> bool {
    let segments: Vec<String> = path
        .split('/')
        .filter(|segment| !segment.is_empty())
        .map(percent_decode)
        .collect();
    segments == ["health"]
}

// ---------------------------------------------------------------------------
// HTTP
// ---------------------------------------------------------------------------

/// Reads the body, failing as soon as it exceeds [`MAX_REQUEST_BYTES`].
async fn read_limited_body(body: Body) -> Result<Vec<u8>, &'static str> {
    let mut stream = body.into_data_stream();
    let mut collected = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|_| "Request body could not be read.")?;
        if collected.len() + chunk.len() > MAX_REQUEST_BYTES {
            return Err(TOO_LARGE);
        }
        collected.extend_from_slice(&chunk);
    }
    Ok(collected)
}

async fn http_entry<S: Analyzer>(service: Arc<S>, request: Request) -> Response {
    let (parts, body) = request.into_parts();
    if parts.method == Method::GET && is_health_path(parts.uri.path()) {
        return json_response(StatusCode::OK, "{\"status\":\"ok\"}".to_owned());
    }
    if !authorized(&parts.headers) {
        return json_response(StatusCode::UNAUTHORIZED, encode_error("Unauthorized."));
    }
    if parts.method != Method::POST {
        return (
            StatusCode::METHOD_NOT_ALLOWED,
            [(CONTENT_TYPE, "text/plain")],
            "Method Not Allowed",
        )
            .into_response();
    }

    let bytes = match read_limited_body(body).await {
        Ok(bytes) => bytes,
        Err(message) => return json_response(StatusCode::BAD_REQUEST, encode_error(message)),
    };
    let Some(message) = parse_client_message(&bytes) else {
        return json_response(StatusCode::BAD_REQUEST, encode_error(INVALID_JSON));
    };

    safe_print("Processing authenticated repository request (HTTP).");
    // Progress is discarded, but the receiver stays alive for the whole request.
    let (progress, _updates) = mpsc::channel(PROGRESS_CAPACITY);
    match service
        .analyze(trim(&message.url).to_owned(), message.auth_token, progress)
        .await
    {
        Ok(data) => json_response(StatusCode::OK, encode_value("value", &*data)),
        Err(error) => json_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            encode_error(&format!("err: {error}")),
        ),
    }
}

// ---------------------------------------------------------------------------
// WebSocket
// ---------------------------------------------------------------------------

async fn websocket_entry<S: Analyzer>(service: Arc<S>, request: Request) -> Response {
    if !authorized(request.headers()) {
        return (StatusCode::BAD_REQUEST, "Unauthorized").into_response();
    }
    let (mut parts, _body) = request.into_parts();
    match WebSocketUpgrade::from_request_parts(&mut parts, &()).await {
        Ok(upgrade) => upgrade.on_upgrade(move |socket| handle_socket(service, socket)),
        Err(rejection) => rejection.into_response(),
    }
}

type Sink = Arc<Mutex<futures_util::stream::SplitSink<WebSocket, Message>>>;

async fn send_text(sink: &Sink, text: String) {
    let _ = sink.lock().await.send(Message::Text(text.into())).await;
}

async fn send_close(sink: &Sink, reason: &'static str) {
    let frame = CloseFrame {
        code: close_code::NORMAL,
        reason: reason.into(),
    };
    let _ = sink.lock().await.send(Message::Close(Some(frame))).await;
}

async fn handle_socket<S: Analyzer>(service: Arc<S>, socket: WebSocket) {
    let (sink, mut stream) = socket.split();
    let sink: Sink = Arc::new(Mutex::new(sink));

    // A single text or binary message carries the request.
    let payload: Vec<u8> = loop {
        match stream.next().await {
            Some(Ok(Message::Text(text))) => break text.as_bytes().to_vec(),
            Some(Ok(Message::Binary(bytes))) => break bytes.to_vec(),
            Some(Ok(Message::Ping(_) | Message::Pong(_))) => continue,
            _ => return,
        }
    };

    if payload.len() > MAX_REQUEST_BYTES {
        send_text(&sink, encode_error(TOO_LARGE)).await;
        send_close(&sink, "Bad input").await;
        return;
    }
    let Some(message) = parse_client_message(&payload) else {
        send_text(&sink, encode_error(INVALID_JSON)).await;
        send_close(&sink, "Bad input").await;
        return;
    };

    safe_print("Processing authenticated repository request (WS).");
    let (progress, mut updates) = mpsc::channel::<String>(PROGRESS_CAPACITY);
    let forwarder = {
        let sink = Arc::clone(&sink);
        tokio::spawn(async move {
            while let Some(update) = updates.recv().await {
                send_text(&sink, encode_value("text_update", &update)).await;
            }
        })
    };

    let result = service
        .analyze(trim(&message.url).to_owned(), message.auth_token, progress)
        .await;
    // Remaining queued updates are dropped; a slow client never delays the result.
    forwarder.abort();
    let _ = forwarder.await;

    match result {
        Ok(data) => send_text(&sink, encode_value("value", &*data)).await,
        Err(error) => {
            send_text(&sink, encode_value("text_update", &error)).await;
            send_text(&sink, encode_error(&format!("err: {error}"))).await;
        }
    }
    send_close(&sink, "Done").await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_messages_follow_the_source_schema() {
        let parse = |text: &str| parse_client_message(text.as_bytes());
        let ok = |url: &str, token: Option<&str>| {
            Some(ClientMessage {
                url: url.to_owned(),
                auth_token: token.map(str::to_owned),
            })
        };
        assert_eq!(parse(r#"{"url":"u"}"#), ok("u", None));
        assert_eq!(parse(r#"{"url":"u","auth_token":null}"#), ok("u", None));
        assert_eq!(
            parse(r#"{"url":"u","auth_token":"t","extra":[1]}"#),
            ok("u", Some("t"))
        );
        assert_eq!(parse(r#"{"url":"a","url":"b"}"#), ok("b", None));
        assert_eq!(parse(" {\"url\":\"u\"} \n"), ok("u", None));
        assert_eq!(parse(r#"{"url":1}"#), None);
        assert_eq!(parse(r#"{"url":"u","auth_token":5}"#), None);
        assert_eq!(parse(r#"{"auth_token":"t"}"#), None);
        assert_eq!(parse(r#"["url"]"#), None);
        assert_eq!(parse(r#"{"url":"u"} trailing"#), None);
        assert_eq!(parse(""), None);
    }

    #[test]
    fn envelopes_have_the_documented_shape() {
        let error: Value = serde_json::from_str(&encode_error("boom")).unwrap();
        assert_eq!(error, json!({"type": "error", "message": "boom"}));
        let update: Value = serde_json::from_str(&encode_value("text_update", &"hi")).unwrap();
        assert_eq!(update, json!({"type": "text_update", "data": "hi"}));
    }

    #[test]
    fn health_path_matches_like_wai_path_info() {
        assert!(is_health_path("/health"));
        assert!(is_health_path("/health/"));
        assert!(is_health_path("//health"));
        assert!(is_health_path("/%68ealth"));
        assert!(!is_health_path("/health/x"));
        assert!(!is_health_path("/"));
        assert!(!is_health_path("/healthz"));
    }

    #[test]
    fn websocket_detection_requires_upgrade_and_connection_tokens() {
        let mut headers = HeaderMap::new();
        assert!(!is_websocket_request(&headers));
        headers.insert(UPGRADE, "WebSocket".parse().unwrap());
        assert!(!is_websocket_request(&headers));
        headers.insert(CONNECTION, "keep-alive, Upgrade".parse().unwrap());
        assert!(is_websocket_request(&headers));
        headers.insert(UPGRADE, "h2c".parse().unwrap());
        assert!(!is_websocket_request(&headers));
    }
}
