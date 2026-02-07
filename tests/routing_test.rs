use std::{collections::HashMap, sync::Arc};

use hyper_tls::HttpsConnector;
use hyper_util::client::legacy::Client;
use sol_rpc_router::{
    config::Backend,
    health::{BackendHealthStatus, HealthState},
    mock::MockKeyStore,
    state::AppState,
};

fn create_test_state() -> AppState {
    let https = HttpsConnector::new();
    let client = Client::builder(hyper_util::rt::TokioExecutor::new()).build(https);
    let keystore = Arc::new(MockKeyStore::new());

    let backends = vec![
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

    let backend_labels = backends.iter().map(|b| b.label.clone()).collect();
    let health_state = Arc::new(HealthState::new(backend_labels));
    let label_to_url = backends
        .iter()
        .map(|b| (b.label.clone(), b.url.clone()))
        .collect();

    AppState {
        client,
        backends,
        keystore,
        method_routes: HashMap::new(),
        label_to_url,
        health_state,
        proxy_timeout_secs: 10,
    }
}

#[test]
fn test_select_backend_weighted() {
    let mut state = create_test_state();
    state.backends[0].weight = 1;
    state.backends[1].weight = 1;

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

    // We shouldn't modify the backends on the same state instance if we want to be safe with borrows,
    // but here we just cloned the string.

    // Re-setup effectively by just checking the logic directly
    // With weight 0 for secondary in create_test_state, it should be primary
    assert_eq!(label_str, "primary");
}

#[test]
fn test_select_backend_unhealthy_fallback() {
    let state = create_test_state();
    // Mark primary as unhealthy
    let mut status = BackendHealthStatus::default();
    status.healthy = false;
    state.health_state.update_status("primary", status);

    let (label, _) = state.select_backend(None).unwrap();
    assert_eq!(label, "secondary");
}
