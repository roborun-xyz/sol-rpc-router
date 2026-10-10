use std::{
    net::SocketAddr,
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

use axum::{
    body::{to_bytes, Body},
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        ConnectInfo, Query, State,
    },
    http::{header, HeaderMap, HeaderValue, Request, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
    Json,
};
use bytes::Bytes;
use futures_util::{SinkExt, StreamExt};
use http_body_util::BodyExt;
use metrics::{counter, gauge, histogram};
use rand::{distributions::Alphanumeric, Rng};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::{
    sync::mpsc,
    time::{timeout, Duration},
};
use tokio_tungstenite::{connect_async, tungstenite::Message as TungsteniteMessage};
use tracing::{error, info, warn};

use crate::{
    keystore::{KeyStoreError, MAX_KEY_LEN},
    rpc::{self, codes, RpcRequestInfo},
    state::{AppState, HttpClient, Selection},
    upstream,
};

const MAX_BODY_SIZE: usize = 10 * 1024 * 1024; // 10 MB
/// Fan-out responses are buffered so we can inspect them; sendTransaction
/// replies are tiny, so this is generous.
const MAX_FANOUT_RESPONSE_SIZE: usize = 1024 * 1024;
const WS_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// Slow-body clients are cut off after this long.
const BODY_READ_TIMEOUT: Duration = Duration::from_secs(10);

pub const HEALTH_PATH: &str = "/health";
pub const REQUEST_ID_HEADER: &str = "x-request-id";
pub const BACKEND_HEADER: &str = "x-rpc-backend";
pub const ATTEMPTS_HEADER: &str = "x-rpc-attempts";
pub const API_KEY_HEADER: &str = "x-api-key";

// ---------------------------------------------------------------------------
// Request/response extensions shared between middleware and handlers
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub struct RpcMethod(pub String);

#[derive(Clone)]
pub struct SelectedBackend(pub String);

#[derive(Clone)]
pub struct ClientOwner(pub String);

#[derive(Clone)]
pub struct RequestId(pub String);

#[derive(Clone, Copy)]
pub struct Attempts(pub u32);

#[derive(Deserialize)]
pub struct Params {
    #[serde(rename = "api-key")]
    pub api_key: Option<String>,
}

// ---------------------------------------------------------------------------
// Middleware
// ---------------------------------------------------------------------------

/// Assigns (or propagates) an `x-request-id` so a single call can be traced
/// across router logs, upstream logs and the client.
pub async fn request_id(mut req: Request<Body>, next: Next) -> Response {
    let id = req
        .headers()
        .get(REQUEST_ID_HEADER)
        .and_then(|v| v.to_str().ok())
        .filter(|s| !s.is_empty() && s.len() <= 128)
        .map(str::to_string)
        .unwrap_or_else(new_request_id);

    if let Ok(v) = HeaderValue::from_str(&id) {
        req.headers_mut().insert(REQUEST_ID_HEADER, v.clone());
        req.extensions_mut().insert(RequestId(id));
        let mut resp = next.run(req).await;
        resp.headers_mut().insert(REQUEST_ID_HEADER, v);
        resp
    } else {
        next.run(req).await
    }
}

fn new_request_id() -> String {
    rand::thread_rng()
        .sample_iter(&Alphanumeric)
        .take(16)
        .map(char::from)
        .collect()
}

/// Reads the body once, extracts the JSON-RPC method(s) and id, and puts the
/// body back so the handler can forward it.
pub async fn extract_rpc_method(req: Request<Body>, next: Next) -> Response {
    let (mut parts, body) = req.into_parts();
    let body_bytes = match timeout(BODY_READ_TIMEOUT, to_bytes(body, MAX_BODY_SIZE)).await {
        Ok(Ok(bytes)) => bytes,
        Ok(Err(_)) => {
            return rpc::error_response(
                StatusCode::PAYLOAD_TOO_LARGE,
                codes::INVALID_REQUEST,
                "Request body too large",
                None,
            );
        }
        Err(_) => {
            return rpc::error_response(
                StatusCode::REQUEST_TIMEOUT,
                codes::INVALID_REQUEST,
                "Timed out reading request body",
                None,
            );
        }
    };

    if let Some(info) = rpc::probe_request(&body_bytes) {
        parts.extensions.insert(RpcMethod(info.method.clone()));
        parts.extensions.insert(info);
    }

    next.run(Request::from_parts(parts, Body::from(body_bytes)))
        .await
}

pub async fn log_requests(
    addr: Option<ConnectInfo<SocketAddr>>,
    req: Request<Body>,
    next: Next,
) -> Response {
    let addr = addr
        .map(|ConnectInfo(a)| a.to_string())
        .unwrap_or_else(|| "-".to_string());
    let method = req.method().clone();
    let path = req.uri().path().to_string();
    let rpc_method = req
        .extensions()
        .get::<RpcMethod>()
        .map(|m| m.0.clone())
        .unwrap_or_else(|| "-".to_string());
    let request_id = req
        .extensions()
        .get::<RequestId>()
        .map(|r| r.0.clone())
        .unwrap_or_else(|| "-".to_string());

    let start = std::time::Instant::now();
    let response = next.run(req).await;
    let duration = start.elapsed();

    let backend = response
        .extensions()
        .get::<SelectedBackend>()
        .map(|b| b.0.as_str())
        .unwrap_or("-");
    let owner = response
        .extensions()
        .get::<ClientOwner>()
        .map(|o| o.0.as_str())
        .unwrap_or("-");
    let attempts = response
        .extensions()
        .get::<Attempts>()
        .map(|a| a.0)
        .unwrap_or(1);

    if path == HEALTH_PATH {
        // Liveness probes every few seconds would drown real traffic.
        tracing::debug!(
            "{} {} {} status={} {:.1?}",
            method,
            path,
            addr,
            response.status().as_u16(),
            duration
        );
    } else {
        info!(
            "{} {} {} status={} {:.1?} rpc_method={} backend={} owner={} attempts={} request_id={}",
            method,
            path,
            addr,
            response.status().as_u16(),
            duration,
            rpc_method,
            backend,
            owner,
            attempts,
            request_id
        );
    }

    response
}

pub async fn track_metrics(req: Request<Body>, next: Next) -> Response {
    if req.uri().path() == HEALTH_PATH {
        return next.run(req).await;
    }
    let start = std::time::Instant::now();
    let method = req.method().to_string();

    let rpc_method = req
        .extensions()
        .get::<RpcMethod>()
        .map(|m| rpc::metric_label(&m.0).to_string())
        .unwrap_or_else(|| rpc::OTHER_METHOD.to_string());

    let response = next.run(req).await;

    let duration = start.elapsed().as_secs_f64();
    let status = response.status().as_u16().to_string();

    let backend = response
        .extensions()
        .get::<SelectedBackend>()
        .map(|b| b.0.clone())
        .unwrap_or_else(|| "none".to_string());

    let owner = response
        .extensions()
        .get::<ClientOwner>()
        .map(|o| o.0.clone())
        .unwrap_or_else(|| "none".to_string());

    histogram!("rpc_request_duration_seconds", "rpc_method" => rpc_method.clone(), "backend" => backend.clone(), "owner" => owner.clone()).record(duration);
    counter!("rpc_requests_total", "method" => method, "status" => status, "rpc_method" => rpc_method, "backend" => backend, "owner" => owner).increment(1);

    response
}

// ---------------------------------------------------------------------------
// Authentication
// ---------------------------------------------------------------------------

/// Finds the router API key. Precedence: `?api-key=` query parameter,
/// `x-api-key` header, `Authorization: Bearer <key>` header.
pub fn extract_api_key(headers: &HeaderMap, params: &Params) -> Option<String> {
    if let Some(k) = &params.api_key {
        if !k.is_empty() {
            return Some(k.clone());
        }
    }
    if let Some(k) = headers
        .get(API_KEY_HEADER)
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        return Some(k.to_string());
    }
    headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| {
            let (scheme, rest) = v.split_once(' ')?;
            if scheme.eq_ignore_ascii_case("bearer") {
                let k = rest.trim();
                (!k.is_empty()).then(|| k.to_string())
            } else {
                None
            }
        })
}

