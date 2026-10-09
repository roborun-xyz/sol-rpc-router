//! End-to-end WebSocket proxy test: a real client connects to the router over
//! TCP, the router dials an in-process echo backend, and frames flow both ways.

use std::{net::SocketAddr, sync::Arc};

use arc_swap::ArcSwap;
use futures_util::{SinkExt, StreamExt};
use hyper_tls::HttpsConnector;
use hyper_util::client::legacy::Client;
use sol_rpc_router::{
    config::Backend,
    health::HealthState,
    mock::MockKeyStore,
    router::ws_router,
    state::{AppState, RouterState, RuntimeBackend},
};
use tokio_tungstenite::{accept_async, connect_async, tungstenite::Message};

/// Echo server that prefixes every text frame with "echo:".
async fn start_echo_backend() -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let (stream, _) = listener.accept().await.unwrap();
            tokio::spawn(async move {
                let mut ws = accept_async(stream).await.unwrap();
                while let Some(Ok(msg)) = ws.next().await {
                    match msg {
                        Message::Text(t) => {
                            if ws.send(Message::Text(format!("echo:{}", t))).await.is_err() {
                                break;
                            }
                        }
                        Message::Close(_) => break,
                        _ => {}
                    }
                }
            });
        }
    });
    format!("ws://{}", addr)
}

async fn start_router(ws_url: Option<String>, healthy: bool) -> SocketAddr {
    start_router_inner(ws_url, healthy, 0).await
}

async fn start_router_with_cap(ws_url: Option<String>, cap: u32) -> SocketAddr {
    start_router_inner(ws_url, true, cap).await
}

async fn start_router_inner(ws_url: Option<String>, healthy: bool, cap: u32) -> SocketAddr {
    let https = HttpsConnector::new();
    let client = Client::builder(hyper_util::rt::TokioExecutor::new()).build(https);
    let keystore = Arc::new(MockKeyStore::new());
    keystore.add_key("ws-key", "ws-tester", 100);

    let backend = RuntimeBackend::new(
        Backend {
            label: "echo".into(),
            url: "http://unused".into(),
            ws_url,
            weight: 1,
        },
        healthy,
    );
    let health_state = Arc::new(HealthState::new(vec!["echo".into()]));
    let mut router_state = RouterState::simple(vec![backend], health_state);
    router_state.proxy.max_ws_connections_per_key = cap;
    let state = Arc::new(AppState::new(
        client,
        keystore,
        Arc::new(ArcSwap::from_pointee(router_state)),
    ));

    let app = ws_router(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .await
        .unwrap();
    });
    addr
}

#[tokio::test]
async fn ws_frames_are_relayed_both_ways() {
    let backend = start_echo_backend().await;
    let router = start_router(Some(backend), true).await;

    let (mut ws, _) = connect_async(format!("ws://{}/?api-key=ws-key", router))
        .await
        .expect("upgrade through router");

    ws.send(Message::Text(
        r#"{"jsonrpc":"2.0","id":1,"method":"slotSubscribe"}"#.into(),
    ))
    .await
    .unwrap();

    let reply = tokio::time::timeout(std::time::Duration::from_secs(5), ws.next())
        .await
        .expect("reply in time")
        .unwrap()
        .unwrap();
    assert_eq!(
        reply.into_text().unwrap(),
        r#"echo:{"jsonrpc":"2.0","id":1,"method":"slotSubscribe"}"#
    );

    ws.close(None).await.unwrap();
}

#[tokio::test]
async fn ws_auth_via_header() {
    let backend = start_echo_backend().await;
    let router = start_router(Some(backend), true).await;

    let mut req = tokio_tungstenite::tungstenite::client::IntoClientRequest::into_client_request(
        format!("ws://{}/", router),
    )
    .unwrap();
    req.headers_mut()
        .insert("x-api-key", "ws-key".parse().unwrap());

    let (mut ws, _) = connect_async(req).await.expect("header auth accepted");
    ws.send(Message::Text("hi".into())).await.unwrap();
    let reply = ws.next().await.unwrap().unwrap();
    assert_eq!(reply.into_text().unwrap(), "echo:hi");
}

#[tokio::test]
async fn ws_rejects_bad_key_before_upgrade() {
    let backend = start_echo_backend().await;
    let router = start_router(Some(backend), true).await;

    let err = connect_async(format!("ws://{}/?api-key=wrong", router))
        .await
        .expect_err("handshake must fail");
    match err {
        tokio_tungstenite::tungstenite::Error::Http(resp) => {
            assert_eq!(resp.status(), 401);
        }
        other => panic!("unexpected error: {:?}", other),
    }
}

#[tokio::test]
async fn ws_no_backend_returns_503() {
    let router = start_router(None, true).await;

    let err = connect_async(format!("ws://{}/?api-key=ws-key", router))
        .await
        .expect_err("handshake must fail");
    match err {
        tokio_tungstenite::tungstenite::Error::Http(resp) => {
            assert_eq!(resp.status(), 503);
        }
        other => panic!("unexpected error: {:?}", other),
    }
}

#[tokio::test]
async fn ws_per_key_connection_cap() {
    let backend = start_echo_backend().await;
    let router = start_router_with_cap(Some(backend), 2).await;
    let url = format!("ws://{}/?api-key=ws-key", router);

    let (_a, _) = connect_async(&url).await.expect("first");
    let (_b, _) = connect_async(&url).await.expect("second");
    let err = connect_async(&url)
        .await
        .expect_err("third must be refused");
    match err {
        tokio_tungstenite::tungstenite::Error::Http(resp) => assert_eq!(resp.status(), 429),
        other => panic!("unexpected error: {:?}", other),
    }

    // Closing one frees a slot.
    drop(_a);
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    let (_c, _) = connect_async(&url).await.expect("slot freed");
}
