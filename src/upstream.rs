//! Building upstream requests and deciding when a failed attempt is retryable.

use axum::{
    body::Body,
    http::{header, HeaderMap, HeaderName, HeaderValue, Method, Request, StatusCode, Uri},
};
use bytes::Bytes;

/// Headers that carry the client's credentials for *this* router and must never
/// be forwarded to a backend.
pub const CREDENTIAL_HEADERS: [HeaderName; 2] =
    [HeaderName::from_static("x-api-key"), header::AUTHORIZATION];

/// Hop-by-hop headers (RFC 7230 §6.1) that must not be forwarded by a proxy.
const HOP_BY_HOP_HEADERS: [HeaderName; 8] = [
    header::CONNECTION,
    header::PROXY_AUTHENTICATE,
    header::PROXY_AUTHORIZATION,
    header::TE,
    header::TRAILER,
    header::TRANSFER_ENCODING,
    header::UPGRADE,
    HeaderName::from_static("keep-alive"),
];

/// Query parameter that carries the router API key.
pub const API_KEY_PARAM: &str = "api-key";

/// Builds the upstream URI for a backend. Handles backend URLs that already
/// carry a path and/or query string (e.g. `https://host/?api-key=...`), joins
/// the client's sub-path, and merges any remaining client query parameters.
pub fn build_uri(backend_url: &str, path: &str, query: Option<&str>) -> Result<Uri, String> {
    let base: Uri = backend_url
        .parse()
        .map_err(|e| format!("invalid backend url '{}': {}", redact_url(backend_url), e))?;

    let scheme = base
        .scheme_str()
        .ok_or_else(|| format!("backend url '{}' has no scheme", redact_url(backend_url)))?;
    let authority = base
        .authority()
        .ok_or_else(|| format!("backend url '{}' has no host", redact_url(backend_url)))?;

    let base_path = base.path();
    let sub_path = path.trim_start_matches('/');
    let mut full_path = if sub_path.is_empty() {
        base_path.to_string()
    } else if base_path.ends_with('/') {
        format!("{}{}", base_path, sub_path)
    } else {
        format!("{}/{}", base_path, sub_path)
    };
    if full_path.is_empty() {
        full_path.push('/');
    }

    let client_query = query.map(strip_api_key).unwrap_or_default();
    let merged_query = match (base.query(), client_query.is_empty()) {
        (Some(bq), true) => bq.to_string(),
        (Some(bq), false) => format!("{}&{}", bq, client_query),
        (None, true) => String::new(),
        (None, false) => client_query,
    };

    let path_and_query = if merged_query.is_empty() {
        full_path
    } else {
        format!("{}?{}", full_path, merged_query)
    };

    Uri::builder()
        .scheme(scheme)
        .authority(authority.as_str())
        .path_and_query(path_and_query)
        .build()
        .map_err(|e| format!("failed to build upstream uri: {}", e))
}

/// Renders a backend URL for logs and errors with its query string hidden,
/// since provider URLs commonly carry credentials (`?api-key=...`).
pub fn redact_url(url: &str) -> String {
    match url.split_once('?') {
        Some((base, _)) => format!("{}?<redacted>", base),
        None => url.to_string(),
    }
}

/// Removes the `api-key` parameter from a query string, keeping everything else.
pub fn strip_api_key(query: &str) -> String {
    query
        .split('&')
        .filter(|p| !p.is_empty())
        .filter(|p| {
            let key = p.split('=').next().unwrap_or("");
            key != API_KEY_PARAM
        })
        .collect::<Vec<_>>()
        .join("&")
}

/// Copies client headers that are safe to forward and sets `host` for the
/// backend. Credentials for this router and hop-by-hop headers are dropped.
pub fn forwardable_headers(client_headers: &HeaderMap, uri: &Uri) -> HeaderMap {
    let mut out = HeaderMap::with_capacity(client_headers.len() + 1);
    for (name, value) in client_headers {
        if name == header::HOST
            || name == header::CONTENT_LENGTH
            || CREDENTIAL_HEADERS.contains(name)
            || HOP_BY_HOP_HEADERS.contains(name)
        {
            continue;
        }
        out.append(name.clone(), value.clone());
    }
    if let Some(host) = uri.host() {
        let host_value = match uri.port_u16() {
            Some(port) => format!("{}:{}", host, port),
            None => host.to_string(),
        };
        if let Ok(v) = HeaderValue::from_str(&host_value) {
            out.insert(header::HOST, v);
        }
    }
    out
}

/// Removes headers from an upstream response that must not reach the client:
/// hop-by-hop headers (hyper re-frames the body itself) and `set-cookie`,
/// which would leak provider/CDN session state across backends.
pub fn sanitize_response_headers(headers: &mut HeaderMap) {
    for name in HOP_BY_HOP_HEADERS.iter() {
        headers.remove(name);
    }
    headers.remove(header::SET_COOKIE);
}

/// Builds a fresh POST request for one upstream attempt.
pub fn build_request(uri: Uri, headers: &HeaderMap, body: Bytes) -> Request<Body> {
    let mut req = Request::builder()
        .method(Method::POST)
        .uri(uri)
        .body(Body::from(body))
        .expect("valid request parts");
    *req.headers_mut() = headers.clone();
    req
}