fn key_prefix(key: &str) -> &str {
    let end = key
        .char_indices()
        .nth(6)
        .map(|(i, _)| i)
        .unwrap_or(key.len());
    &key[..end]
}

pub enum AuthFailure {
    Missing,
    Invalid,
    RateLimited,
    Internal(String),
}

/// Validates the key with the keystore and returns the owner on success.
pub async fn authenticate(
    state: &AppState,
    headers: &HeaderMap,
    params: &Params,
) -> Result<String, AuthFailure> {
    let api_key = extract_api_key(headers, params).ok_or(AuthFailure::Missing)?;

    // Cheap rejection before any keystore round trip.
    if api_key.len() > MAX_KEY_LEN {
        info!("Rejected API key longer than {} bytes", MAX_KEY_LEN);
        return Err(AuthFailure::Invalid);
    }

    match state.keystore.validate_key(&api_key).await {
        Ok(Some(info)) => Ok(info.owner),
        Ok(None) => {
            info!(
                "Invalid API key presented (prefix={}...)",
                key_prefix(&api_key)
            );
            Err(AuthFailure::Invalid)
        }
        Err(KeyStoreError::RateLimited) => {
            warn!("API key rate limited (prefix={}...)", key_prefix(&api_key));
            Err(AuthFailure::RateLimited)
        }
        Err(KeyStoreError::Backend(e)) => {
            error!("Key validation error: {}", e);
            Err(AuthFailure::Internal(e))
        }
    }
}

