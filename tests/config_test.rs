use std::io::Write;

use sol_rpc_router::config::load_config;

fn write_temp_config(name: &str, content: &str) -> String {
    let mut path = std::env::temp_dir();
    path.push(format!("sol_rpc_router_test_config_{}.toml", name));
    let path_str = path.to_str().unwrap().to_string();

    let mut f = std::fs::File::create(&path).unwrap();
    f.write_all(content.as_bytes()).unwrap();
    path_str
}

#[test]
fn test_load_config_valid() {
    let path = write_temp_config(
        "valid",
        r#"
port = 8080
metrics_port = 9091
redis_url = "redis://localhost"

[[backends]]
label = "b1"
url = "http://localhost:9000"
weight = 1
"#,
    );
    let config = load_config(&path).unwrap();
    assert_eq!(config.port, 8080);
    assert_eq!(config.metrics_port, 9091);
    assert_eq!(config.backends.len(), 1);
    assert_eq!(config.backends[0].label, "b1");
}

#[test]
fn test_load_config_file_not_found() {
    let mut path = std::env::temp_dir();
    path.push("sol_rpc_router_nonexistent_config.toml");
    let path_str = path.to_str().unwrap();

    let err = load_config(path_str).unwrap_err();
    assert!(
        err.to_string().contains("not found") || err.to_string().contains("No such file"),
        "Expected 'not found' in error: {}",
        err
    );
}

#[test]
fn test_load_config_invalid_toml() {
    let path = write_temp_config("invalid_toml", "this is not valid toml {{{{");
    let err = load_config(&path).unwrap_err();
    // toml parse errors are descriptive enough; just make sure it's an error
    assert!(!err.to_string().is_empty());
}

#[test]
fn test_validate_config_empty_redis_url_and_no_keys() {
    use sol_rpc_router::config::{validate_config, Config};
    // Goes through validate_config directly so a REDIS_URL in the test
    // environment cannot mask the check.
    let config: Config = toml::from_str(
        r#"
port = 8080
metrics_port = 9091
redis_url = ""

[[backends]]
label = "b1"
url = "http://localhost:9000"
weight = 1
"#,
    )
    .unwrap();
    let err = validate_config(config, None).unwrap_err();
    assert!(
        err.to_string().contains("No keystore configured"),
        "Expected keystore error: {}",
        err
    );
}

#[test]
fn test_load_config_no_backends() {
    let path = write_temp_config(
        "no_backends",
        r#"
port = 8080
metrics_port = 9091
redis_url = "redis://localhost"
backends = []
"#,
    );
    let err = load_config(&path).unwrap_err();
    assert!(
        err.to_string().contains("At least one backend"),
        "Expected 'At least one backend' in error: {}",
        err
    );
}

#[test]
fn test_load_config_duplicate_labels() {
    let path = write_temp_config(
        "dup_labels",
        r#"
port = 8080
metrics_port = 9091
redis_url = "redis://localhost"

[[backends]]
label = "same"
url = "http://localhost:9000"
weight = 1

[[backends]]
label = "same"
url = "http://localhost:9001"
weight = 1
"#,
    );
    let err = load_config(&path).unwrap_err();
    assert!(
        err.to_string().contains("Duplicate backend labels"),
        "Expected 'Duplicate backend labels' in error: {}",
        err
    );
}

#[test]
fn test_load_config_zero_weight() {
    let path = write_temp_config(
        "zero_weight",
        r#"
port = 8080
metrics_port = 9091
redis_url = "redis://localhost"

[[backends]]
label = "bad-backend"
url = "http://localhost:9000"
weight = 0
"#,
    );
    let err = load_config(&path).unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("weight 0"),
        "Expected 'weight 0' in error: {}",
        msg
    );
    assert!(
        msg.contains("bad-backend"),
        "Expected backend name in error: {}",
        msg
    );
}

