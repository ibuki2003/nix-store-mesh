use super::*;
use axum::http::Version;
use std::{
    net::SocketAddr,
    sync::atomic::{AtomicUsize, Ordering},
};
use tokio::{io::AsyncWriteExt, net::TcpListener, sync::Mutex, task::JoinSet};
use tokio_util::io::ReaderStream;
use tower::ServiceExt;

const HASH: &str = "00000000000000000000000000000000";
const NARINFO: &str = "StorePath: /nix/store/00000000000000000000000000000000-test\nURL: nar/test.nar\nCompression: none\nNarHash: sha256:test\nNarSize: 7\nSig: test:signature\n";

struct TestServer {
    address: SocketAddr,
    connections: Arc<AtomicUsize>,
    task: AbortOnDropHandle<()>,
}

impl TestServer {
    async fn start(app: Router) -> Self {
        Self::bind("127.0.0.1:0".parse().unwrap(), app).await
    }

    async fn bind(address: SocketAddr, app: Router) -> Self {
        let listener = TcpListener::bind(address).await.unwrap();
        let address = listener.local_addr().unwrap();
        let connections = Arc::new(AtomicUsize::new(0));
        let count = connections.clone();
        let task = tokio::spawn(async move {
            let mut tasks = JoinSet::new();
            loop {
                tokio::select! {
                    result = listener.accept() => {
                        let (stream, _) = result.unwrap();
                        count.fetch_add(1, Ordering::SeqCst);
                        let app = app.clone();
                        tasks.spawn(async move {
                            let service = hyper::service::service_fn(move |request| {
                                let app = app.clone();
                                async move { app.oneshot(request).await }
                            });
                            let _ = hyper::server::conn::http2::Builder::new(TokioExecutor::new())
                                .serve_connection(TokioIo::new(stream), service)
                                .await;
                        });
                    }
                    _ = tasks.join_next(), if !tasks.is_empty() => {}
                }
            }
        });
        Self {
            address,
            connections,
            task: AbortOnDropHandle::new(task),
        }
    }

    async fn stop(&mut self) {
        self.task.abort();
        let _ = (&mut self.task).await;
    }
}

fn app(narinfo: Option<&'static str>, nar: &'static str) -> Router {
    let app = Router::new()
        .route(
            "/nix-cache-info",
            get(|| async { "StoreDir: /nix/store\n" }),
        )
        .route("/nar/test.nar", get(move || async move { nar }));
    match narinfo {
        Some(body) => app.route(
            &format!("/{HASH}.narinfo"),
            get(move || async move { body }),
        ),
        None => app,
    }
}

fn mesh(peers: &[(&str, SocketAddr)]) -> Mesh {
    Mesh {
        peers: Arc::new(
            peers
                .iter()
                .map(|(name, address)| {
                    (
                        name.to_string(),
                        Peer::spawn(
                            name.to_string(),
                            address.to_string().parse().unwrap(),
                            |ip| ip.is_loopback(),
                        ),
                    )
                })
                .collect(),
        ),
    }
}

