use axum::{
    Router,
    body::Body,
    extract::Path,
    http::{Response, StatusCode, header},
    response::IntoResponse,
    routing::get,
};
use once_cell::sync::Lazy;
use serde::Deserialize;
use std::{env, process::Stdio};
use tokio::process::Command;
use tokio_util::io::ReaderStream;

static STORE_DIR: Lazy<String> =
    Lazy::new(|| env::var("NIX_STORE_DIR").unwrap_or_else(|_| "/nix/store".to_string()));

pub async fn nix_serve_app() -> Router<()> {
    Router::new()
        .without_v07_checks()
        .route("/nix-cache-info", get(nix_cache_info))
        .route("/:hash.narinfo", get(narinfo))
        .route("/nar/:hash-:expected.nar", get(nar_with_hash))
        .route("/nar/:hash.nar", get(nar_without_hash))
        .route("/log/:store_name", get(log_stream))
}

#[derive(Debug, Deserialize)]
struct PathInfo {
    #[serde(rename = "narHash")]
    nar_hash: String,
    #[serde(rename = "narSize")]
    nar_size: u64,
    references: Vec<String>,
    deriver: Option<String>,
    #[serde(default)]
    signatures: Option<Vec<String>>,
}

async fn query_store_path(hash_part: &str) -> Result<String, StatusCode> {
    let output = Command::new("nix")
        .args(["store", "path-from-hash-part", hash_part])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    if !output.status.success() {
        return Err(StatusCode::NOT_FOUND);
    }

    let store_path = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if store_path.is_empty() {
        return Err(StatusCode::NOT_FOUND);
    }

    Ok(store_path)
}

async fn query_path_info(store_path: &str) -> Result<PathInfo, StatusCode> {
    let output = Command::new("nix")
        .args(["path-info", "--json", "--sigs", "--size", store_path])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    if !output.status.success() {
        return Err(StatusCode::INTERNAL_SERVER_ERROR);
    }

    let parsed: serde_json::Value =
        serde_json::from_slice(&output.stdout).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    // The JSON is a map from store path to details
    let entry = parsed
        .as_object()
        .and_then(|m| m.get(store_path))
        .ok_or(StatusCode::INTERNAL_SERVER_ERROR)?;

    serde_json::from_value(entry.clone()).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)
}

async fn nix_cache_info() -> impl IntoResponse {
    let body = format!("StoreDir: {}\nWantMassQuery: 1\nPriority: 30\n", *STORE_DIR);
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/plain")
        .header(header::CONTENT_LENGTH, body.len())
        .body(Body::from(body))
        .unwrap()
}

async fn narinfo(Path(hash_part): Path<String>) -> Result<Response<Body>, StatusCode> {
    let store_path = query_store_path(&hash_part).await?;
    let info = query_path_info(&store_path).await?;

    let nar_hash_short = info
        .nar_hash
        .strip_prefix("sha256:")
        .unwrap_or(&info.nar_hash)
        .to_string();

    let mut res = String::new();
    res.push_str(&format!("StorePath: {}\n", store_path));
    res.push_str(&format!("URL: nar/{}-{}.nar\n", hash_part, nar_hash_short));
    res.push_str("Compression: none\n");
    res.push_str(&format!("NarHash: {}\n", info.nar_hash));
    res.push_str(&format!("NarSize: {}\n", info.nar_size));

    if !info.references.is_empty() {
        let refs: Vec<String> = info.references.iter().map(|r| strip_path(r)).collect();
        res.push_str(&format!("References: {}\n", refs.join(" ")));
    }

    if let Some(deriver) = info.deriver.as_ref() {
        res.push_str(&format!("Deriver: {}\n", strip_path(deriver)));
    }

    if let Some(sigs) = info.signatures.as_ref() {
        for sig in sigs {
            res.push_str(&format!("Sig: {}\n", sig));
        }
    }

    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/x-nix-narinfo")
        .header(header::CONTENT_LENGTH, res.len())
        .body(Body::from(res))
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)
}

async fn nar_with_hash(
    Path((hash_part, expected_hash)): Path<(String, String)>,
) -> Result<Response<Body>, StatusCode> {
    let store_path = query_store_path(&hash_part).await?;
    let info = query_path_info(&store_path).await?;

    let actual = info
        .nar_hash
        .strip_prefix("sha256:")
        .unwrap_or(&info.nar_hash);
    if actual != expected_hash {
        return Err(StatusCode::NOT_FOUND);
    }

    stream_command(
        Command::new("nix")
            .arg("--extra-experimental-features")
            .arg("nix-command")
            .arg("store")
            .arg("dump-path")
            .arg("--")
            .arg(&store_path),
        info.nar_size,
    )
    .await
}

async fn nar_without_hash(Path(hash_part): Path<String>) -> Result<Response<Body>, StatusCode> {
    let store_path = query_store_path(&hash_part).await?;
    let info = query_path_info(&store_path).await?;

    stream_command(
        Command::new("nix")
            .arg("--extra-experimental-features")
            .arg("nix-command")
            .arg("store")
            .arg("dump-path")
            .arg("--")
            .arg(&store_path),
        info.nar_size,
    )
    .await
}

async fn log_stream(Path(store_name): Path<String>) -> Result<Response<Body>, StatusCode> {
    let store_path = format!("{}/{}", *STORE_DIR, store_name);
    stream_command(
        Command::new("nix")
            .arg("--extra-experimental-features")
            .arg("nix-command")
            .arg("log")
            .arg(&store_path),
        None,
    )
    .await
}

fn strip_path(path: &str) -> String {
    path.rsplit('/').next().unwrap_or(path).to_string()
}

async fn stream_command(
    cmd: &mut Command,
    content_length: impl Into<Option<u64>>,
) -> Result<Response<Body>, StatusCode> {
    let mut child = cmd
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    let stdout = child
        .stdout
        .take()
        .ok_or(StatusCode::INTERNAL_SERVER_ERROR)?;

    let stream = ReaderStream::new(stdout);
    let body = Body::from_stream(stream);

    let mut builder = Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/plain");

    if let Some(len) = content_length.into() {
        builder = builder.header(header::CONTENT_LENGTH, len);
    }

    builder
        .body(body)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)
}