#[test]
fn test_load_config_empty_label() {
    let path = write_temp_config(
        "empty_label",
        r#"
port = 8080
metrics_port = 9091
redis_url = "redis://localhost"

[[backends]]
label = ""
url = "http://localhost:9000"
weight = 1
"#,
    );
    let err = load_config(&path).unwrap_err();
    assert!(
        err.to_string().contains("empty label"),
        "Expected 'empty label' in error: {}",
        err
    );
}

#[test]
fn test_load_config_zero_proxy_timeout() {
    let path = write_temp_config(
        "zero_timeout",
        r#"
port = 8080
metrics_port = 9091
redis_url = "redis://localhost"

[[backends]]
label = "b1"
url = "http://localhost:9000"
weight = 1

[proxy]
timeout_secs = 0
"#,
    );
    let err = load_config(&path).unwrap_err();
    assert!(
        err.to_string().contains("timeout_secs"),
        "Expected 'timeout_secs' in error: {}",
        err
    );
}

#[test]
fn test_load_config_unknown_method_route() {
    let path = write_temp_config(
        "bad_method_route",
        r#"
port = 8080
metrics_port = 9091
redis_url = "redis://localhost"

[[backends]]
label = "b1"
url = "http://localhost:9000"
weight = 1

[method_routes]
getSlot = "nonexistent"
"#,
    );
    let err = load_config(&path).unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("nonexistent"),
        "Expected 'nonexistent' in error: {}",
        msg
    );
    assert!(
        msg.contains("unknown backend label"),
        "Expected 'unknown backend label' in error: {}",
        msg
    );
}

#[test]
fn test_load_config_missing_metrics_port() {
    let path = write_temp_config(
        "missing_metrics",
        r#"
port = 8080
# metrics_port missing
redis_url = "redis://localhost"

[[backends]]
label = "b1"
url = "http://localhost:9000"
weight = 1
"#,
    );
    let err = load_config(&path).unwrap_err();
    assert!(err.to_string().contains("missing field `metrics_port`"));
}

#[test]
fn test_load_config_metrics_port_conflict_http() {
    let path = write_temp_config(
        "metrics_conflict_http",
        r#"
port = 8080
metrics_port = 8080
redis_url = "redis://localhost"

[[backends]]
label = "b1"
url = "http://localhost:9000"
weight = 1
"#,
    );
    let err = load_config(&path).unwrap_err();
    assert!(
        err.to_string()
            .contains("HTTP port and Metrics port must be different"),
        "Expected conflict error: {}",
        err
    );
}

#[test]
fn test_load_config_metrics_port_conflict_ws() {
    let path = write_temp_config(
        "metrics_conflict_ws",
        r#"
port = 8080
metrics_port = 8081
redis_url = "redis://localhost"

[[backends]]
label = "b1"
url = "http://localhost:9000"
weight = 1
"#,
    );
    let err = load_config(&path).unwrap_err();
    assert!(
        err.to_string().contains("conflicts with WebSocket port"),
        "Expected WS conflict error: {}",
        err
    );
}

#[test]
fn test_load_config_proxy_options() {
    let path = write_temp_config(
        "proxy_options",
        r#"
port = 8080
metrics_port = 9091
redis_url = "redis://localhost"

[[backends]]
label = "b1"
url = "http://localhost:9000"
weight = 1

[proxy]
timeout_secs = 7
max_retries = 1
fanout_methods = ["sendTransaction"]
blocked_methods = ["getProgramAccounts"]
"#,
    );
    let config = load_config(&path).unwrap();
    assert_eq!(config.proxy.timeout_secs, 7);
    assert_eq!(config.proxy.max_retries, 1);
    assert_eq!(config.proxy.fanout_methods, vec!["sendTransaction"]);
    assert_eq!(config.proxy.blocked_methods, vec!["getProgramAccounts"]);
}

#[test]
fn test_load_config_defaults_for_proxy() {
    let path = write_temp_config(
        "proxy_defaults",
        r#"
port = 8080
metrics_port = 9091
redis_url = "redis://localhost"

[[backends]]
label = "b1"
url = "http://localhost:9000"
weight = 1
"#,
    );
    let config = load_config(&path).unwrap();
    assert_eq!(config.proxy.timeout_secs, 30);
    assert_eq!(config.proxy.max_retries, 2);
    assert!(config.proxy.fanout_methods.is_empty());
    assert!(config.proxy.blocked_methods.is_empty());
}