impl AuthFailure {
    fn into_response(self, id: Option<&Value>) -> Response {
        match self {
            AuthFailure::Missing => rpc::error_response(
                StatusCode::UNAUTHORIZED,
                codes::UNAUTHORIZED,
                "Missing API key: pass ?api-key=, x-api-key header or Authorization: Bearer",
                id,
            ),
            AuthFailure::Invalid => rpc::error_response(
                StatusCode::UNAUTHORIZED,
                codes::UNAUTHORIZED,
                "Invalid or inactive API key",
                id,
            ),
            AuthFailure::RateLimited => rpc::error_response(
                StatusCode::TOO_MANY_REQUESTS,
                codes::RATE_LIMITED,
                "Rate limit exceeded",
                id,
            ),
            AuthFailure::Internal(_) => rpc::error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                codes::INTERNAL,
                "Internal server error",
                id,
            ),
        }
    }

    fn metric_status(&self) -> &'static str {
        match self {
            AuthFailure::Missing | AuthFailure::Invalid => "auth_failed",
            AuthFailure::RateLimited => "rate_limited",
            AuthFailure::Internal(_) => "error",
        }
    }
}

/// Middleware: authenticates the request and stores the owner in request
/// extensions. Runs before any body is read, so bad or rate-limited keys cost
/// nothing beyond the key lookup.
pub async fn require_api_key(
    State(state): State<Arc<AppState>>,
    params: Option<Query<Params>>,
    mut req: Request<Body>,
    next: Next,
) -> Response {
    // A malformed query string (duplicate api-key, bad percent-encoding) is
    // treated as "no key in the query"; header auth still applies.
    let params = params.map(|Query(p)| p).unwrap_or(Params { api_key: None });
    let is_ws = req
        .headers()
        .get(header::UPGRADE)
        .and_then(|v| v.to_str().ok())
        .map(|v| v.eq_ignore_ascii_case("websocket"))
        .unwrap_or(false);

    match authenticate(&state, req.headers(), &params).await {
        Ok(owner) => {
            req.extensions_mut().insert(ClientOwner(owner.clone()));
            let mut resp = next.run(req).await;
            if resp.extensions().get::<ClientOwner>().is_none() {
                resp.extensions_mut().insert(ClientOwner(owner));
            }
            resp
        }
        Err(failure) => {
            if is_ws {
                counter!("ws_connections_total", "backend" => "none", "owner" => "none", "status" => failure.metric_status()).increment(1);
            }
            failure.into_response(None)
        }
    }
}

// ---------------------------------------------------------------------------
// HTTP proxy
// ---------------------------------------------------------------------------

enum AttemptError {
    Timeout,
    Transport(String),
}

async fn send_once(
    client: &HttpClient,
    req: Request<Body>,
    timeout_secs: u64,
) -> Result<hyper::Response<hyper::body::Incoming>, AttemptError> {
    match timeout(Duration::from_secs(timeout_secs), client.request(req)).await {
        Ok(Ok(resp)) => Ok(resp),
        Ok(Err(e)) => Err(AttemptError::Transport(e.to_string())),
        Err(_) => Err(AttemptError::Timeout),
    }
}

