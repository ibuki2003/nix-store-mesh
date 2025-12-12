use axum::{
    Router,
    body::Body,
    http::{Request, Response, StatusCode},
    routing::{any, get},
};
use nix_store_mesh::remote::try_forward;
// use serde::{Deserialize, Serialize};

#[tokio::main]
async fn main() {
    // build our application with a route
    let app = Router::new()
        // `GET /` goes to `root`
        .route("/", get(root))
        .route("/{*path}", any(handler))
        .layer(axum::middleware::from_fn(logging_middleware));

    // run our app with hyper, listening globally on port 3000
    let listener = tokio::net::TcpListener::bind("0.0.0.0:3000").await.unwrap();
    println!("Listening on port 3000");
    axum::serve(listener, app).await.unwrap();
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