#[test]
fn test_load_config_fanout_and_blocked_conflict() {
    let path = write_temp_config(
        "fanout_blocked_conflict",
        r#"
port = 8080
metrics_port = 9091
redis_url = "redis://localhost"

[[backends]]
label = "b1"
url = "http://localhost:9000"
weight = 1

[proxy]
fanout_methods = ["sendTransaction"]
blocked_methods = ["sendTransaction"]
"#,
    );
    let err = load_config(&path).unwrap_err();
    assert!(err.to_string().contains("both fanned out and blocked"));
}

#[test]
fn test_load_config_rejects_bad_backend_scheme() {
    let path = write_temp_config(
        "bad_scheme",
        r#"
port = 8080
metrics_port = 9091
redis_url = "redis://localhost"

[[backends]]
label = "b1"
url = "localhost:9000"
weight = 1
"#,
    );
    let err = load_config(&path).unwrap_err();
    assert!(err.to_string().contains("http://"));
}

#[test]
fn test_load_config_rejects_bad_ws_scheme() {
    let path = write_temp_config(
        "bad_ws_scheme",
        r#"
port = 8080
metrics_port = 9091
redis_url = "redis://localhost"

[[backends]]
label = "b1"
url = "http://localhost:9000"
ws_url = "http://localhost:9000"
weight = 1
"#,
    );
    let err = load_config(&path).unwrap_err();
    assert!(err.to_string().contains("ws://"));
}

#[test]
fn test_validate_config_redis_url_override() {
    use sol_rpc_router::config::{validate_config, Config};

    let config: Config = toml::from_str(
        r#"
port = 8080
metrics_port = 9091

[[backends]]
label = "b1"
url = "http://localhost:9000"
weight = 1
"#,
    )
    .unwrap();

    // Without an override an empty redis_url is rejected...
    let err = validate_config(config.clone(), None).unwrap_err();
    assert!(err.to_string().contains("REDIS_URL"));

    // ...and with one it is used.
    let config = validate_config(config, Some("redis://override:6379".into())).unwrap();
    assert_eq!(config.redis_url, "redis://override:6379");
}

#[test]
fn test_file_keystore_config_is_accepted_without_redis() {
    let path = write_temp_config(
        "file_keystore",
        r#"
port = 8080
metrics_port = 9091

[[backends]]
label = "b1"
url = "http://localhost:9000"
weight = 1

[[api_keys]]
key = "abc123"
owner = "alice"
rate_limit = 25

[[api_keys]]
key = "def456"
owner = "bob"
"#,
    );
    let config = load_config(&path).unwrap();
    assert_eq!(
        config.keystore_kind(),
        sol_rpc_router::config::KeyStoreKind::File
    );
    assert_eq!(config.api_keys.len(), 2);
    assert_eq!(config.api_keys[0].rate_limit, 25);
    assert_eq!(config.api_keys[1].rate_limit, 0);
    assert!(config.api_keys[1].active);
    assert_eq!(config.api_keys[1].expires_at, 0);
}

#[test]
fn test_no_keystore_is_rejected() {
    let path = write_temp_config(
        "no_keystore",
        r#"
port = 8080
metrics_port = 9091

[[backends]]
label = "b1"
url = "http://localhost:9000"
weight = 1
"#,
    );
    let err = load_config(&path).unwrap_err();
    assert!(
        err.to_string().contains("No keystore configured"),
        "{}",
        err
    );
}

#[test]
fn test_both_keystores_is_rejected() {
    let path = write_temp_config(
        "both_keystores",
        r#"
port = 8080
metrics_port = 9091
redis_url = "redis://localhost"

[[backends]]
label = "b1"
url = "http://localhost:9000"
weight = 1

[[api_keys]]
key = "abc123"
owner = "alice"
"#,
    );
    let err = load_config(&path).unwrap_err();
    assert!(err.to_string().contains("pick one keystore"), "{}", err);
}