fn tag_response(mut resp: Response, backend: &str, owner: &str, attempts: u32) -> Response {
    upstream::sanitize_response_headers(resp.headers_mut());
    resp.extensions_mut()
        .insert(SelectedBackend(backend.to_string()));
    resp.extensions_mut().insert(ClientOwner(owner.to_string()));
    resp.extensions_mut().insert(Attempts(attempts));
    if let Ok(v) = HeaderValue::from_str(backend) {
        resp.headers_mut().insert(BACKEND_HEADER, v);
    }
    resp.headers_mut()
        .insert(ATTEMPTS_HEADER, HeaderValue::from(attempts));
    resp
}

pub async fn proxy(State(state): State<Arc<AppState>>, req: Request<Body>) -> Response {
    let (parts, body) = req.into_parts();
    let rpc_info = parts.extensions.get::<RpcRequestInfo>().cloned();
    let rpc_id = rpc_info.as_ref().map(|i| &i.id);
    let rpc_method = rpc_info.as_ref().map(|i| i.method.as_str());

    // Set by `require_api_key`; absent only if the router was assembled
    // without it, which we treat as a configuration error, not open access.
    let owner = match parts.extensions.get::<ClientOwner>() {
        Some(o) => o.0.clone(),
        None => return AuthFailure::Missing.into_response(rpc_id),
    };

    let rs = state.state.load();

    // Blocked methods never reach a backend.
    if let Some(info) = &rpc_info {
        if let Some(blocked) = info.methods.iter().find(|m| rs.is_blocked(m)) {
            counter!("rpc_blocked_total", "rpc_method" => rpc::metric_label(blocked).to_string(), "owner" => owner.clone()).increment(1);
            let resp = rpc::error_response(
                StatusCode::FORBIDDEN,
                codes::METHOD_BLOCKED,
                format!("Method '{}' is not available on this endpoint", blocked),
                rpc_id,
            );
            return tag_response(resp, "none", &owner, 0);
        }
    }

    let body_bytes = match to_bytes(body, MAX_BODY_SIZE).await {
        Ok(b) => b,
        Err(_) => {
            let resp = rpc::error_response(
                StatusCode::PAYLOAD_TOO_LARGE,
                codes::INVALID_REQUEST,
                "Request body too large",
                rpc_id,
            );
            return tag_response(resp, "none", &owner, 0);
        }
    };

    let path = parts.uri.path().to_string();
    let query = parts.uri.query().map(str::to_string);
    let timeout_secs = rs.proxy.timeout_secs;

    // Fan-out path: broadcast to every healthy backend, first good answer wins.
    if let Some(method) = rpc_method.filter(|m| rs.is_fanout(m)) {
        let targets = rs.healthy_backends();
        if targets.len() > 1 {
            return fanout(
                &state.client,
                targets,
                &parts.headers,
                &path,
                query.as_deref(),
                body_bytes,
                timeout_secs,
                method,
                &owner,
                rpc_id,
            )
            .await;
        }
    }

    // Normal path: one backend at a time, failing over on transport errors,
    // timeouts and retryable HTTP statuses.
    let max_attempts = rs.proxy.max_retries.saturating_add(1);
    let mut tried: Vec<String> = Vec::with_capacity(max_attempts as usize);
    let mut last_error: Option<(String, AttemptError)> = None;

    for attempt in 1..=max_attempts {
        let selection = match rs.select_backend(rpc_method, &tried) {
            Some(s) => s,
            None => break,
        };

        let uri = match upstream::build_uri(&selection.url, &path, query.as_deref()) {
            Ok(u) => u,
            Err(e) => {
                error!("backend={} {}", selection.label, e);
                let resp = rpc::error_response(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    codes::INTERNAL,
                    "Invalid backend configuration",
                    rpc_id,
                );
                return tag_response(resp, &selection.label, &owner, attempt);
            }
        };

        let headers = upstream::forwardable_headers(&parts.headers, &uri);
        let req = upstream::build_request(uri, &headers, body_bytes.clone());
        tried.push(selection.label.clone());

        let has_more = attempt < max_attempts;
        let started = std::time::Instant::now();
        let result = send_once(&state.client, req, timeout_secs).await;
        rs.record_latency(&selection.label, started.elapsed());
        match result {
            Ok(resp) => {
                let status = resp.status();
                if upstream::is_retryable_status(status) && has_more {
                    counter!("rpc_upstream_attempts_total", "backend" => selection.label.clone(), "outcome" => "retryable_status").increment(1);
                    warn!(
                        "backend={} returned {} for {}, retrying on another backend (attempt {}/{})",
                        selection.label,
                        status.as_u16(),
                        rpc_method.unwrap_or("-"),
                        attempt,
                        max_attempts
                    );
                    continue;
                }
                let outcome = if status.is_success() {
                    "ok"
                } else {
                    "upstream_status"
                };
                counter!("rpc_upstream_attempts_total", "backend" => selection.label.clone(), "outcome" => outcome).increment(1);
                if attempt > 1 {
                    counter!("rpc_failovers_total", "rpc_method" => rpc::metric_label(rpc_method.unwrap_or("")).to_string()).increment(1);
                }
                return tag_response(resp.into_response(), &selection.label, &owner, attempt);
            }
            Err(e) => {
                let outcome = match &e {
                    AttemptError::Timeout => "timeout",
                    AttemptError::Transport(_) => "transport_error",
                };
                counter!("rpc_upstream_attempts_total", "backend" => selection.label.clone(), "outcome" => outcome).increment(1);
                match &e {
                    AttemptError::Timeout => warn!(
                        "backend={} timed out after {}s (attempt {}/{})",
                        selection.label, timeout_secs, attempt, max_attempts
                    ),
                    AttemptError::Transport(msg) => warn!(
                        "backend={} request failed: {} (attempt {}/{})",
                        selection.label, msg, attempt, max_attempts
                    ),
                }
                last_error = Some((selection.label, e));
            }
        }
    }

    let attempts = tried.len() as u32;
    match last_error {
        None if attempts == 0 && rs.any_healthy() => {
            // Everything healthy is out of upstream budget right now.
            counter!("rpc_backends_at_capacity_total", "rpc_method" => rpc::metric_label(rpc_method.unwrap_or("")).to_string()).increment(1);
            let resp = rpc::error_response(
                StatusCode::TOO_MANY_REQUESTS,
                codes::BACKENDS_AT_CAPACITY,
                "All backends are at their configured max_rps; retry shortly",
                rpc_id,
            );
            tag_response(resp, "none", &owner, attempts)
        }
        None => {
            error!("No healthy backends available for request");
            let resp = rpc::error_response(
                StatusCode::SERVICE_UNAVAILABLE,
                codes::NO_BACKEND,
                "No healthy backends available",
                rpc_id,
            );
            tag_response(resp, "none", &owner, attempts)
        }
        Some((label, AttemptError::Timeout)) => {
            let resp = rpc::error_response(
                StatusCode::GATEWAY_TIMEOUT,
                codes::UPSTREAM_TIMEOUT,
                format!(
                    "Upstream request timed out after {}s ({} attempt(s))",
                    timeout_secs, attempts
                ),
                rpc_id,
            );
            tag_response(resp, &label, &owner, attempts)
        }
        Some((label, AttemptError::Transport(msg))) => {
            let resp = rpc::error_response(
                StatusCode::BAD_GATEWAY,
                codes::UPSTREAM_ERROR,
                format!("Upstream request failed: {} ({} attempt(s))", msg, attempts),
                rpc_id,
            );
            tag_response(resp, &label, &owner, attempts)
        }
    }
}

