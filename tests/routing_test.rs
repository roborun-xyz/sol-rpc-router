use std::{collections::HashMap, sync::Arc};
use std::sync::atomic::{AtomicBool, Ordering};

use hyper_tls::HttpsConnector;
use hyper_util::client::legacy::Client;
use sol_rpc_router::{
    config::Backend,
    health::{BackendHealthStatus, HealthState},
    mock::MockKeyStore,
    state::{AppState, RuntimeBackend},
};

fn create_test_state() -> AppState {
    let https = HttpsConnector::new();
    let client = Client::builder(hyper_util::rt::TokioExecutor::new()).build(https);
    let keystore = Arc::new(MockKeyStore::new());

    let backend_configs = vec![
        Backend {
            label: "primary".to_string(),
            url: "http://primary".to_string(),
            ws_url: None,
            weight: 100,
        },
        Backend {
            label: "secondary".to_string(),
            url: "http://secondary".to_string(),
            ws_url: None,
            weight: 0,
        },
    ];

    let backends = backend_configs
        .iter()
        .map(|b| RuntimeBackend {
            config: b.clone(),
            healthy: Arc::new(AtomicBool::new(true)),
        })
        .collect();

    let backend_labels = backend_configs.iter().map(|b| b.label.clone()).collect();
    let health_state = Arc::new(HealthState::new(backend_labels));

    AppState {
        client,
        backends,
        keystore,
        method_routes: HashMap::new(),
        health_state,
        proxy_timeout_secs: 10,
    }
}

#[test]
fn test_select_backend_weighted() {
    let mut state = create_test_state();
    state.backends[0].config.weight = 1;
    state.backends[1].config.weight = 1;

    let iterations = 1000;
    let mut primary_count = 0;
    let mut secondary_count = 0;

    for _ in 0..iterations {
        let (label, _) = state.select_backend(None).unwrap();
        if label == "primary" {
            primary_count += 1;
        } else {
            secondary_count += 1;
        }
    }

    // Both should be selected roughly 50%
    assert!(primary_count > 400);
    assert!(secondary_count > 400);
}

#[test]
fn test_select_backend_method_override() {
    let mut state = create_test_state();
    state
        .method_routes
        .insert("eth_call".to_string(), "secondary".to_string());

    let (label, _) = state.select_backend(Some("eth_call")).unwrap();
    assert_eq!(label, "secondary");

    let (label, _) = state.select_backend(Some("eth_blockNumber")).unwrap();
    let label_str = label.to_string(); // Clone to drop borrow

    // Re-setup effectively by just checking the logic directly
    // With weight 0 for secondary in create_test_state, it should be primary
    assert_eq!(label_str, "primary");
}

#[test]
fn test_select_backend_unhealthy_fallback() {
    let state = create_test_state();
    // Mark primary as unhealthy (update AtomicBool directly for hot path)
    state.backends[0].healthy.store(false, Ordering::Relaxed);
    
    // Also update health_state for consistency (though select_backend uses AtomicBool)
    let mut status = BackendHealthStatus::default();
    status.healthy = false;
    state.health_state.update_status("primary", status);

    let (label, _) = state.select_backend(None).unwrap();
    assert_eq!(label, "secondary");
}

#[test]
fn test_select_backend_all_unhealthy() {
    let state = create_test_state();
    for backend in &state.backends {
        backend.healthy.store(false, Ordering::Relaxed);
    }
    
    assert!(state.select_backend(None).is_none());
}

// --- WebSocket backend selection tests ---

fn create_ws_test_state() -> AppState {
    let https = HttpsConnector::new();
    let client = Client::builder(hyper_util::rt::TokioExecutor::new()).build(https);
    let keystore = Arc::new(MockKeyStore::new());

    let backend_configs = vec![
        Backend {
            label: "ws-a".to_string(),
            url: "http://ws-a".to_string(),
            ws_url: Some("ws://ws-a".to_string()),
            weight: 1,
        },
        Backend {
            label: "ws-b".to_string(),
            url: "http://ws-b".to_string(),
            ws_url: Some("ws://ws-b".to_string()),
            weight: 1,
        },
    ];

    let backends = backend_configs
        .iter()
        .map(|b| RuntimeBackend {
            config: b.clone(),
            healthy: Arc::new(AtomicBool::new(true)),
        })
        .collect();

    let backend_labels = backend_configs.iter().map(|b| b.label.clone()).collect();
    let health_state = Arc::new(HealthState::new(backend_labels));

    AppState {
        client,
        backends,
        keystore,
        method_routes: HashMap::new(),
        health_state,
        proxy_timeout_secs: 10,
    }
}

#[test]
fn test_select_ws_backend_weighted() {
    let state = create_ws_test_state();
    let mut a_count = 0;
    let mut b_count = 0;

    for _ in 0..1000 {
        let (label, url) = state.select_ws_backend().unwrap();
        assert!(url.starts_with("ws://"));
        if label == "ws-a" {
            a_count += 1;
        } else {
            b_count += 1;
        }
    }

    assert!(a_count > 400, "ws-a selected {} times", a_count);
    assert!(b_count > 400, "ws-b selected {} times", b_count);
}

#[test]
fn test_select_ws_backend_no_ws_urls() {
    let state = create_test_state(); // backends have no ws_url
    assert!(state.select_ws_backend().is_none());
}

#[test]
fn test_select_ws_backend_unhealthy_excluded() {
    let state = create_ws_test_state();
    
    // Mark ws-a as unhealthy via AtomicBool
    state.backends[0].healthy.store(false, Ordering::Relaxed);

    for _ in 0..100 {
        let (label, _) = state.select_ws_backend().unwrap();
        assert_eq!(label, "ws-b");
    }
}