/// Whether an upstream HTTP status means "try another backend".
pub fn is_retryable_status(status: StatusCode) -> bool {
    status == StatusCode::REQUEST_TIMEOUT
        || status == StatusCode::TOO_MANY_REQUESTS
        || status.is_server_error()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn root_path_plain_backend() {
        let uri = build_uri("https://api.mainnet-beta.solana.com", "/", None).unwrap();
        assert_eq!(uri.to_string(), "https://api.mainnet-beta.solana.com/");
    }

    #[test]
    fn root_path_backend_with_trailing_slash() {
        let uri = build_uri("http://127.0.0.1:8899/", "/", None).unwrap();
        assert_eq!(uri.to_string(), "http://127.0.0.1:8899/");
    }

    #[test]
    fn backend_with_query_keeps_its_key() {
        let uri = build_uri("https://rpc.example.com/?api-key=UP", "/", None).unwrap();
        assert_eq!(uri.to_string(), "https://rpc.example.com/?api-key=UP");
    }

    #[test]
    fn client_query_is_merged_and_router_key_stripped() {
        let uri = build_uri(
            "https://rpc.example.com/?api-key=UP",
            "/",
            Some("api-key=ROUTER&foo=bar"),
        )
        .unwrap();
        assert_eq!(
            uri.to_string(),
            "https://rpc.example.com/?api-key=UP&foo=bar"
        );
    }

    #[test]
    fn sub_path_is_joined() {
        let uri = build_uri("https://rpc.example.com/v1", "/extra", Some("api-key=x")).unwrap();
        assert_eq!(uri.to_string(), "https://rpc.example.com/v1/extra");
        let uri = build_uri("https://rpc.example.com/v1/", "extra/", None).unwrap();
        assert_eq!(uri.to_string(), "https://rpc.example.com/v1/extra/");
    }

    #[test]
    fn strip_api_key_only_removes_exact_param() {
        assert_eq!(strip_api_key("api-key=a&x=1"), "x=1");
        assert_eq!(strip_api_key("x=1&api-key=a"), "x=1");
        assert_eq!(strip_api_key("api-key=a"), "");
        assert_eq!(strip_api_key("api-keys=a"), "api-keys=a");
    }

    #[test]
    fn rejects_bad_backend_url() {
        assert!(build_uri("not a url", "/", None).is_err());
        assert!(build_uri("/relative", "/", None).is_err());
    }

    #[test]
    fn forwardable_headers_drop_credentials_and_set_host() {
        let mut h = HeaderMap::new();
        h.insert("x-api-key", HeaderValue::from_static("secret"));
        h.insert(header::AUTHORIZATION, HeaderValue::from_static("Bearer s"));
        h.insert(header::HOST, HeaderValue::from_static("router.local"));
        h.insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/json"),
        );
        h.insert(header::CONNECTION, HeaderValue::from_static("keep-alive"));
        h.insert("x-request-id", HeaderValue::from_static("abc"));

        let uri: Uri = "https://rpc.example.com:8443/".parse().unwrap();
        let out = forwardable_headers(&h, &uri);
        assert!(out.get("x-api-key").is_none());
        assert!(out.get(header::AUTHORIZATION).is_none());
        assert!(out.get(header::CONNECTION).is_none());
        assert_eq!(out.get(header::HOST).unwrap(), "rpc.example.com:8443");
        assert_eq!(out.get(header::CONTENT_TYPE).unwrap(), "application/json");
        assert_eq!(out.get("x-request-id").unwrap(), "abc");
    }

    #[test]
    fn sanitize_response_strips_cookies_and_hop_by_hop() {
        let mut h = HeaderMap::new();
        h.append(header::SET_COOKIE, HeaderValue::from_static("__cf_bm=abc"));
        h.append(header::SET_COOKIE, HeaderValue::from_static("other=1"));
        h.insert(
            header::TRANSFER_ENCODING,
            HeaderValue::from_static("chunked"),
        );
        h.insert(header::CONNECTION, HeaderValue::from_static("keep-alive"));
        h.insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/json"),
        );
        h.insert("x-rpc-backend", HeaderValue::from_static("helius"));

        sanitize_response_headers(&mut h);
        assert!(h.get_all(header::SET_COOKIE).iter().next().is_none());
        assert!(h.get(header::TRANSFER_ENCODING).is_none());
        assert!(h.get(header::CONNECTION).is_none());
        assert_eq!(h.get(header::CONTENT_TYPE).unwrap(), "application/json");
        assert_eq!(h.get("x-rpc-backend").unwrap(), "helius");
    }

    #[test]
    fn redact_url_hides_query_only() {
        assert_eq!(
            redact_url("https://rpc.example.com/?api-key=SECRET"),
            "https://rpc.example.com/?<redacted>"
        );
        assert_eq!(
            redact_url("https://rpc.example.com/v1"),
            "https://rpc.example.com/v1"
        );
        assert_eq!(redact_url("wss://x/?a=1&b=2"), "wss://x/?<redacted>");
    }

    #[test]
    fn build_uri_errors_do_not_leak_query() {
        let err = build_uri("/relative?api-key=SECRET", "/", None).unwrap_err();
        assert!(!err.contains("SECRET"), "{}", err);
    }

    #[test]
    fn retryable_statuses() {
        assert!(is_retryable_status(StatusCode::TOO_MANY_REQUESTS));
        assert!(is_retryable_status(StatusCode::BAD_GATEWAY));
        assert!(is_retryable_status(StatusCode::INTERNAL_SERVER_ERROR));
        assert!(!is_retryable_status(StatusCode::OK));
        assert!(!is_retryable_status(StatusCode::BAD_REQUEST));
        assert!(!is_retryable_status(StatusCode::FORBIDDEN));
    }
}