enum FanoutOutcome {
    /// 2xx with a JSON body that has no `error` member.
    Good(StatusCode, Option<HeaderValue>, Bytes),
    /// Anything else we got an HTTP response for.
    Bad(StatusCode, Option<HeaderValue>, Bytes),
    Failed(String),
}

#[allow(clippy::too_many_arguments)]
async fn fanout(
    client: &HttpClient,
    targets: Vec<Selection>,
    client_headers: &HeaderMap,
    path: &str,
    query: Option<&str>,
    body: Bytes,
    timeout_secs: u64,
    method: &str,
    owner: &str,
    rpc_id: Option<&Value>,
) -> Response {
    let total = targets.len() as u32;
    let (tx, mut rx) = mpsc::channel::<(String, FanoutOutcome)>(targets.len());

    for selection in targets {
        let uri = match upstream::build_uri(&selection.url, path, query) {
            Ok(u) => u,
            Err(e) => {
                error!("backend={} {}", selection.label, e);
                continue;
            }
        };
        let mut headers = upstream::forwardable_headers(client_headers, &uri);
        // We must inspect the body to pick a winner, so ask for identity
        // encoding; otherwise a gzip reply would look like a parse failure.
        headers.remove(header::ACCEPT_ENCODING);
        let req = upstream::build_request(uri, &headers, body.clone());
        let client = client.clone();
        let tx = tx.clone();

        // Spawned (not merely polled) so every backend still receives the
        // transaction even after we've already answered the client.
        tokio::spawn(async move {
            let outcome = match send_once(&client, req, timeout_secs).await {
                Ok(resp) => {
                    let status = resp.status();
                    let content_type = resp.headers().get(header::CONTENT_TYPE).cloned();
                    let bytes = match http_body_util::Limited::new(
                        resp.into_body(),
                        MAX_FANOUT_RESPONSE_SIZE,
                    )
                    .collect()
                    .await
                    {
                        Ok(c) => c.to_bytes(),
                        Err(e) => {
                            let _ = tx
                                .send((
                                    selection.label.clone(),
                                    FanoutOutcome::Failed(format!("body read failed: {}", e)),
                                ))
                                .await;
                            return;
                        }
                    };
                    let is_rpc_error = serde_json::from_slice::<Value>(&bytes)
                        .map(|v| v.get("error").is_some())
                        .unwrap_or(true);
                    if status.is_success() && !is_rpc_error {
                        FanoutOutcome::Good(status, content_type, bytes)
                    } else {
                        FanoutOutcome::Bad(status, content_type, bytes)
                    }
                }
                Err(AttemptError::Timeout) => {
                    FanoutOutcome::Failed(format!("timed out after {}s", timeout_secs))
                }
                Err(AttemptError::Transport(msg)) => FanoutOutcome::Failed(msg),
            };
            let _ = tx.send((selection.label, outcome)).await;
        });
    }
    drop(tx);

    let method_label = rpc::metric_label(method).to_string();
    let mut first_bad: Option<(String, StatusCode, Option<HeaderValue>, Bytes)> = None;
    let mut first_failure: Option<(String, String)> = None;
    let mut received = 0u32;

    while let Some((label, outcome)) = rx.recv().await {
        received += 1;
        match outcome {
            FanoutOutcome::Good(status, content_type, bytes) => {
                counter!("rpc_fanout_total", "rpc_method" => method_label.clone(), "outcome" => "ok").increment(1);
                info!(
                    "fanout {} answered by backend={} ({}/{} responses in)",
                    method, label, received, total
                );
                let resp = bytes_response(status, content_type, bytes);
                return tag_response(resp, &label, owner, received);
            }
            FanoutOutcome::Bad(status, content_type, bytes) => {
                warn!(
                    "fanout {} backend={} returned {} with an error body",
                    method,
                    label,
                    status.as_u16()
                );
                first_bad.get_or_insert((label, status, content_type, bytes));
            }
            FanoutOutcome::Failed(msg) => {
                warn!("fanout {} backend={} failed: {}", method, label, msg);
                first_failure.get_or_insert((label, msg));
            }
        }
    }

    counter!("rpc_fanout_total", "rpc_method" => method_label, "outcome" => "failed").increment(1);
    if let Some((label, status, content_type, bytes)) = first_bad {
        // Surface the real upstream error (e.g. blockhash not found) rather
        // than hiding it behind a generic proxy message.
        return tag_response(
            bytes_response(status, content_type, bytes),
            &label,
            owner,
            received,
        );
    }
    let (label, msg) = first_failure.unwrap_or_else(|| ("none".to_string(), "no backends".into()));
    let resp = rpc::error_response(
        StatusCode::BAD_GATEWAY,
        codes::UPSTREAM_ERROR,
        format!("All {} backends failed: {}", total, msg),
        rpc_id,
    );
    tag_response(resp, &label, owner, received)
}

