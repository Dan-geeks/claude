//! End-to-end checks for the Claude -> Codex failover in the request path.

use async_trait::async_trait;
use axum::body::Body;
use axum::http::{Method, Request, StatusCode, header};
use axum::response::{IntoResponse, Response};
use claude_codex::{
    MessagesRequest,
    config::AliasProvider,
    provider::{CliHandlers, Provider, RequestContext},
    registry::Registry,
    server::app,
};
use http_body_util::BodyExt;
use serde_json::json;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use tower::util::ServiceExt;

struct NoCli;
impl CliHandlers for NoCli {
    fn login(&self) -> anyhow::Result<()> {
        Ok(())
    }
    fn device(&self) -> anyhow::Result<()> {
        Ok(())
    }
    fn status(&self) -> anyhow::Result<()> {
        Ok(())
    }
    fn logout(&self) -> anyhow::Result<()> {
        Ok(())
    }
}
static NO_CLI: NoCli = NoCli;

/// Stands in for the Claude passthrough, answering every request with `status`.
struct FakeClaude {
    status: StatusCode,
    calls: Arc<AtomicUsize>,
}

#[async_trait]
impl Provider for FakeClaude {
    fn name(&self) -> &'static str {
        "anthropic"
    }
    fn supported_models(&self) -> Vec<String> {
        vec!["claude-fable-5-1".into()]
    }
    fn cli(&self) -> &'static dyn CliHandlers {
        &NO_CLI
    }
    async fn handle_messages(&self, _body: MessagesRequest, _ctx: RequestContext) -> Response {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if self.status == StatusCode::OK {
            return (StatusCode::OK, "claude-ok").into_response();
        }
        Response::builder()
            .status(self.status)
            .header(header::RETRY_AFTER, "600")
            .body(Body::from(r#"{"type":"error","error":{"type":"rate_limit_error"}}"#))
            .unwrap()
    }
    async fn handle_count_tokens(&self, _body: MessagesRequest, _ctx: RequestContext) -> Response {
        (StatusCode::OK, "{}").into_response()
    }
}

/// Stands in for Codex. It reports a real `gpt-*` model, like the translator does.
struct FakeCodex {
    calls: Arc<AtomicUsize>,
    saw_thread: Arc<AtomicUsize>,
}

#[async_trait]
impl Provider for FakeCodex {
    fn name(&self) -> &'static str {
        "codex"
    }
    fn supported_models(&self) -> Vec<String> {
        vec!["gpt-6-sol".into()]
    }
    fn cli(&self) -> &'static dyn CliHandlers {
        &NO_CLI
    }
    async fn handle_messages(&self, body: MessagesRequest, _ctx: RequestContext) -> Response {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if body.extra.contains_key("thread") {
            self.saw_thread.fetch_add(1, Ordering::SeqCst);
        }
        let sse = concat!(
            "event: message_start\n",
            "data: {\"type\":\"message_start\",\"message\":{\"id\":\"m\",\"model\":\"gpt-6-sol\",\"content\":[]}}\n\n",
            "event: content_block_delta\n",
            "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"from-codex\"}}\n\n",
            "event: message_stop\n",
            "data: {\"type\":\"message_stop\"}\n\n",
        );
        Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, "text/event-stream")
            .body(Body::from(sse))
            .unwrap()
    }
    async fn handle_count_tokens(&self, _body: MessagesRequest, _ctx: RequestContext) -> Response {
        (StatusCode::OK, "{}").into_response()
    }
}

struct Harness {
    app: axum::Router,
    claude_calls: Arc<AtomicUsize>,
    codex_calls: Arc<AtomicUsize>,
    codex_saw_thread: Arc<AtomicUsize>,
}

fn harness(claude_status: StatusCode) -> Harness {
    let claude_calls = Arc::new(AtomicUsize::new(0));
    let codex_calls = Arc::new(AtomicUsize::new(0));
    let codex_saw_thread = Arc::new(AtomicUsize::new(0));
    let providers: Vec<Arc<dyn Provider>> = vec![
        Arc::new(FakeClaude {
            status: claude_status,
            calls: claude_calls.clone(),
        }),
        Arc::new(FakeCodex {
            calls: codex_calls.clone(),
            saw_thread: codex_saw_thread.clone(),
        }),
    ];
    let registry = Arc::new(Registry::from_providers(AliasProvider::Anthropic, providers));
    Harness {
        app: app(registry),
        claude_calls,
        codex_calls,
        codex_saw_thread,
    }
}

async fn ask(app: &axum::Router, model: &str) -> (StatusCode, String) {
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/v1/messages")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({"model": model, "messages": [{"role": "user", "content": "hi"}]})
                        .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let body = response.into_body().collect().await.unwrap().to_bytes();
    (status, String::from_utf8_lossy(&body).into_owned())
}

#[tokio::test]
async fn claude_usage_limit_falls_over_to_codex_and_hides_the_swap() {
    let h = harness(StatusCode::TOO_MANY_REQUESTS);
    let (status, body) = ask(&h.app, "claude-fable-5-1").await;

    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body.contains("from-codex"), "{body}");
    // The client asked for Fable and still sees Fable, not gpt-*.
    assert!(body.contains("\"model\":\"claude-fable-5-1\""), "{body}");
    assert!(!body.contains("gpt-6-sol"), "{body}");
    assert_eq!(h.claude_calls.load(Ordering::SeqCst), 1);
    assert_eq!(h.codex_calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn later_requests_skip_claude_during_the_cooldown() {
    let h = harness(StatusCode::TOO_MANY_REQUESTS);
    for _ in 0..3 {
        let (status, body) = ask(&h.app, "claude-fable-5-1").await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert!(body.contains("from-codex"), "{body}");
    }
    // Claude was probed once, by the first request. The rest went straight to Codex.
    assert_eq!(h.claude_calls.load(Ordering::SeqCst), 1);
    assert_eq!(h.codex_calls.load(Ordering::SeqCst), 3);
}

#[tokio::test]
async fn healthy_claude_is_untouched() {
    let h = harness(StatusCode::OK);
    let (status, body) = ask(&h.app, "claude-fable-5-1").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, "claude-ok");
    assert_eq!(h.codex_calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn other_claude_errors_are_not_failed_over() {
    for status in [
        StatusCode::BAD_REQUEST,
        StatusCode::UNAUTHORIZED,
        StatusCode::INTERNAL_SERVER_ERROR,
    ] {
        let h = harness(status);
        let (got, _) = ask(&h.app, "claude-fable-5-1").await;
        assert_eq!(got, status);
        assert_eq!(h.codex_calls.load(Ordering::SeqCst), 0, "{status}");
    }
}

#[tokio::test]
async fn message_threads_are_dropped_on_the_codex_retry() {
    let h = harness(StatusCode::TOO_MANY_REQUESTS);
    let response = h
        .app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/v1/messages")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({
                        "model": "claude-fable-5-1",
                        "messages": [{"role": "user", "content": "hi"}],
                        "thread": {"type": "continue", "id": "t1"}
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(h.codex_saw_thread.load(Ordering::SeqCst), 0);
}
