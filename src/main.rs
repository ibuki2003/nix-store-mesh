use std::net::SocketAddr;

use axum::{
    Router,
    body::Body,
    extract::ConnectInfo,
    http::{Request, Response, StatusCode},
    routing::{any, get},
};

use nix_store_mesh::{remote::try_forward, store::nix_serve_app};
use tower::{ServiceExt as _, service_fn};
use tower_http::compression::{CompressionLayer, CompressionLevel};

#[tokio::main]
async fn main() {
    // service for local; proxy
    let app_local = Router::new()
        .route("/", get(root))
        .route("/{*path}", any(handler));

    // service for remote; nix-serve
    let app_remote = nix_serve_app()
        .await
        .layer(CompressionLayer::new().quality(CompressionLevel::Default));

    let dispatch = service_fn(move |req: Request<Body>| {
        let local = app_local.clone();
        let external = app_remote.clone();
        async move {
            let is_loopback = req
                .extensions()
                .get::<ConnectInfo<SocketAddr>>()
                .map(|ConnectInfo(addr)| addr.ip().is_loopback())
                .unwrap_or(false);

            if is_loopback {
                local.oneshot(req).await
            } else {
                external.oneshot(req).await
            }
        }
    });

    let app = Router::new()
        .fallback_service(dispatch)
        .layer(axum::middleware::from_fn(logging_middleware));

    let listener = tokio::net::TcpListener::bind("0.0.0.0:3000").await.unwrap();
    println!("Listening on port 3000");
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .await
    .unwrap();
}

pub async fn logging_middleware(
    req: Request<Body>,
    next: axum::middleware::Next,
) -> impl axum::response::IntoResponse {
    let path = req.uri().path().to_owned();
    let method = req.method().clone();

    println!("Received request: {} {}", method, path);

    next.run(req).await
}

// basic handler that responds with a static string
async fn root() -> (StatusCode, &'static str) {
    (StatusCode::OK, "Hello, World!")
}

async fn handler(req: Request<Body>) -> Result<Response<Body>, StatusCode> {
    // handle the request
    match try_forward(req).await {
        Some(response) => Ok(response),
        None => Err(StatusCode::NOT_FOUND),
    }
}
