use axum::{
    Router,
    body::Body,
    extract::State,
    http::{Method, Request, StatusCode},
    response::IntoResponse,
};
use serde_json::{Value, json};
use std::{
    process::{Child, Command, Stdio},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

#[derive(Clone, Default)]
struct Upstream {
    models: Arc<AtomicUsize>,
    posts: Arc<AtomicUsize>,
}

async fn upstream(
    State(state): State<Upstream>,
    request: Request<Body>,
) -> axum::response::Response {
    if request.method() == Method::GET && request.uri().path() == "/models" {
        assert_eq!(
            request.headers()["authorization"],
            "Bearer isolated-test-token"
        );
        assert_eq!(request.headers()["chatgpt-account-id"], "isolated-account");
        state.models.fetch_add(1, Ordering::SeqCst);
        return axum::Json(json!({"models":[
            {"slug":"gpt-discovered", "supported_in_api":false, "visibility":"list", "priority":1,
             "context_window":200000,"max_context_window":1000000,"auto_compact_token_limit":150000},
            {"slug":"gpt-hidden", "supported_in_api":true, "visibility":"hide", "priority":0,
             "context_window":80000},
            {"slug":"gpt-second", "supported_in_api":true, "visibility":"list", "priority":2,
             "context_window":100000}
        ]})).into_response();
    }
    assert_eq!(
        request.method(),
        Method::POST,
        "unexpected discovery path {}",
        request.uri()
    );
    assert!(
        matches!(request.uri().path(), "/" | "/responses" | "/responses/lite"),
        "{}",
        request.uri()
    );
    state.posts.fetch_add(1, Ordering::SeqCst);
    let bytes = axum::body::to_bytes(request.into_body(), 1_000_000)
        .await
        .unwrap();
    let body: Value = serde_json::from_slice(&bytes).unwrap();
    if body["model"] == "unadvertised-model" {
        return (
            StatusCode::NOT_FOUND,
            axum::Json(json!({"error":{"message":"Codex rejected unadvertised-model"}})),
        )
            .into_response();
    }
    assert_eq!(body["model"], "gpt-discovered");
    (
        StatusCode::IM_A_TEAPOT,
        axum::Json(json!({"error":{"message":"isolated upstream reached"}})),
    )
        .into_response()
}

struct ChildGuard(Child);
impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn isolated_command(config: &std::path::Path, upstream: &str) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_claude-code-proxy"));
    command
        .env_clear()
        .env("HOME", config)
        .env("CCP_CONFIG_DIR", config)
        .env("CCP_CODEX_BASE_URL", upstream)
        .env("CCP_CODEX_TRANSPORT", "http")
        .env("CCP_CODEX_RESPONSES_API", "1")
        .env("NO_PROXY", "127.0.0.1,localhost")
        .env("no_proxy", "127.0.0.1,localhost");
    command
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn discovery_scopes_all_text_ingress_and_cli_without_constructor_io() {
    let state = Upstream::default();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream_url = format!("http://{}", listener.local_addr().unwrap());
    let fixture = tokio::spawn(
        axum::serve(
            listener,
            Router::new().fallback(upstream).with_state(state.clone()),
        )
        .into_future(),
    );
    let config = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(config.path().join("codex")).unwrap();
    std::fs::write(config.path().join("codex/auth.json"), r#"{"access":"isolated-test-token","refresh":"isolated-refresh","expires":4102444800000,"accountId":"isolated-account"}"#).unwrap();
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let mut service = ChildGuard(
        isolated_command(config.path(), &upstream_url)
            .args(["serve", "--no-monitor", "--port", &port.to_string()])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap(),
    );
    let base = format!("http://127.0.0.1:{port}");
    let client = reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(5))
        .build()
        .unwrap();
    for attempt in 0..100 {
        if client.get(format!("{base}/healthz")).send().await.is_ok() {
            break;
        }
        assert!(
            service.0.try_wait().unwrap().is_none(),
            "proxy exited during startup"
        );
        assert!(attempt < 99, "proxy did not start");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(state.models.load(Ordering::SeqCst), 0);
    assert_eq!(
        client
            .get(format!("{base}/monitor"))
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::OK
    );
    assert_eq!(state.models.load(Ordering::SeqCst), 0);
    let listed: Value = client
        .get(format!("{base}/v1/models"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let ids: Vec<_> = listed["data"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|entry| entry["id"].as_str())
        .collect();
    assert!(ids.contains(&"gpt-discovered"), "{listed}");
    assert!(ids.contains(&"gpt-discovered-fast"), "{listed}");
    assert!(!ids.contains(&"gpt-hidden"), "{listed}");
    assert!(
        !ids.contains(&"gpt-5.4"),
        "stale compiled model leaked into live listing"
    );
    assert_eq!(state.models.load(Ordering::SeqCst), 1);
    let messages = json!({"model":"gpt-discovered","max_tokens":32,"messages":[{"role":"user","content":"hello"}]});
    let count = client
        .post(format!("{base}/v1/messages/count_tokens"))
        .json(&messages)
        .send()
        .await
        .unwrap();
    assert_eq!(
        count.status(),
        StatusCode::OK,
        "{}",
        count.text().await.unwrap()
    );
    for (path, body) in [
        ("/v1/messages", messages),
        (
            "/v1/responses",
            json!({"model":"gpt-discovered","input":"hello","stream":false}),
        ),
        (
            "/v1/chat/completions",
            json!({"model":"gpt-discovered","messages":[{"role":"user","content":"hello"}]}),
        ),
    ] {
        let response = client
            .post(format!("{base}{path}"))
            .json(&body)
            .send()
            .await
            .unwrap();
        assert_eq!(
            response.status(),
            StatusCode::IM_A_TEAPOT,
            "{path}: {}",
            response.text().await.unwrap()
        );
    }
    let stale = client
        .post(format!("{base}/v1/messages/count_tokens"))
        .json(&json!({"model":"gpt-5.4","max_tokens":32,"messages":[]}))
        .send()
        .await
        .unwrap();
    assert_eq!(stale.status(), StatusCode::OK);
    for (path, body) in [
        (
            "/v1/messages",
            json!({"model":"unadvertised-model","messages":[{"role":"user","content":"hello"}],"max_tokens":32}),
        ),
        (
            "/v1/responses",
            json!({"model":"unadvertised-model","input":"hello"}),
        ),
        (
            "/v1/chat/completions",
            json!({"model":"unadvertised-model","messages":[{"role":"user","content":"hello"}]}),
        ),
    ] {
        let response = client
            .post(format!("{base}{path}"))
            .json(&body)
            .send()
            .await
            .unwrap();
        assert_eq!(
            response.status(),
            StatusCode::NOT_FOUND,
            "{path}: {}",
            response.text().await.unwrap()
        );
    }
    assert_eq!(state.posts.load(Ordering::SeqCst), 6);
    assert_eq!(
        state.models.load(Ordering::SeqCst),
        1,
        "request-time discovery must reuse the bounded cache"
    );
    let output = isolated_command(config.path(), &upstream_url)
        .arg("models")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let models = String::from_utf8(output.stdout).unwrap();
    assert!(models.contains("gpt-discovered"));
    assert!(!models.contains("gpt-5.4"));
    let output = isolated_command(config.path(), &upstream_url)
        .args(["launch", "--print-env"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let env: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(env["ANTHROPIC_MODEL"], "gpt-discovered");
    assert_eq!(env["ANTHROPIC_SMALL_FAST_MODEL"], "gpt-discovered");
    assert_eq!(env["CLAUDE_CODE_MAX_CONTEXT_TOKENS"], "190000");
    assert_eq!(env["CLAUDE_CODE_AUTO_COMPACT_WINDOW"], "150000");
    assert_eq!(state.models.load(Ordering::SeqCst), 3);
    fixture.abort();
}
