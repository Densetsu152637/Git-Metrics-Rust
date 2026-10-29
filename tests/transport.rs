//! HTTP and WebSocket contract tests against a real loopback server with a
//! controlled analyzer, so no Git or network access is involved.

mod common;

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use common::EnvScope;
use futures_util::{SinkExt, StreamExt};
use git_api::api::{router, Analyzer, MAX_REQUEST_BYTES};
use git_api::types::RepositoryData;
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Message;

const INVALID_JSON: &str = "Invalid JSON format. Expected: {\"url\": \"...\"}";

#[derive(Default)]
struct Fake {
    seen: Mutex<Vec<(String, Option<String>)>>,
}

fn sample() -> RepositoryData {
    RepositoryData {
        repo_name: "sample".to_owned(),
        branches: vec![],
        all_commits: BTreeMap::new(),
        contributors: BTreeMap::new(),
    }
}

impl Analyzer for Fake {
    async fn analyze(
        &self,
        url: String,
        auth_token: Option<String>,
        progress: mpsc::Sender<String>,
    ) -> Result<Arc<RepositoryData>, String> {
        self.seen.lock().unwrap().push((url.clone(), auth_token));
        match url.as_str() {
            "fail" => {
                let _ = progress.try_send("about to fail".to_owned());
                tokio::time::sleep(Duration::from_millis(100)).await;
                Err("boom".to_owned())
            }
            "flood" => {
                // Nothing drains HTTP progress; overflow must be dropped, not awaited.
                for index in 0..5000 {
                    let _ = progress.try_send(format!("update {index}"));
                }
                Ok(Arc::new(sample()))
            }
            _ => {
                let _ = progress.try_send("p1".to_owned());
                let _ = progress.try_send("p2".to_owned());
                // Give the forwarder time to deliver before the result is sent.
                tokio::time::sleep(Duration::from_millis(150)).await;
                Ok(Arc::new(sample()))
            }
        }
    }
}

async fn start() -> (SocketAddr, Arc<Fake>) {
    let fake = Arc::new(Fake::default());
    let app = router(fake.clone());
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await });
    (address, fake)
}

struct Reply {
    status: u16,
    headers: Vec<(String, String)>,
    body: String,
}

impl Reply {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }

    fn json(&self) -> Value {
        serde_json::from_str(&self.body).unwrap_or_else(|error| panic!("{error}: {:?}", self.body))
    }
}

async fn http(
    address: SocketAddr,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: &[u8],
) -> Reply {
    let mut stream = TcpStream::connect(address).await.unwrap();
    let mut request = format!("{method} {path} HTTP/1.1\r\nHost: test\r\nConnection: close\r\n");
    for (name, value) in headers {
        request.push_str(&format!("{name}: {value}\r\n"));
    }
    if method == "POST" || !body.is_empty() {
        request.push_str(&format!("Content-Length: {}\r\n", body.len()));
    }
    request.push_str("\r\n");
    let mut bytes = request.into_bytes();
    bytes.extend_from_slice(body);
    // The server may answer and close before reading everything.
    let _ = stream.write_all(&bytes).await;

    let mut raw = Vec::new();
    let _ = stream.read_to_end(&mut raw).await;
    let text = String::from_utf8_lossy(&raw).into_owned();
    let (head, body) = text.split_once("\r\n\r\n").expect("complete response");
    let mut lines = head.split("\r\n");
    let status = lines
        .next()
        .unwrap()
        .split(' ')
        .nth(1)
        .unwrap()
        .parse()
        .unwrap();
    let headers = lines
        .filter_map(|line| line.split_once(": "))
        .map(|(name, value)| (name.to_owned(), value.to_owned()))
        .collect();
    Reply {
        status,
        headers,
        body: body.to_owned(),
    }
}

const SECRET: &str = "transport-test-secret";

fn unauthorized() -> Value {
    json!({"type": "error", "message": "Unauthorized."})
}

#[tokio::test]
async fn health_bypasses_authorization_only_for_get() {
    let _env = EnvScope::new(&[("GIT_API_TOKEN", Some(SECRET))]).await;
    let (address, _) = start().await;

    for path in ["/health", "/health/", "//health"] {
        let reply = http(address, "GET", path, &[], b"").await;
        assert_eq!(reply.status, 200, "{path}");
        assert_eq!(reply.header("content-type"), Some("application/json"));
        assert_eq!(reply.body, "{\"status\":\"ok\"}");
    }
    assert_eq!(
        http(address, "POST", "/health", &[], b"{}").await.status,
        401
    );
    assert_eq!(http(address, "HEAD", "/health", &[], b"").await.status, 401);
    assert_eq!(
        http(address, "GET", "/health/extra", &[], b"").await.status,
        401
    );
}

