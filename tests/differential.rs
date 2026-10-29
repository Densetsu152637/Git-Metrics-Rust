//! Compares the Rust service with a prebuilt Haskell `git-api` on one fixture.
//!
//! Ignored by default because it needs the legacy binary and port 8081:
//! `HASKELL_GIT_API_EXE=<path to git-api> cargo test --test differential -- --ignored`

mod common;

use std::time::Duration;

use common::{build_fixture, make_writable, rewrite_env, EnvScope};
use git_api::process::RepositoryService;
use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

async fn http(method: &str, path: &str, body: &str) -> Option<(u16, String)> {
    let mut stream = TcpStream::connect(("127.0.0.1", 8081)).await.ok()?;
    let request = format!(
        "{method} {path} HTTP/1.1\r\nHost: t\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(request.as_bytes()).await.ok()?;
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).await.ok()?;
    let text = String::from_utf8_lossy(&raw).into_owned();
    let (head, body) = text.split_once("\r\n\r\n")?;
    let status = head.split(' ').nth(1)?.parse().ok()?;
    Some((status, body.to_owned()))
}

/// Decodes a chunked transfer-encoded body when the server used one.
fn dechunk(body: &str) -> String {
    let mut rest = body;
    let mut out = String::new();
    while let Some((size, tail)) = rest.split_once("\r\n") {
        let Ok(length) = usize::from_str_radix(size.trim(), 16) else {
            return body.to_owned();
        };
        if length == 0 {
            break;
        }
        out.push_str(&tail[..length]);
        rest = &tail[length + 2..];
    }
    out
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs HASKELL_GIT_API_EXE and port 8081"]
async fn rust_matches_the_haskell_service() {
    let Ok(executable) = std::env::var("HASKELL_GIT_API_EXE") else {
        panic!("set HASKELL_GIT_API_EXE to the legacy git-api binary");
    };
    let fixture = build_fixture("differential");
    make_writable(&fixture.root);
    let vars = rewrite_env(&fixture);
    let borrowed: Vec<(&str, Option<&str>)> =
        vars.iter().map(|(k, v)| (*k, v.as_deref())).collect();
    let _env = EnvScope::new(&borrowed).await;

    // Keep the service's clone directory deletable while it runs (see make_writable).
    let relaxer = tokio::spawn(async {
        let clones = std::env::temp_dir().join("cloned-repos");
        loop {
            make_writable(&clones);
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    });
    let mut haskell = tokio::process::Command::new(executable)
        .kill_on_drop(true)
        .spawn()
        .expect("start the Haskell service");
    let mut ready = false;
    for _ in 0..120 {
        if http("GET", "/health", "")
            .await
            .is_some_and(|(status, _)| status == 200)
        {
            ready = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    assert!(ready, "Haskell service did not become healthy");

    let url = "https://github.com/owner/differential";
    let request = format!("{{\"url\":\"{url}\"}}");
    let (status, body) = http("POST", "/", &request).await.expect("Haskell response");
    assert_eq!(status, 200, "{body}");
    let haskell_value: Value = serde_json::from_str(&dechunk(&body)).unwrap();

    // Failure bodies must agree too: validation, token precedence, and a missing repository.
    let failures = [
        (
            r#"{"url":"http://github.com/owner/x"}"#,
            "http://github.com/owner/x",
            None,
        ),
        (
            r#"{"url":"https://github.com/owner/..git"}"#,
            "https://github.com/owner/..git",
            None,
        ),
        (r#"{"url":"bad","auth_token":""}"#, "bad", Some("")),
        (
            r#"{"url":"https://github.com/owner/missing"}"#,
            "https://github.com/owner/missing",
            None,
        ),
    ];
    let mut haskell_failures = Vec::new();
    for (request, _, _) in &failures {
        let (status, body) = http("POST", "/", request).await.expect("Haskell response");
        haskell_failures.push((status, dechunk(&body)));
    }
    let _ = haskell.kill().await;
    relaxer.abort();

    let service = RepositoryService::new();
    let (progress, _updates) = tokio::sync::mpsc::channel(1000);
    let data = service
        .analyze(url.to_owned(), None, progress)
        .await
        .unwrap();
    let rust_value =
        serde_json::json!({"type": "value", "data": serde_json::to_value(&*data).unwrap()});

    assert_eq!(rust_value, haskell_value);

    for ((_, url, token), (status, body)) in failures.iter().zip(&haskell_failures) {
        let (progress, _updates) = tokio::sync::mpsc::channel(1000);
        let error = service
            .analyze((*url).to_owned(), token.map(str::to_owned), progress)
            .await
            .unwrap_err();
        let rust_body = serde_json::json!({"type": "error", "message": format!("err: {error}")});
        let haskell_body: Value = serde_json::from_str(body).unwrap();
        assert_eq!(*status, 500, "{url}");
        // The one intended correction: Haskell accepts `..git` and then probes the
        // directory-traversal path `owner/.`; Rust rejects the URL up front.
        if *url == "https://github.com/owner/..git" {
            assert_eq!(error, "Repository path contains unsupported characters.");
            continue;
        }
        assert_eq!(rust_body, haskell_body, "{url}");
    }
}
