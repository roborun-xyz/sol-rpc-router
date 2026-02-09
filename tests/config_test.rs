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
redis_url = "redis://localhost"

[[backends]]
label = "b1"
url = "http://localhost:9000"
weight = 1
"#,
    );
    let config = load_config(&path).unwrap();
    assert_eq!(config.port, 8080);
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
fn test_load_config_empty_redis_url() {
    let path = write_temp_config(
        "empty_redis",
        r#"
port = 8080
redis_url = ""

[[backends]]
label = "b1"
url = "http://localhost:9000"
weight = 1
"#,
    );
    let err = load_config(&path).unwrap_err();
    assert!(
        err.to_string().contains("Redis URL"),
        "Expected 'Redis URL' in error: {}",
        err
    );
}

#[test]
fn test_load_config_no_backends() {
    let path = write_temp_config(
        "no_backends",
        r#"
port = 8080
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
redis_url = "redis://localhost"

[[backends]]
label = "bad-backend"
url = "http://localhost:9000"
weight = 0
"#,
    );
    let err = load_config(&path).unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("weight 0"), "Expected 'weight 0' in error: {}", msg);
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