fn bytes_response(status: StatusCode, content_type: Option<HeaderValue>, bytes: Bytes) -> Response {
    let mut resp = (status, bytes).into_response();
    resp.headers_mut().insert(
        header::CONTENT_TYPE,
        content_type.unwrap_or_else(|| HeaderValue::from_static("application/json")),
    );
    resp
}

// ---------------------------------------------------------------------------
// Health endpoint
// ---------------------------------------------------------------------------

#[derive(Serialize)]
pub struct HealthResponse {
    pub overall_status: String,
    pub version: &'static str,
    pub healthy_backends: usize,
    pub total_backends: usize,
    pub backends: Vec<BackendHealth>,
}

#[derive(Serialize)]
pub struct BackendHealth {
    pub label: String,
    pub healthy: bool,
    pub slot: Option<u64>,
    /// Round-trip of the last health probe.
    pub latency_ms: Option<u64>,
    /// EWMA of real request latency through this backend, if any.
    pub request_latency_ms: Option<f64>,
    /// Upstream budget left this second, if `max_rps` is set.
    pub has_capacity: bool,
    pub last_check_unix: Option<u64>,
    pub last_check_age_secs: Option<u64>,
    pub consecutive_failures: u32,
    pub consecutive_successes: u32,
    pub last_error: Option<String>,
}