#[tokio::test]
async fn authorization_precedes_method_body_and_schema_checks() {
    let _env = EnvScope::new(&[("GIT_API_TOKEN", Some(SECRET))]).await;
    let (address, fake) = start().await;
    let oversized = vec![b'x'; MAX_REQUEST_BYTES + 100];

    for (method, body) in [
        ("GET", &b""[..]),
        ("PUT", b"{}"),
        ("POST", b"not json"),
        ("POST", &oversized[..]),
        ("POST", br#"{"url":"ok"}"#),
    ] {
        let missing = http(address, method, "/", &[], body).await;
        assert_eq!(missing.status, 401, "{method}");
        assert_eq!(missing.header("content-type"), Some("application/json"));
        assert_eq!(missing.json(), unauthorized());
        let wrong = http(address, method, "/", &[("X-Git-Api-Key", "nope")], body).await;
        assert_eq!(wrong.status, 401, "{method} wrong key");
    }
    assert!(
        fake.seen.lock().unwrap().is_empty(),
        "unauthorized requests reached the service"
    );

    for method in ["GET", "PUT", "DELETE", "PATCH"] {
        let reply = http(address, method, "/x", &[("x-git-api-key", SECRET)], b"").await;
        assert_eq!(reply.status, 405, "{method}");
        assert_eq!(reply.header("content-type"), Some("text/plain"));
        assert_eq!(reply.body, "Method Not Allowed");
    }
}

#[tokio::test]
async fn token_configuration_controls_the_key_requirement() {
    {
        let _env = EnvScope::new(&[("GIT_API_TOKEN", None)]).await;
        let (address, _) = start().await;
        assert_eq!(http(address, "GET", "/", &[], b"").await.status, 405);
        assert_eq!(
            http(address, "POST", "/", &[], br#"{"url":"ok"}"#)
                .await
                .status,
            200
        );
    }
    {
        // A present but empty token still requires a matching (empty) header.
        let _env = EnvScope::new(&[("GIT_API_TOKEN", Some(""))]).await;
        let (address, _) = start().await;
        assert_eq!(http(address, "GET", "/", &[], b"").await.status, 401);
        assert_eq!(
            http(address, "GET", "/", &[("X-Git-Api-Key", "")], b"")
                .await
                .status,
            405
        );
        assert_eq!(
            http(address, "GET", "/", &[("X-Git-Api-Key", "x")], b"")
                .await
                .status,
            401
        );
    }
}

#[tokio::test]
async fn post_accepts_any_path_without_a_content_type() {
    let _env = EnvScope::new(&[("GIT_API_TOKEN", Some(SECRET))]).await;
    let (address, fake) = start().await;
    let key = [("X-Git-Api-Key", SECRET)];

    for path in ["/", "/analyze", "/some/deep/path?query=1"] {
        let reply = http(
            address,
            "POST",
            path,
            &key,
            br#"{"url":"ok","ignored":[1,2]}"#,
        )
        .await;
        assert_eq!(reply.status, 200, "{path}");
        assert_eq!(reply.header("content-type"), Some("application/json"));
        let body = reply.json();
        assert_eq!(body["type"], "value");
        assert_eq!(body["data"]["repoName"], "sample");
        let keys: Vec<_> = body.as_object().unwrap().keys().cloned().collect();
        assert_eq!(keys, ["data", "type"]);
    }
    assert_eq!(fake.seen.lock().unwrap().len(), 3);
}

#[tokio::test]
async fn request_url_is_trimmed_and_token_is_forwarded() {
    let _env = EnvScope::new(&[("GIT_API_TOKEN", None)]).await;
    let (address, fake) = start().await;
    let reply = http(
        address,
        "POST",
        "/",
        &[],
        serde_json::json!({"url": "  \t ok \u{a0}\n", "auth_token": "repo-token"})
            .to_string()
            .as_bytes(),
    )
    .await;
    assert_eq!(reply.status, 200);
    let reply = http(
        address,
        "POST",
        "/",
        &[],
        br#"{"url":"ok","auth_token":null}"#,
    )
    .await;
    assert_eq!(reply.status, 200);
    assert_eq!(
        *fake.seen.lock().unwrap(),
        [
            ("ok".to_owned(), Some("repo-token".to_owned())),
            ("ok".to_owned(), None)
        ]
    );
}

fn padded_body(total: usize) -> Vec<u8> {
    let prefix = "{\"url\":\"ok\",\"pad\":\"";
    let suffix = "\"}";
    let padding = total - prefix.len() - suffix.len();
    let body = format!("{prefix}{}{suffix}", "x".repeat(padding));
    assert_eq!(body.len(), total);
    body.into_bytes()
}

#[tokio::test]
async fn request_size_limit_is_exact() {
    let _env = EnvScope::new(&[("GIT_API_TOKEN", None)]).await;
    let (address, _) = start().await;

    let exact = http(address, "POST", "/", &[], &padded_body(MAX_REQUEST_BYTES)).await;
    assert_eq!(exact.status, 200, "{}", exact.body);

    let over = http(
        address,
        "POST",
        "/",
        &[],
        &padded_body(MAX_REQUEST_BYTES + 1),
    )
    .await;
    assert_eq!(over.status, 400);
    assert_eq!(
        over.json(),
        json!({"type": "error", "message": "Request body is too large."})
    );
}

#[tokio::test]
async fn malformed_requests_are_rejected_with_the_schema_message() {
    let _env = EnvScope::new(&[("GIT_API_TOKEN", None)]).await;
    let (address, fake) = start().await;

    for body in [
        &b""[..],
        b"not json",
        b"[]",
        b"\"url\"",
        br#"{}"#,
        br#"{"url":7}"#,
        br#"{"url":null}"#,
        br#"{"url":"ok","auth_token":9}"#,
        br#"{"url":"ok"} trailing"#,
    ] {
        let reply = http(address, "POST", "/", &[], body).await;
        assert_eq!(reply.status, 400, "{:?}", String::from_utf8_lossy(body));
        assert_eq!(
            reply.json(),
            json!({"type": "error", "message": INVALID_JSON})
        );
    }
    assert!(fake.seen.lock().unwrap().is_empty());
}

#[tokio::test]
async fn service_failures_are_500_with_the_err_prefix() {
    let _env = EnvScope::new(&[("GIT_API_TOKEN", None)]).await;
    let (address, _) = start().await;
    let reply = http(address, "POST", "/", &[], br#"{"url":"fail"}"#).await;
    assert_eq!(reply.status, 500);
    assert_eq!(reply.header("content-type"), Some("application/json"));
    assert_eq!(
        reply.json(),
        json!({"type": "error", "message": "err: boom"})
    );
}

#[tokio::test]
async fn http_completion_does_not_depend_on_draining_progress() {
    let _env = EnvScope::new(&[("GIT_API_TOKEN", None)]).await;
    let (address, _) = start().await;
    let reply = tokio::time::timeout(
        Duration::from_secs(20),
        http(address, "POST", "/", &[], br#"{"url":"flood"}"#),
    )
    .await
    .expect("request stalled on progress overflow");
    assert_eq!(reply.status, 200);
}

// ---------------------------------------------------------------------------
// WebSocket
// ---------------------------------------------------------------------------

type Client = tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<TcpStream>>;

async fn connect(
    address: SocketAddr,
    path: &str,
    key: Option<&str>,
) -> Result<Client, tokio_tungstenite::tungstenite::Error> {
    let mut request = format!("ws://{address}{path}")
        .into_client_request()
        .unwrap();
    if let Some(key) = key {
        request
            .headers_mut()
            .insert("X-Git-Api-Key", key.parse().unwrap());
    }
    tokio_tungstenite::connect_async(request)
        .await
        .map(|(client, _)| client)
}

struct Conversation {
    texts: Vec<Value>,
    close: Option<(u16, String)>,
}

async fn converse(mut client: Client, message: Message) -> Conversation {
    client.send(message).await.unwrap();
    let mut texts = Vec::new();
    let mut close = None;
    let finished = tokio::time::timeout(Duration::from_secs(20), async {
        while let Some(frame) = client.next().await {
            match frame {
                Ok(Message::Text(text)) => texts.push(serde_json::from_str(text.as_str()).unwrap()),
                Ok(Message::Close(frame)) => {
                    close = frame.map(|frame| (u16::from(frame.code), frame.reason.to_string()));
                    break;
                }
                Ok(_) => {}
                Err(_) => break,
            }
        }
    })
    .await;
    assert!(finished.is_ok(), "websocket conversation stalled");
    Conversation { texts, close }
}

fn done() -> Option<(u16, String)> {
    Some((1000, "Done".to_owned()))
}

#[tokio::test]
async fn websocket_streams_progress_then_the_value_then_closes() {
    let _env = EnvScope::new(&[("GIT_API_TOKEN", Some(SECRET))]).await;
    let (address, fake) = start().await;

    // The upgrade is accepted on any path.
    for path in ["/", "/ws", "/deep/er/path"] {
        let client = connect(address, path, Some(SECRET)).await.unwrap();
        let conversation =
            converse(client, Message::text(r#"{"url":"  ok ","auth_token":"t"}"#)).await;
        assert_eq!(
            conversation.texts.len(),
            3,
            "{path}: {:?}",
            conversation.texts
        );
        assert_eq!(
            conversation.texts[0],
            json!({"type": "text_update", "data": "p1"})
        );
        assert_eq!(
            conversation.texts[1],
            json!({"type": "text_update", "data": "p2"})
        );
        assert_eq!(conversation.texts[2]["type"], "value");
        assert_eq!(conversation.texts[2]["data"]["repoName"], "sample");
        assert_eq!(conversation.close, done());
    }
    assert_eq!(
        fake.seen.lock().unwrap()[0],
        ("ok".to_owned(), Some("t".to_owned()))
    );
}

#[tokio::test]
async fn websocket_accepts_binary_requests() {
    let _env = EnvScope::new(&[("GIT_API_TOKEN", None)]).await;
    let (address, _) = start().await;
    let client = connect(address, "/", None).await.unwrap();
    let conversation = converse(client, Message::binary(br#"{"url":"ok"}"#.to_vec())).await;
    assert_eq!(conversation.texts.last().unwrap()["type"], "value");
    assert_eq!(conversation.close, done());
}

#[tokio::test]
async fn websocket_failures_send_the_raw_error_then_the_error_envelope() {
    let _env = EnvScope::new(&[("GIT_API_TOKEN", None)]).await;
    let (address, _) = start().await;
    let client = connect(address, "/", None).await.unwrap();
    let conversation = converse(client, Message::text(r#"{"url":"fail"}"#)).await;
    let tail = &conversation.texts[conversation.texts.len() - 2..];
    assert_eq!(tail[0], json!({"type": "text_update", "data": "boom"}));
    assert_eq!(tail[1], json!({"type": "error", "message": "err: boom"}));
    assert_eq!(conversation.close, done());
}

#[tokio::test]
async fn websocket_bad_input_closes_with_bad_input() {
    let _env = EnvScope::new(&[("GIT_API_TOKEN", None)]).await;
    let (address, fake) = start().await;
    let bad_input = Some((1000, "Bad input".to_owned()));

    for text in ["nope", r#"{"url":3}"#, r#"{"auth_token":"x"}"#, "[]"] {
        let client = connect(address, "/", None).await.unwrap();
        let conversation = converse(client, Message::text(text)).await;
        assert_eq!(
            conversation.texts,
            [json!({"type": "error", "message": INVALID_JSON})],
            "{text}"
        );
        assert_eq!(conversation.close, bad_input, "{text}");
    }

    let client = connect(address, "/", None).await.unwrap();
    let too_large = String::from_utf8(padded_body(MAX_REQUEST_BYTES + 1)).unwrap();
    let conversation = converse(client, Message::text(too_large)).await;
    assert_eq!(
        conversation.texts,
        [json!({"type": "error", "message": "Request body is too large."})]
    );
    assert_eq!(conversation.close, bad_input);

    // The limit is inclusive for WebSocket messages too.
    let client = connect(address, "/", None).await.unwrap();
    let exact = String::from_utf8(padded_body(MAX_REQUEST_BYTES)).unwrap();
    let conversation = converse(client, Message::text(exact)).await;
    assert_eq!(conversation.close, done());
    assert_eq!(fake.seen.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn websocket_is_authenticated_before_acceptance() {
    let _env = EnvScope::new(&[("GIT_API_TOKEN", Some(SECRET))]).await;
    let (address, fake) = start().await;

    for key in [None, Some("wrong")] {
        match connect(address, "/", key).await {
            Err(tokio_tungstenite::tungstenite::Error::Http(response)) => {
                assert_eq!(response.status().as_u16(), 400);
                assert_eq!(response.body().as_deref(), Some(&b"Unauthorized"[..]));
            }
            Err(other) => panic!("unexpected error {other:?}"),
            Ok(_) => panic!("unauthorized websocket was accepted"),
        }
    }
    assert!(fake.seen.lock().unwrap().is_empty());
    assert!(connect(address, "/", Some(SECRET)).await.is_ok());
}
