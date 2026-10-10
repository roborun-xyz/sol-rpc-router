use std::sync::atomic::Ordering;
use std::{collections::HashMap, sync::Arc};

use arc_swap::ArcSwap;
use hyper_tls::HttpsConnector;
use hyper_util::client::legacy::Client;
use sol_rpc_router::{
    config::Backend,
    health::{BackendHealthStatus, HealthState},
    mock::MockKeyStore,
    state::{AppState, RouterState, RuntimeBackend},
};

fn create_test_state() -> AppState {
    let https = HttpsConnector::new();
    let client = Client::builder(hyper_util::rt::TokioExecutor::new()).build(https);
    let keystore = Arc::new(MockKeyStore::new());

    let backend_configs = [
        Backend {
            label: "primary".to_string(),
            url: "http://primary".to_string(),
            ws_url: None,
            max_rps: 0,
            weight: 100,
        },
        Backend {
            label: "secondary".to_string(),
            url: "http://secondary".to_string(),
            ws_url: None,
            max_rps: 0,
            weight: 0,
        },
    ];

    let backends = backend_configs
        .iter()
        .map(|b| RuntimeBackend::new(b.clone(), true))
        .collect();

    let backend_labels = backend_configs.iter().map(|b| b.label.clone()).collect();
    let health_state = Arc::new(HealthState::new(backend_labels));

    let router_state = RouterState::simple(backends, health_state);

    AppState::new(
        client,
        keystore,
        Arc::new(ArcSwap::from_pointee(router_state)),
    )
}