pub async fn health_endpoint(State(state): State<Arc<AppState>>) -> Response {
    let current_state = state.state.load();
    let all_statuses = current_state.health_state.get_all_statuses();
    let now = SystemTime::now();

    let mut backends = Vec::with_capacity(current_state.backends.len());
    let mut healthy_count = 0;

    for backend in &current_state.backends {
        let status = all_statuses
            .get(&backend.config.label)
            .cloned()
            .unwrap_or_default();

        let healthy = status.healthy;
        if healthy {
            healthy_count += 1;
        }

        let last_check_unix = status
            .last_check_time
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map(|d| d.as_secs());
        let last_check_age_secs = status
            .last_check_time
            .and_then(|t| now.duration_since(t).ok())
            .map(|d| d.as_secs());

        backends.push(BackendHealth {
            label: backend.config.label.clone(),
            healthy,
            slot: status.slot,
            latency_ms: status.latency_ms,
            request_latency_ms: backend
                .latency()
                .map(|d| (d.as_secs_f64() * 1000.0 * 10.0).round() / 10.0),
            has_capacity: backend.has_capacity(),
            last_check_unix,
            last_check_age_secs,
            consecutive_failures: status.consecutive_failures,
            consecutive_successes: status.consecutive_successes,
            last_error: status.last_error,
        });
    }

    let overall_status = if healthy_count > 0 {
        "healthy"
    } else {
        "unhealthy"
    };
    let http_status = if healthy_count > 0 {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };

    let body = HealthResponse {
        overall_status: overall_status.to_string(),
        version: env!("CARGO_PKG_VERSION"),
        healthy_backends: healthy_count,
        total_backends: current_state.backends.len(),
        backends,
    };

    (http_status, Json(body)).into_response()
}

// ---------------------------------------------------------------------------
// WebSocket proxy
// ---------------------------------------------------------------------------

pub async fn ws_proxy(
    ws: WebSocketUpgrade,
    State(state): State<Arc<AppState>>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    req: Request<Body>,
) -> Response {
    let owner = match req.extensions().get::<ClientOwner>() {
        Some(o) => o.0.clone(),
        None => {
            counter!("ws_connections_total", "backend" => "none", "owner" => "none", "status" => "auth_failed").increment(1);
            return AuthFailure::Missing.into_response(None);
        }
    };

    let limit = state.state.load().proxy.max_ws_connections_per_key;
    let session = match state.try_open_ws_session(&owner, limit) {
        Some(guard) => guard,
        None => {
            warn!(
                "WebSocket: owner={} hit the per-key connection cap ({})",
                owner, limit
            );
            counter!("ws_connections_total", "backend" => "none", "owner" => owner.clone(), "status" => "connection_limit").increment(1);
            return rpc::error_response(
                StatusCode::TOO_MANY_REQUESTS,
                codes::RATE_LIMITED,
                format!(
                    "Too many open WebSocket connections for this key (limit {})",
                    limit
                ),
                None,
            );
        }
    };

    let (backend_label, backend_ws_url) = match state.state.load().select_ws_backend() {
        Some(s) => (s.label, s.url),
        None => {
            error!("No healthy WebSocket backends available");
            counter!("ws_connections_total", "backend" => "none", "owner" => owner.clone(), "status" => "no_backend").increment(1);
            return rpc::error_response(
                StatusCode::SERVICE_UNAVAILABLE,
                codes::NO_BACKEND,
                "No healthy WebSocket backends available",
                None,
            );
        }
    };

    info!(
        "WebSocket: {} upgrading connection, backend={}, owner={}",
        addr, backend_label, owner
    );

    ws.on_upgrade(move |client_socket| async move {
        // Keep the slot until the session ends.
        let _session = session;
        handle_ws_connection(client_socket, backend_ws_url, backend_label, owner, addr).await
    })
}

