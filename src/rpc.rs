//! JSON-RPC helpers: request probing, error responses and metric label hygiene.

use axum::{
    http::{header, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use serde::Deserialize;
use serde_json::{json, Value};

/// JSON-RPC error codes used by the router itself.
///
/// Solana nodes use -32001..-32016 for their own custom errors (node
/// unhealthy, preflight failure, block cleaned up, ...), so router errors
/// live in -32090..-32099 where no Solana client will misread them.
/// `INVALID_REQUEST` and `INTERNAL` are the standard JSON-RPC codes.
pub mod codes {
    pub const INVALID_REQUEST: i64 = -32600;
    pub const INTERNAL: i64 = -32603;
    pub const UNAUTHORIZED: i64 = -32090;
    pub const RATE_LIMITED: i64 = -32091;
    pub const METHOD_BLOCKED: i64 = -32092;
    pub const NO_BACKEND: i64 = -32093;
    pub const UPSTREAM_ERROR: i64 = -32094;
    pub const UPSTREAM_TIMEOUT: i64 = -32095;
    pub const BACKENDS_AT_CAPACITY: i64 = -32096;
}

/// Label used for batch (array) JSON-RPC requests in logs and metrics.
pub const BATCH_METHOD: &str = "batch";

/// Label used for unknown methods in metrics to keep cardinality bounded.
pub const OTHER_METHOD: &str = "other";

/// Summary of an incoming JSON-RPC body, extracted once by middleware and
/// shared with handlers through request extensions.
#[derive(Clone, Debug)]
pub struct RpcRequestInfo {
    /// The JSON-RPC `method`, or [`BATCH_METHOD`] for arrays.
    pub method: String,
    /// The JSON-RPC `id` of a single request. `Null` for batches.
    pub id: Value,
    /// Every method in the request (one for single calls, N for batches).
    pub methods: Vec<String>,
}

#[derive(Deserialize)]
struct MethodProbe<'a> {
    method: Option<&'a str>,
    #[serde(default)]
    id: Value,
}

/// Parses just enough of a JSON-RPC body to learn its method(s) and id without
/// materialising `params`. Returns `None` if the body isn't a JSON-RPC request.
pub fn probe_request(body: &[u8]) -> Option<RpcRequestInfo> {
    // Fast path: single request object.
    if let Ok(probe) = serde_json::from_slice::<MethodProbe>(body) {
        let method = probe.method?.to_string();
        return Some(RpcRequestInfo {
            methods: vec![method.clone()],
            method,
            id: probe.id,
        });
    }

    // Batch request: array of objects.
    if let Ok(probes) = serde_json::from_slice::<Vec<MethodProbe>>(body) {
        let methods: Vec<String> = probes
            .iter()
            .filter_map(|p| p.method.map(str::to_string))
            .collect();
        if methods.is_empty() {
            return None;
        }
        return Some(RpcRequestInfo {
            method: BATCH_METHOD.to_string(),
            id: Value::Null,
            methods,
        });
    }

    None
}

/// Builds a JSON-RPC error response with the given HTTP status.
pub fn error_response(
    status: StatusCode,
    code: i64,
    message: impl Into<String>,
    id: Option<&Value>,
) -> Response {
    let body = json!({
        "jsonrpc": "2.0",
        "error": { "code": code, "message": message.into() },
        "id": id.cloned().unwrap_or(Value::Null),
    });
    let mut resp = (status, Json(body)).into_response();
    if status == StatusCode::TOO_MANY_REQUESTS {
        resp.headers_mut()
            .insert(header::RETRY_AFTER, HeaderValue::from_static("1"));
    }
    resp
}

/// Public Solana JSON-RPC HTTP methods plus the WebSocket subscription pairs.
/// Keeping this list lets us bound the `rpc_method` metric label: anything not
/// listed is reported as [`OTHER_METHOD`] so a client can't blow up Prometheus
/// cardinality by spamming random method names.
pub const KNOWN_METHODS: &[&str] = &[
    "getAccountInfo",
    "getBalance",
    "getBlock",
    "getBlockCommitment",
    "getBlockHeight",
    "getBlockProduction",
    "getBlockTime",
    "getBlocks",
    "getBlocksWithLimit",
    "getClusterNodes",
    "getEpochInfo",
    "getEpochSchedule",
    "getFeeForMessage",
    "getFirstAvailableBlock",
    "getGenesisHash",
    "getHealth",
    "getHighestSnapshotSlot",
    "getIdentity",
    "getInflationGovernor",
    "getInflationRate",
    "getInflationReward",
    "getLargestAccounts",
    "getLatestBlockhash",
    "getLeaderSchedule",
    "getMaxRetransmitSlot",
    "getMaxShredInsertSlot",
    "getMinimumBalanceForRentExemption",
    "getMultipleAccounts",
    "getProgramAccounts",
    "getRecentPerformanceSamples",
    "getRecentPrioritizationFees",
    "getSignatureStatuses",
    "getSignaturesForAddress",
    "getSlot",
    "getSlotLeader",
    "getSlotLeaders",
    "getStakeMinimumDelegation",
    "getSupply",
    "getTokenAccountBalance",
    "getTokenAccountsByDelegate",
    "getTokenAccountsByOwner",
    "getTokenLargestAccounts",
    "getTokenSupply",
    "getTransaction",
    "getTransactionCount",
    "getVersion",
    "getVoteAccounts",
    "isBlockhashValid",
    "minimumLedgerSlot",
    "requestAirdrop",
    "sendTransaction",
    "simulateTransaction",
    // Deprecated but still widely called
    "getConfirmedBlock",
    "getConfirmedSignaturesForAddress2",
    "getConfirmedTransaction",
    "getFees",
    "getRecentBlockhash",
    "getSignatureStatus",
    "getSnapshotSlot",
    "getStakeActivation",
    // WebSocket subscriptions
    "accountSubscribe",
    "accountUnsubscribe",
    "blockSubscribe",
    "blockUnsubscribe",
    "logsSubscribe",
    "logsUnsubscribe",
    "programSubscribe",
    "programUnsubscribe",
    "rootSubscribe",
    "rootUnsubscribe",
    "signatureSubscribe",
    "signatureUnsubscribe",
    "slotSubscribe",
    "slotUnsubscribe",
    "slotsUpdatesSubscribe",
    "slotsUpdatesUnsubscribe",
    "voteSubscribe",
    "voteUnsubscribe",
    // Common extensions offered by providers (DAS, priority fees)
    "getAsset",
    "getAssetProof",
    "getAssetsByOwner",
    "getAssetsByAuthority",
    "getAssetsByCreator",
    "getAssetsByGroup",
    "searchAssets",
    "getTokenAccounts",
    "getPriorityFeeEstimate",
];

/// Returns the method name if it is a known Solana method, otherwise
/// [`OTHER_METHOD`]. [`BATCH_METHOD`] passes through unchanged.
pub fn metric_label(method: &str) -> &str {
    if method == BATCH_METHOD || KNOWN_METHODS.contains(&method) {
        method
    } else {
        OTHER_METHOD
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn probe_single_request() {
        let info =
            probe_request(br#"{"jsonrpc":"2.0","id":7,"method":"getSlot","params":[]}"#).unwrap();
        assert_eq!(info.method, "getSlot");
        assert_eq!(info.id, json!(7));
        assert_eq!(info.methods, vec!["getSlot"]);
    }

    #[test]
    fn probe_string_id() {
        let info = probe_request(br#"{"id":"abc","method":"getHealth"}"#).unwrap();
        assert_eq!(info.id, json!("abc"));
    }

    #[test]
    fn probe_batch_request() {
        let info = probe_request(
            br#"[{"id":1,"method":"getSlot"},{"id":2,"method":"getBlockHeight","params":[]}]"#,
        )
        .unwrap();
        assert_eq!(info.method, BATCH_METHOD);
        assert_eq!(info.id, Value::Null);
        assert_eq!(info.methods, vec!["getSlot", "getBlockHeight"]);
    }

    #[test]
    fn probe_rejects_garbage() {
        assert!(probe_request(b"not json").is_none());
        assert!(probe_request(br#"{"id":1}"#).is_none());
        assert!(probe_request(b"[]").is_none());
        assert!(probe_request(br#"[{"id":1}]"#).is_none());
    }

    #[test]
    fn metric_label_bounds_cardinality() {
        assert_eq!(metric_label("getSlot"), "getSlot");
        assert_eq!(metric_label("batch"), "batch");
        assert_eq!(metric_label("totallyMadeUp"), "other");
        assert_eq!(metric_label(""), "other");
    }
}