#[test]
fn test_select_backend_weighted() {
    let https = HttpsConnector::new();
    let client = Client::builder(hyper_util::rt::TokioExecutor::new()).build(https);
    let keystore = Arc::new(MockKeyStore::new());

    let backends = vec![
        RuntimeBackend::new(
            Backend {
                label: "primary".to_string(),
                url: "http://primary".to_string(),
                ws_url: None,
                max_rps: 0,
                weight: 1,
            },
            true,
        ),
        RuntimeBackend::new(
            Backend {
                label: "secondary".to_string(),
                url: "http://secondary".to_string(),
                ws_url: None,
                max_rps: 0,
                weight: 1,
            },
            true,
        ),
    ];

    let health_state = Arc::new(HealthState::new(vec![
        "primary".to_string(),
        "secondary".to_string(),
    ]));

    let router_state = RouterState::simple(backends, health_state);

    let state = AppState::new(
        client,
        keystore,
        Arc::new(ArcSwap::from_pointee(router_state)),
    );

    let iterations = 1000;
    let mut primary_count = 0;
    let mut secondary_count = 0;

    for _ in 0..iterations {
        let label = state.state.load().select_backend(None, &[]).unwrap().label;
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
    let https = HttpsConnector::new();
    let client = Client::builder(hyper_util::rt::TokioExecutor::new()).build(https);
    let keystore = Arc::new(MockKeyStore::new());

    let backends = vec![
        RuntimeBackend::new(
            Backend {
                label: "primary".to_string(),
                url: "http://primary".to_string(),
                ws_url: None,
                max_rps: 0,
                weight: 100,
            },
            true,
        ),
        RuntimeBackend::new(
            Backend {
                label: "secondary".to_string(),
                url: "http://secondary".to_string(),
                ws_url: None,
                max_rps: 0,
                weight: 0,
            },
            true,
        ),
    ];

    let health_state = Arc::new(HealthState::new(vec![
        "primary".to_string(),
        "secondary".to_string(),
    ]));

    let mut method_routes = HashMap::new();
    method_routes.insert("getAsset".to_string(), "secondary".to_string());

    let router_state = RouterState {
        method_routes,
        ..RouterState::simple(backends, health_state)
    };

    let state = AppState::new(
        client,
        keystore,
        Arc::new(ArcSwap::from_pointee(router_state)),
    );

    let label = state
        .state
        .load()
        .select_backend(Some("getAsset"), &[])
        .unwrap()
        .label;
    assert_eq!(label, "secondary");

    let label = state
        .state
        .load()
        .select_backend(Some("getSlot"), &[])
        .unwrap()
        .label;
    // With weight 0 for secondary, it should be primary
    assert_eq!(label, "primary");
}

#[test]
fn test_select_backend_unhealthy_fallback() {
    let state = create_test_state();
    let loaded = state.state.load();

    // Mark primary as unhealthy
    loaded.backends[0].healthy.store(false, Ordering::Relaxed);

    // Also update health_state for consistency
    let status = BackendHealthStatus {
        healthy: false,
        ..Default::default()
    };
    loaded.health_state.update_status("primary", status);

    let label = state.state.load().select_backend(None, &[]).unwrap().label;
    assert_eq!(label, "secondary");
}

#[test]
fn test_select_backend_all_unhealthy() {
    let state = create_test_state();
    let loaded = state.state.load();
    for backend in &loaded.backends {
        backend.healthy.store(false, Ordering::Relaxed);
    }

    assert!(state.state.load().select_backend(None, &[]).is_none());
}

// --- WebSocket backend selection tests ---

fn create_ws_test_state() -> AppState {
    let https = HttpsConnector::new();
    let client = Client::builder(hyper_util::rt::TokioExecutor::new()).build(https);
    let keystore = Arc::new(MockKeyStore::new());

    let backend_configs = [
        Backend {
            label: "ws-a".to_string(),
            url: "http://ws-a".to_string(),
            ws_url: Some("ws://ws-a".to_string()),
            max_rps: 0,
            weight: 1,
        },
        Backend {
            label: "ws-b".to_string(),
            url: "http://ws-b".to_string(),
            ws_url: Some("ws://ws-b".to_string()),
            max_rps: 0,
            weight: 1,
        },
    ];

    let backends = backend_configs
        .iter()
        .map(|b| RuntimeBackend::new(b.clone(), true))
        .collect();

    let backend_labels = backend_configs.iter().map(|b| b.label.clone()).collect();
    let health_state = Arc::new(HealthState::new(backend_labels));

    let router_state = RouterState::simple(backends, health_state);

    AppState::new(
        client,
        keystore,
        Arc::new(ArcSwap::from_pointee(router_state)),
    )
}

#[test]
fn test_select_ws_backend_weighted() {
    let state = create_ws_test_state();
    let mut a_count = 0;
    let mut b_count = 0;

    for _ in 0..1000 {
        let sel = state.state.load().select_ws_backend().unwrap();
        let (label, url) = (sel.label, sel.url);
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
    assert!(state.state.load().select_ws_backend().is_none());
}

#[test]
fn test_select_ws_backend_unhealthy_excluded() {
    let state = create_ws_test_state();
    let loaded = state.state.load();

    // Mark ws-a as unhealthy via AtomicBool
    loaded.backends[0].healthy.store(false, Ordering::Relaxed);

    for _ in 0..100 {
        let label = state.state.load().select_ws_backend().unwrap().label;
        assert_eq!(label, "ws-b");
    }
}

#[test]
fn test_select_backend_excludes_tried_labels() {
    let state = create_test_state();
    let rs = state.state.load();
    // "primary" has weight 100, "secondary" weight 0; excluding primary must
    // still yield secondary (weighted pick degrades to first candidate).
    let sel = rs.select_backend(None, &["primary".to_string()]).unwrap();
    assert_eq!(sel.label, "secondary");
    assert!(rs
        .select_backend(None, &["primary".to_string(), "secondary".to_string()])
        .is_none());
}

#[test]
fn test_max_rps_budget_skips_exhausted_backend() {
    let https = HttpsConnector::new();
    let client = Client::builder(hyper_util::rt::TokioExecutor::new()).build(https);
    let keystore = Arc::new(MockKeyStore::new());

    let backends = vec![
        RuntimeBackend::new(
            Backend {
                label: "metered".to_string(),
                url: "http://metered".to_string(),
                ws_url: None,
                weight: 1,
                max_rps: 2,
            },
            true,
        ),
        RuntimeBackend::new(
            Backend {
                label: "unlimited".to_string(),
                url: "http://unlimited".to_string(),
                ws_url: None,
                weight: 1,
                max_rps: 0,
            },
            true,
        ),
    ];
    let health_state = Arc::new(HealthState::new(vec![
        "metered".to_string(),
        "unlimited".to_string(),
    ]));
    let mut method_routes = HashMap::new();
    method_routes.insert("getSlot".to_string(), "metered".to_string());
    let router_state = RouterState {
        method_routes,
        ..RouterState::simple(backends, health_state)
    };
    let state = AppState::new(
        client,
        keystore,
        Arc::new(ArcSwap::from_pointee(router_state)),
    );
    let rs = state.state.load();

    // Pinned to the metered backend while it has budget...
    assert_eq!(
        rs.select_backend(Some("getSlot"), &[]).unwrap().label,
        "metered"
    );
    assert_eq!(
        rs.select_backend(Some("getSlot"), &[]).unwrap().label,
        "metered"
    );
    // ...then falls through to the backend that still has capacity.
    for _ in 0..20 {
        assert_eq!(
            rs.select_backend(Some("getSlot"), &[]).unwrap().label,
            "unlimited"
        );
    }
}

#[test]
fn test_all_backends_at_capacity_is_distinguishable_from_unhealthy() {
    let https = HttpsConnector::new();
    let client = Client::builder(hyper_util::rt::TokioExecutor::new()).build(https);
    let keystore = Arc::new(MockKeyStore::new());
    let backends = vec![RuntimeBackend::new(
        Backend {
            label: "metered".to_string(),
            url: "http://metered".to_string(),
            ws_url: None,
            weight: 1,
            max_rps: 1,
        },
        true,
    )];
    let health_state = Arc::new(HealthState::new(vec!["metered".to_string()]));
    let state = AppState::new(
        client,
        keystore,
        Arc::new(ArcSwap::from_pointee(RouterState::simple(
            backends,
            health_state,
        ))),
    );
    let rs = state.state.load();
    assert!(rs.select_backend(None, &[]).is_some());
    assert!(rs.select_backend(None, &[]).is_none());
    assert!(
        rs.any_healthy(),
        "out of budget is not the same as unhealthy"
    );
}
