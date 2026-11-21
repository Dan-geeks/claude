//! Automatic Claude -> Codex failover.
//!
//! When the Claude subscription answers a request with HTTP 429 (its 5-hour or
//! weekly usage limit, or a plain rate limit), the proxy re-sends the same
//! request to the Codex backend instead of returning the error. The limit is
//! remembered for a cooldown so later requests go straight to Codex without
//! probing Claude first, and Claude is tried again once the cooldown ends.
//!
//! By default the model name in Codex's reply is rewritten back to the model
//! Claude Code asked for, so a session that started on a Claude model keeps
//! showing that model. Set `CCP_FAILOVER_MASK_MODEL=0` to expose the real
//! backend model instead.
//!
//! Environment:
//! - `CCP_FAILOVER`: `0`/`false`/`off` disables failover (default: on).
//! - `CCP_FAILOVER_MASK_MODEL`: `0`/`false`/`off` shows the real model name.

use axum::{
    body::Body,
    http::{HeaderMap, StatusCode, header},
    response::Response,
};
use bytes::Bytes;
use futures_util::StreamExt;
use http_body_util::BodyExt;
use serde_json::Value;
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// Used when the 429 carries no usable reset time.
const DEFAULT_COOLDOWN: Duration = Duration::from_secs(300);
/// A plan limit can reset hours away; never block Claude for longer than this.
const MAX_COOLDOWN: Duration = Duration::from_secs(6 * 3600);
/// Never probe Claude more often than this, even if the server says "retry now".
const MIN_COOLDOWN: Duration = Duration::from_secs(30);

pub struct Failover {
    enabled: bool,
    mask_model: bool,
    blocked_until: Mutex<Option<Instant>>,
}

fn env_flag(name: &str, default: bool) -> bool {
    match std::env::var(name) {
        Ok(value) => !matches!(
            value.trim().to_ascii_lowercase().as_str(),
            "0" | "false" | "off" | "no"
        ),
        Err(_) => default,
    }
}

impl Failover {
    pub fn new(enabled: bool, mask_model: bool) -> Self {
        Self {
            enabled,
            mask_model,
            blocked_until: Mutex::new(None),
        }
    }

    pub fn from_env() -> Self {
        Self::new(
            env_flag("CCP_FAILOVER", true),
            env_flag("CCP_FAILOVER_MASK_MODEL", true),
        )
    }

    pub fn enabled(&self) -> bool {
        self.enabled
    }

    pub fn mask_model(&self) -> bool {
        self.mask_model
    }

    /// True while Claude is in its post-limit cooldown.
    pub fn is_blocked(&self) -> bool {
        let mut guard = self.blocked_until.lock().unwrap_or_else(|e| e.into_inner());
        match *guard {
            Some(until) if Instant::now() < until => true,
            Some(_) => {
                *guard = None;
                false
            }
            None => false,
        }
    }

    pub fn block_for(&self, cooldown: Duration) {
        let mut guard = self.blocked_until.lock().unwrap_or_else(|e| e.into_inner());
        *guard = Some(Instant::now() + cooldown);
    }
}

/// If this Anthropic response is a usage/rate limit, how long to stay on Codex.
pub fn limit_cooldown(status: StatusCode, headers: &HeaderMap) -> Option<Duration> {
    if status != StatusCode::TOO_MANY_REQUESTS {
        return None;
    }
    let retry_after = headers
        .get(header::RETRY_AFTER)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().parse::<u64>().ok())
        .map(Duration::from_secs);
    let unified_reset = headers
        .get("anthropic-ratelimit-unified-reset")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().parse::<u64>().ok())
        .and_then(|epoch| {
            let now = SystemTime::now().duration_since(UNIX_EPOCH).ok()?.as_secs();
            epoch.checked_sub(now).map(Duration::from_secs)
        });
    let cooldown = retry_after.or(unified_reset).unwrap_or(DEFAULT_COOLDOWN);
    Some(cooldown.clamp(MIN_COOLDOWN, MAX_COOLDOWN))
}