#[test]
fn test_api_keys_validation() {
    for (name, block, needle) in [
        (
            "dup",
            "[[api_keys]]\nkey = \"a\"\nowner = \"x\"\n[[api_keys]]\nkey = \"a\"\nowner = \"y\"\n",
            "Duplicate",
        ),
        (
            "emptykey",
            "[[api_keys]]\nkey = \"\"\nowner = \"x\"\n",
            "empty key",
        ),
        (
            "emptyowner",
            "[[api_keys]]\nkey = \"a\"\nowner = \"\"\n",
            "empty owner",
        ),
    ] {
        let path = write_temp_config(
            name,
            &format!(
                "port = 8080\nmetrics_port = 9091\n\n[[backends]]\nlabel = \"b1\"\nurl = \"http://localhost:9000\"\nweight = 1\n\n{}",
                block
            ),
        );
        let err = load_config(&path).unwrap_err();
        assert!(err.to_string().contains(needle), "{}: {}", name, err);
    }
}

#[test]
fn test_unknown_fields_are_rejected() {
    for (name, body) in [
        ("top", "max_retry = 5\n"),
        ("proxy", "[proxy]\nmax_retry = 5\n"),
        ("health", "[health_check]\ninterval = 5\n"),
        (
            "backend",
            "[[backends]]\nlabel = \"x\"\nurl = \"http://x\"\nweight = 1\nwss_url = \"wss://x\"\n",
        ),
    ] {
        let path = write_temp_config(
            &format!("unknown_{}", name),
            &format!(
                "port = 8080\nmetrics_port = 9091\nredis_url = \"redis://localhost\"\n\n[[backends]]\nlabel = \"b1\"\nurl = \"http://localhost:9000\"\nweight = 1\n\n{}",
                body
            ),
        );
        let err = load_config(&path).unwrap_err();
        assert!(
            err.to_string().contains("unknown field"),
            "{}: {}",
            name,
            err
        );
    }
}

#[test]
fn test_zero_health_check_timings_are_rejected() {
    for (name, body, needle) in [
        (
            "interval",
            "[health_check]\ninterval_secs = 0\n",
            "interval_secs",
        ),
        (
            "timeout",
            "[health_check]\ntimeout_secs = 0\n",
            "timeout_secs",
        ),
        (
            "threshold",
            "[health_check]\nconsecutive_failures_threshold = 0\n",
            "thresholds",
        ),
    ] {
        let path = write_temp_config(
            &format!("zero_{}", name),
            &format!(
                "port = 8080\nmetrics_port = 9091\nredis_url = \"redis://localhost\"\n\n[[backends]]\nlabel = \"b1\"\nurl = \"http://localhost:9000\"\nweight = 1\n\n{}",
                body
            ),
        );
        let err = load_config(&path).unwrap_err();
        assert!(err.to_string().contains(needle), "{}: {}", name, err);
    }
}

#[test]
fn test_selection_strategy_parses() {
    use sol_rpc_router::config::SelectionStrategy;
    let path = write_temp_config(
        "selection",
        r#"
port = 8080
metrics_port = 9091
redis_url = "redis://localhost"

[[backends]]
label = "b1"
url = "http://localhost:9000"
weight = 1

[proxy]
selection = "latency_weighted"
"#,
    );
    let config = load_config(&path).unwrap();
    assert_eq!(config.proxy.selection, SelectionStrategy::LatencyWeighted);

    let path = write_temp_config(
        "selection_default",
        r#"
port = 8080
metrics_port = 9091
redis_url = "redis://localhost"

[[backends]]
label = "b1"
url = "http://localhost:9000"
weight = 1
"#,
    );
    assert_eq!(
        load_config(&path).unwrap().proxy.selection,
        SelectionStrategy::Weighted
    );

    let path = write_temp_config(
        "selection_bad",
        r#"
port = 8080
metrics_port = 9091
redis_url = "redis://localhost"

[[backends]]
label = "b1"
url = "http://localhost:9000"
weight = 1

[proxy]
selection = "fastest"
"#,
    );
    assert!(load_config(&path).is_err());
}