async fn handle_ws_connection(
    client_socket: WebSocket,
    backend_url: String,
    backend_label: String,
    owner: String,
    client_addr: SocketAddr,
) {
    let backend_socket = match timeout(WS_CONNECT_TIMEOUT, connect_async(&backend_url)).await {
        Ok(Ok((socket, _))) => socket,
        Ok(Err(e)) => {
            error!(
                "WebSocket: Failed to connect to backend {}: {}",
                backend_label, e
            );
            counter!("ws_connections_total", "backend" => backend_label, "owner" => owner, "status" => "backend_connect_failed").increment(1);
            let _ = close_client(client_socket, "backend unavailable").await;
            return;
        }
        Err(_) => {
            error!(
                "WebSocket: Timed out connecting to backend {} after {:?}",
                backend_label, WS_CONNECT_TIMEOUT
            );
            counter!("ws_connections_total", "backend" => backend_label, "owner" => owner, "status" => "backend_connect_timeout").increment(1);
            let _ = close_client(client_socket, "backend connect timeout").await;
            return;
        }
    };

    counter!("ws_connections_total", "backend" => backend_label.clone(), "owner" => owner.clone(), "status" => "connected").increment(1);
    gauge!("ws_active_connections", "backend" => backend_label.clone(), "owner" => owner.clone())
        .increment(1.0);
    let connect_time = std::time::Instant::now();

    info!(
        "WebSocket: {} connected to backend {}",
        client_addr, backend_label
    );

    let (mut client_write, mut client_read) = client_socket.split();
    let (mut backend_write, mut backend_read) = backend_socket.split();

    let bl1 = backend_label.clone();
    let ow1 = owner.clone();
    let bl2 = backend_label.clone();
    let ow2 = owner.clone();

    let client_to_backend = async {
        while let Some(msg) = client_read.next().await {
            let forward = match msg {
                Ok(Message::Text(text)) => {
                    counter!("ws_messages_total", "backend" => bl1.clone(), "owner" => ow1.clone(), "direction" => "client_to_backend").increment(1);
                    TungsteniteMessage::Text(text)
                }
                Ok(Message::Binary(data)) => {
                    counter!("ws_messages_total", "backend" => bl1.clone(), "owner" => ow1.clone(), "direction" => "client_to_backend").increment(1);
                    TungsteniteMessage::Binary(data)
                }
                Ok(Message::Ping(data)) => TungsteniteMessage::Ping(data),
                Ok(Message::Pong(data)) => TungsteniteMessage::Pong(data),
                Ok(Message::Close(_)) | Err(_) => break,
            };
            if backend_write.send(forward).await.is_err() {
                break;
            }
        }
    };

    let backend_to_client = async {
        while let Some(msg) = backend_read.next().await {
            let forward = match msg {
                Ok(TungsteniteMessage::Text(text)) => {
                    counter!("ws_messages_total", "backend" => bl2.clone(), "owner" => ow2.clone(), "direction" => "backend_to_client").increment(1);
                    Message::Text(text)
                }
                Ok(TungsteniteMessage::Binary(data)) => {
                    counter!("ws_messages_total", "backend" => bl2.clone(), "owner" => ow2.clone(), "direction" => "backend_to_client").increment(1);
                    Message::Binary(data)
                }
                Ok(TungsteniteMessage::Ping(data)) => Message::Ping(data),
                Ok(TungsteniteMessage::Pong(data)) => Message::Pong(data),
                Ok(TungsteniteMessage::Close(_)) | Ok(TungsteniteMessage::Frame(_)) | Err(_) => {
                    break
                }
            };
            if client_write.send(forward).await.is_err() {
                break;
            }
        }
    };

    tokio::select! {
        _ = client_to_backend => {
            let _ = backend_write.send(TungsteniteMessage::Close(None)).await;
        },
        _ = backend_to_client => {
            let _ = client_write.send(Message::Close(None)).await;
        },
    }

    let duration = connect_time.elapsed().as_secs_f64();
    gauge!("ws_active_connections", "backend" => backend_label.clone(), "owner" => owner.clone())
        .decrement(1.0);
    histogram!("ws_connection_duration_seconds", "backend" => backend_label.clone(), "owner" => owner.clone()).record(duration);

    info!(
        "WebSocket: {} disconnected from backend {} (duration={:.1}s)",
        client_addr, backend_label, duration
    );
}

async fn close_client(mut socket: WebSocket, reason: &'static str) -> Result<(), axum::Error> {
    use axum::extract::ws::{close_code, CloseFrame};
    socket
        .send(Message::Close(Some(CloseFrame {
            code: close_code::AGAIN,
            reason: reason.into(),
        })))
        .await
}