/// Rewrite the `model` the backend reports to the one the client asked for.
pub async fn mask_model(response: Response, requested: &str) -> Response {
    if !response.status().is_success() {
        return response;
    }
    let is_sse = response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.starts_with("text/event-stream"));
    let (mut parts, body) = response.into_parts();
    let requested = requested.to_string();

    if is_sse {
        let stream = futures_util::stream::unfold(
            (body.into_data_stream(), Vec::<u8>::new(), false),
            move |(mut inner, mut buf, finished)| {
                let requested = requested.clone();
                async move {
                    if finished {
                        return None;
                    }
                    loop {
                        if let Some(pos) = buf.iter().rposition(|b| *b == b'\n') {
                            let rest = buf.split_off(pos + 1);
                            let complete = std::mem::replace(&mut buf, rest);
                            let out = Bytes::from(rewrite_sse(&complete, &requested));
                            return Some((Ok::<Bytes, axum::Error>(out), (inner, buf, false)));
                        }
                        match inner.next().await {
                            Some(Ok(chunk)) => buf.extend_from_slice(&chunk),
                            Some(Err(err)) => return Some((Err(err), (inner, buf, true))),
                            None => {
                                if buf.is_empty() {
                                    return None;
                                }
                                let rest = std::mem::take(&mut buf);
                                let out = Bytes::from(rewrite_sse(&rest, &requested));
                                return Some((Ok(out), (inner, buf, true)));
                            }
                        }
                    }
                }
            },
        );
        return Response::from_parts(parts, Body::from_stream(stream));
    }

    let bytes = match body.collect().await {
        Ok(collected) => collected.to_bytes(),
        Err(_) => return Response::from_parts(parts, Body::empty()),
    };
    let out = match serde_json::from_slice::<Value>(&bytes) {
        Ok(mut value) if value.get("type").and_then(Value::as_str) == Some("message") => {
            value["model"] = Value::String(requested);
            serde_json::to_vec(&value).map(Bytes::from).unwrap_or(bytes)
        }
        _ => bytes,
    };
    parts.headers.remove(header::CONTENT_LENGTH);
    Response::from_parts(parts, Body::from(out))
}

/// Rewrites `message.model` in `message_start` events; every other line is untouched.
fn rewrite_sse(chunk: &[u8], requested: &str) -> Vec<u8> {
    let Ok(text) = std::str::from_utf8(chunk) else {
        return chunk.to_vec();
    };
    let mut out = String::with_capacity(text.len());
    for line in text.split_inclusive('\n') {
        match rewrite_sse_line(line, requested) {
            Some(rewritten) => out.push_str(&rewritten),
            None => out.push_str(line),
        }
    }
    out.into_bytes()
}