async fn wait_online(mesh: &Mesh) {
    timeout(Duration::from_secs(5), async {
        while mesh.peers.values().any(|peer| peer.connection().is_none()) {
            sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("peers should connect");
}

fn request(path: &str) -> Request<Body> {
    Request::builder().uri(path).body(Body::empty()).unwrap()
}

async fn body(response: Response<Body>) -> String {
    String::from_utf8(
        to_bytes(response.into_body(), 2 * 1024 * 1024)
            .await
            .unwrap()
            .to_vec(),
    )
    .unwrap()
}

#[test]
fn rewrites_only_the_nar_url() {
    let rewritten = rewrite_narinfo(NARINFO, "desktop").unwrap();
    assert_eq!(
        rewritten,
        NARINFO.replace("URL: nar/test.nar", "URL: nar/desktop/test.nar")
    );
    for url in [
        "http://elsewhere/nar/test.nar",
        "//elsewhere/nar/test.nar",
        "/nar/test.nar",
        "nar/../test.nar",
        "nar/test.nar?redirect=elsewhere",
    ] {
        assert!(rewrite_narinfo(&format!("URL: {url}\n"), "desktop").is_none());
    }
    assert!(rewrite_narinfo("StorePath: /nix/store/test\n", "desktop").is_none());
    assert!(rewrite_narinfo(&format!("{NARINFO}URL: nar/other.nar\n"), "desktop").is_none());
}

#[tokio::test]
async fn refuses_non_tailnet_destinations_before_connecting() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let authority = listener.local_addr().unwrap().to_string().parse().unwrap();
    let error = connect(&authority, is_tailnet_ip).await.unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::AddrNotAvailable);
    assert!(
        timeout(Duration::from_millis(50), listener.accept())
            .await
            .is_err()
    );
}

#[tokio::test]
async fn offline_requests_do_not_wait_for_connection_attempts() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mesh = mesh(&[("silent", listener.local_addr().unwrap())]);
    // Accept TCP, but never answer the HTTP/2 preface or startup request.
    let (_stream, _) = timeout(Duration::from_secs(1), listener.accept())
        .await
        .unwrap()
        .unwrap();
    let router = mesh.router();
    for _ in 0..3 {
        let response = timeout(
            Duration::from_millis(100),
            router.clone().oneshot(request(&format!("/{HASH}.narinfo"))),
        )
        .await
        .expect("offline peers must not delay Nix requests")
        .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }
}

