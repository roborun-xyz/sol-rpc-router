//! End-to-end WebSocket proxy test: a real client connects to the router over
//! TCP, the router dials an in-process echo backend, and frames flow both ways.

use std::{net::SocketAddr, sync::Arc};

use arc_swap::ArcSwap;
use futures_util::{SinkExt, StreamExt};
use hyper_tls::HttpsConnector;
use hyper_util::client::legacy::Client;
use sol_rpc_router::{
    config::{Backend, ProxyConfig},
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
    start_router_inner(ws_url, healthy, |p| p.max_ws_connections_per_key = 0)
        .await
        .0
}

async fn start_router_with_cap(ws_url: Option<String>, cap: u32) -> SocketAddr {
    start_router_inner(ws_url, true, |p| p.max_ws_connections_per_key = cap)
        .await
        .0
}

async fn start_router_inner(
    ws_url: Option<String>,
    healthy: bool,
    configure: impl FnOnce(&mut ProxyConfig),
) -> (SocketAddr, Arc<AppState>) {
    let https = HttpsConnector::new();
    let client = Client::builder(hyper_util::rt::TokioExecutor::new()).build(https);
    let keystore = Arc::new(MockKeyStore::new());
    keystore.add_key("ws-key", "ws-tester", 100);

    let backend = RuntimeBackend::new(
        Backend {
            label: "echo".into(),
            url: "http://unused".into(),
            ws_url,
            max_rps: 0,
            weight: 1,
        },
        healthy,
    );
    let health_state = Arc::new(HealthState::new(vec!["echo".into()]));
    let mut router_state = RouterState::simple(vec![backend], health_state);
    configure(&mut router_state.proxy);
    let state = Arc::new(AppState::new(
        client,
        keystore,
        Arc::new(ArcSwap::from_pointee(router_state)),
    ));

    let app = ws_router(state.clone());
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
    (addr, state)
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

/// Reads frames until the router closes the session, returning the close
/// frame. Reading also lets the client auto-reply to pings.
async fn read_until_close(
    ws: &mut tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
) -> Option<tokio_tungstenite::tungstenite::protocol::CloseFrame<'static>> {
    while let Some(msg) = ws.next().await {
        match msg {
            Ok(Message::Close(frame)) => return frame,
            Ok(_) => {}
            Err(_) => return None,
        }
    }
    None
}

async fn wait_for_slot_release(state: &AppState) {
    for _ in 0..50 {
        if state.ws_session_count("ws-tester") == 0 {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    panic!(
        "slot not released, count={}",
        state.ws_session_count("ws-tester")
    );
}

#[tokio::test]
async fn ws_idle_session_is_closed_and_slot_freed() {
    let backend = start_echo_backend().await;
    let (router, state) = start_router_inner(Some(backend), true, |p| {
        p.max_ws_connections_per_key = 1;
        p.ws_idle_timeout_secs = 1;
        p.ws_ping_interval_secs = 0;
    })
    .await;
    let url = format!("ws://{}/?api-key=ws-key", router);

    let (mut ws, _) = connect_async(&url).await.expect("upgrade");
    // Activity before going quiet is relayed as usual.
    ws.send(Message::Text("hi".into())).await.unwrap();
    assert_eq!(
        ws.next().await.unwrap().unwrap().into_text().unwrap(),
        "echo:hi"
    );
    assert_eq!(state.ws_session_count("ws-tester"), 1);

    let started = std::time::Instant::now();
    let frame = tokio::time::timeout(std::time::Duration::from_secs(5), read_until_close(&mut ws))
        .await
        .expect("router closes the idle session")
        .expect("close frame");
    assert!(started.elapsed() >= std::time::Duration::from_millis(900));
    assert_eq!(frame.reason, "idle timeout");

    wait_for_slot_release(&state).await;
    // The single slot is usable again.
    let (_again, _) = connect_async(&url).await.expect("slot freed");
}

#[tokio::test]
async fn ws_pings_keep_live_idle_client_open() {
    let backend = start_echo_backend().await;
    let (router, state) = start_router_inner(Some(backend), true, |p| {
        p.ws_idle_timeout_secs = 2;
        p.ws_ping_interval_secs = 1;
    })
    .await;

    let (mut ws, _) = connect_async(format!("ws://{}/?api-key=ws-key", router))
        .await
        .expect("upgrade");

    // Stay quiet for longer than the idle timeout, only answering pings.
    let mut pings = 0;
    let quiet = tokio::time::sleep(std::time::Duration::from_millis(3500));
    tokio::pin!(quiet);
    loop {
        tokio::select! {
            _ = &mut quiet => break,
            msg = ws.next() => match msg {
                Some(Ok(Message::Ping(_))) => pings += 1,
                other => panic!("unexpected frame while idle: {:?}", other),
            },
        }
    }
    assert!(pings >= 2, "expected router pings, got {}", pings);
    assert_eq!(state.ws_session_count("ws-tester"), 1);

    ws.send(Message::Text("still here".into())).await.unwrap();
    let reply = loop {
        match ws.next().await.unwrap().unwrap() {
            Message::Ping(_) => continue,
            other => break other,
        }
    };
    assert_eq!(reply.into_text().unwrap(), "echo:still here");
}

#[tokio::test]
async fn ws_unresponsive_client_is_closed_despite_pings() {
    let backend = start_echo_backend().await;
    let (router, state) = start_router_inner(Some(backend), true, |p| {
        p.ws_idle_timeout_secs = 2;
        p.ws_ping_interval_secs = 1;
    })
    .await;

    // Never polling the stream means the client never sends pongs.
    let (_ws, _) = connect_async(format!("ws://{}/?api-key=ws-key", router))
        .await
        .expect("upgrade");
    assert_eq!(state.ws_session_count("ws-tester"), 1);

    tokio::time::sleep(std::time::Duration::from_millis(2500)).await;
    wait_for_slot_release(&state).await;
}

#[tokio::test]
async fn ws_dead_backend_is_closed_despite_live_client_pongs() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let backend_addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (tcp, _) = listener.accept().await.unwrap();
        let ws = accept_async(tcp).await.unwrap();
        std::future::pending::<()>().await;
        drop(ws);
    });
    let (router, state) = start_router_inner(Some(format!("ws://{backend_addr}")), true, |p| {
        p.ws_idle_timeout_secs = 2;
        p.ws_ping_interval_secs = 1;
        p.max_ws_connections_per_key = 1;
    })
    .await;
    let (mut ws, _) = connect_async(format!("ws://{router}/?api-key=ws-key"))
        .await
        .unwrap();
    let frame = tokio::time::timeout(std::time::Duration::from_secs(5), read_until_close(&mut ws))
        .await
        .expect("dead backend must time out even while the client answers pings")
        .expect("close frame");
    assert_eq!(frame.reason, "idle timeout");
    wait_for_slot_release(&state).await;
    server.abort();
}

#[tokio::test]
async fn ws_dead_client_is_closed_despite_active_backend() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (tcp, _) = listener.accept().await.unwrap();
        let mut ws = accept_async(tcp).await.unwrap();
        loop {
            if ws.send(Message::Text("notification".into())).await.is_err() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
    });
    let (router, state) = start_router_inner(Some(format!("ws://{addr}")), true, |p| {
        p.ws_idle_timeout_secs = 2;
        p.ws_ping_interval_secs = 1;
    })
    .await;
    // Never reading means this client does not answer the router's pings.
    let (_ws, _) = connect_async(format!("ws://{router}/?api-key=ws-key"))
        .await
        .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(2500)).await;
    wait_for_slot_release(&state).await;
    server.abort();
}