fn rewrite_sse_line(line: &str, requested: &str) -> Option<String> {
    let rest = line.strip_prefix("data:")?;
    if !rest.contains("message_start") {
        return None;
    }
    let mut value: Value = serde_json::from_str(rest.trim()).ok()?;
    if value.get("type").and_then(Value::as_str) != Some("message_start") {
        return None;
    }
    *value.get_mut("message")?.get_mut("model")? = Value::String(requested.to_string());
    let newline = if line.ends_with("\r\n") {
        "\r\n"
    } else if line.ends_with('\n') {
        "\n"
    } else {
        ""
    };
    Some(format!("data: {}{newline}", serde_json::to_string(&value).ok()?))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers(pairs: &[(&'static str, &str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (name, value) in pairs {
            map.insert(*name, value.parse().unwrap());
        }
        map
    }

    #[test]
    fn only_429_triggers_failover() {
        assert!(limit_cooldown(StatusCode::OK, &HeaderMap::new()).is_none());
        assert!(limit_cooldown(StatusCode::BAD_REQUEST, &HeaderMap::new()).is_none());
        assert!(limit_cooldown(StatusCode::BAD_GATEWAY, &HeaderMap::new()).is_none());
        assert_eq!(
            limit_cooldown(StatusCode::TOO_MANY_REQUESTS, &HeaderMap::new()),
            Some(DEFAULT_COOLDOWN)
        );
    }

    #[test]
    fn cooldown_follows_retry_after_and_is_clamped() {
        let h = headers(&[("retry-after", "600")]);
        assert_eq!(
            limit_cooldown(StatusCode::TOO_MANY_REQUESTS, &h),
            Some(Duration::from_secs(600))
        );
        let tiny = headers(&[("retry-after", "1")]);
        assert_eq!(
            limit_cooldown(StatusCode::TOO_MANY_REQUESTS, &tiny),
            Some(MIN_COOLDOWN)
        );
        let huge = headers(&[("retry-after", "999999")]);
        assert_eq!(
            limit_cooldown(StatusCode::TOO_MANY_REQUESTS, &huge),
            Some(MAX_COOLDOWN)
        );
    }

    #[test]
    fn cooldown_follows_unified_reset_epoch() {
        let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs();
        let h = headers(&[("anthropic-ratelimit-unified-reset", &(now + 1200).to_string())]);
        let got = limit_cooldown(StatusCode::TOO_MANY_REQUESTS, &h).unwrap();
        assert!(got >= Duration::from_secs(1190) && got <= Duration::from_secs(1200));
    }

    #[test]
    fn block_expires() {
        let f = Failover::new(true, true);
        assert!(!f.is_blocked());
        f.block_for(Duration::from_millis(30));
        assert!(f.is_blocked());
        std::thread::sleep(Duration::from_millis(60));
        assert!(!f.is_blocked());
    }

    #[test]
    fn rewrites_message_start_model_only() {
        let sse = concat!(
            "event: message_start\n",
            "data: {\"type\":\"message_start\",\"message\":{\"id\":\"m1\",\"model\":\"gpt-6-sol\",\"content\":[]}}\n",
            "\n",
            "event: content_block_delta\n",
            "data: {\"type\":\"content_block_delta\",\"delta\":{\"text\":\"model gpt-6-sol\"}}\n",
            "\n",
        );
        let out = String::from_utf8(rewrite_sse(sse.as_bytes(), "claude-fable-5-1")).unwrap();
        assert!(out.contains("\"model\":\"claude-fable-5-1\""));
        assert!(!out.contains("\"model\":\"gpt-6-sol\""));
        // Text the model wrote is never altered.
        assert!(out.contains("\"text\":\"model gpt-6-sol\""));
        assert!(out.contains("event: message_start\n"));
    }

    #[tokio::test]
    async fn masks_streamed_response_split_across_chunks() {
        let line = "data: {\"type\":\"message_start\",\"message\":{\"model\":\"gpt-6-sol\"}}\n\n";
        let (a, b) = line.split_at(30);
        let chunks = vec![
            Ok::<Bytes, std::io::Error>(Bytes::from(a.to_string())),
            Ok(Bytes::from(b.to_string())),
        ];
        let response = Response::builder()
            .header(header::CONTENT_TYPE, "text/event-stream")
            .body(Body::from_stream(futures_util::stream::iter(chunks)))
            .unwrap();
        let masked = mask_model(response, "opus").await;
        let body = masked.into_body().collect().await.unwrap().to_bytes();
        let text = String::from_utf8(body.to_vec()).unwrap();
        assert!(text.contains("\"model\":\"opus\""), "{text}");
    }

    #[tokio::test]
    async fn masks_non_streaming_message() {
        let response = Response::builder()
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(
                r#"{"type":"message","model":"gpt-6-sol","content":[]}"#,
            ))
            .unwrap();
        let masked = mask_model(response, "opus").await;
        let body = masked.into_body().collect().await.unwrap().to_bytes();
        let value: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(value["model"], "opus");
    }

    #[tokio::test]
    async fn error_responses_are_left_alone() {
        let response = Response::builder()
            .status(StatusCode::BAD_REQUEST)
            .body(Body::from(r#"{"type":"error","model":"gpt-6-sol"}"#))
            .unwrap();
        let masked = mask_model(response, "opus").await;
        let body = masked.into_body().collect().await.unwrap().to_bytes();
        assert!(String::from_utf8_lossy(&body).contains("gpt-6-sol"));
    }
}