#[tokio::test]
async fn reuses_one_http2_connection_for_concurrent_requests() {
    let server = TestServer::start(app(Some(NARINFO), "payload")).await;
    let mesh = mesh(&[("source", server.address)]);
    wait_online(&mesh).await;
    let router = mesh.router();
    let mut tasks = JoinSet::new();
    for _ in 0..8 {
        let router = router.clone();
        tasks.spawn(async move {
            let response = router
                .oneshot(request(&format!("/{HASH}.narinfo")))
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            assert!(body(response).await.contains("URL: nar/source/test.nar\n"));
        });
    }
    while let Some(result) = tasks.join_next().await {
        result.unwrap();
    }
    assert_eq!(server.connections.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn falls_back_on_missing_or_malformed_narinfo_and_pins_the_source() {
    let missing = TestServer::start(app(None, "wrong-a")).await;
    let malformed =
        TestServer::start(app(Some("URL: http://elsewhere/test.nar\n"), "wrong-b")).await;
    let source = TestServer::start(app(Some(NARINFO), "payload")).await;
    let mesh = mesh(&[
        ("a-missing", missing.address),
        ("b-malformed", malformed.address),
        ("c-source", source.address),
    ]);
    wait_online(&mesh).await;
    let router = mesh.router();
    let response = router
        .clone()
        .oneshot(request(&format!("/{HASH}.narinfo")))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        body(response).await,
        NARINFO.replace("nar/test.nar", "nar/c-source/test.nar")
    );
    let response = router
        .clone()
        .oneshot(request("/nar/c-source/test.nar"))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(body(response).await, "payload");
    assert_eq!(
        router
            .oneshot(request("/nar/unknown/test.nar"))
            .await
            .unwrap()
            .status(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(source.connections.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn pending_nar_does_not_block_metadata_and_releases_its_transfer_slot() {
    let (reader, mut writer) = tokio::io::duplex(64);
    let reader = Arc::new(Mutex::new(Some(reader)));
    let app = app(Some(NARINFO), "unused").route(
        "/nar/slow.nar",
        get(move || {
            let reader = reader.clone();
            async move {
                let reader = reader.lock().await.take().unwrap();
                Body::from_stream(ReaderStream::new(reader))
            }
        }),
    );
    let server = TestServer::start(app).await;
    let mesh = mesh(&[("source", server.address)]);
    wait_online(&mesh).await;
    let connection = mesh.peers["source"].connection().unwrap();
    let router = mesh.router();
    let response = router
        .clone()
        .oneshot(request("/nar/source/slow.nar"))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(connection.transfers.available_permits(), MAX_TRANSFERS - 1);
    let metadata = timeout(
        Duration::from_secs(1),
        router.oneshot(request(&format!("/{HASH}.narinfo"))),
    )
    .await
    .expect("pending NAR must not block metadata")
    .unwrap();
    assert_eq!(metadata.status(), StatusCode::OK);
    writer.write_all(b"streamed").await.unwrap();
    drop(writer);
    assert_eq!(body(response).await, "streamed");
    assert_eq!(connection.transfers.available_permits(), MAX_TRANSFERS);
    assert_eq!(server.connections.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn preserves_encoded_nar_bytes_and_headers() {
    let app = app(Some(NARINFO), "unused").route(
        "/nar/encoded.nar",
        get(|request: Request<Body>| async move {
            assert_eq!(request.version(), Version::HTTP_2);
            assert_eq!(request.headers()[header::ACCEPT_ENCODING], "gzip");
            Response::builder()
                .header(header::CONTENT_ENCODING, "gzip")
                .header(header::CONTENT_LENGTH, "7")
                .body(Body::from("encoded"))
                .unwrap()
        }),
    );
    let server = TestServer::start(app).await;
    let mesh = mesh(&[("source", server.address)]);
    wait_online(&mesh).await;
    let response = mesh
        .router()
        .oneshot(
            Request::builder()
                .uri("/nar/source/encoded.nar")
                .header(header::ACCEPT_ENCODING, "gzip")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()[header::CONTENT_ENCODING], "gzip");
    assert_eq!(response.headers()[header::CONTENT_LENGTH], "7");
    assert_eq!(body(response).await, "encoded");
}

#[tokio::test]
async fn marks_an_idle_blackholed_connection_offline_using_http2_ping() {
    let server = TestServer::start(app(Some(NARINFO), "payload")).await;
    let proxy = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = proxy.local_addr().unwrap();
    let blackhole = CancellationToken::new();
    let stop_forwarding = blackhole.clone();
    let _proxy_task = AbortOnDropHandle::new(tokio::spawn(async move {
        let (mut client, _) = proxy.accept().await.unwrap();
        let mut upstream = TcpStream::connect(server.address).await.unwrap();
        tokio::select! {
            _ = tokio::io::copy_bidirectional(&mut client, &mut upstream) => return,
            _ = stop_forwarding.cancelled() => {}
        }
        // Keep both TCP sockets open, but silently stop forwarding either direction.
        std::future::pending::<()>().await;
        drop((client, upstream));
    }));
    let mesh = mesh(&[("source", address)]);
    wait_online(&mesh).await;
    let connection = mesh.peers["source"].connection().unwrap();
    blackhole.cancel();
    tokio::task::yield_now().await;
    tokio::time::pause();
    for _ in 0..30 {
        tokio::time::advance(Duration::from_secs(1)).await;
        for _ in 0..10 {
            tokio::task::yield_now().await;
        }
        if connection.failed.is_cancelled() {
            break;
        }
    }
    assert!(
        connection.failed.is_cancelled(),
        "idle peer must fail its PING deadline"
    );
    assert!(mesh.peers["source"].connection().is_none());
}

#[tokio::test]
async fn axum_peer_listener_supports_plain_http2() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let _server = AbortOnDropHandle::new(tokio::spawn(async move {
        axum::serve(listener, app(Some(NARINFO), "payload"))
            .await
            .unwrap();
    }));
    let mesh = mesh(&[("source", address)]);
    wait_online(&mesh).await;
    let response = mesh
        .router()
        .oneshot(request(&format!("/{HASH}.narinfo")))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
}

#[tokio::test]
async fn reconnects_in_background_after_disconnect() {
    let mut server = TestServer::start(app(Some(NARINFO), "payload")).await;
    let mesh = mesh(&[("source", server.address)]);
    wait_online(&mesh).await;
    let previous = mesh.peers["source"].connection().unwrap();
    server.stop().await;
    timeout(Duration::from_secs(2), async {
        while mesh.peers["source"].connection().is_some() {
            sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("connection closure must mark peer offline");
    let response = mesh
        .clone()
        .router()
        .oneshot(request(&format!("/{HASH}.narinfo")))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    let replacement = TestServer::bind(server.address, app(Some(NARINFO), "payload")).await;
    wait_online(&mesh).await;
    assert!(!Arc::ptr_eq(
        &previous,
        &mesh.peers["source"].connection().unwrap()
    ));
    let response = mesh
        .router()
        .oneshot(request(&format!("/{HASH}.narinfo")))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(replacement.connections.load(Ordering::SeqCst), 1);
}
