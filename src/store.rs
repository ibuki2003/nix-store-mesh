use axum::{
    Router,
    body::Body,
    http::{Request, Response, StatusCode, header},
    response::IntoResponse,
    routing::{any, get},
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
        .route("/nix-cache-info", get(nix_cache_info))
        .route("/{*path}", any(handler))
}

async fn handler(req: Request<Body>) -> Result<Response<Body>, StatusCode> {
    let path = req.uri().path();

    if let Some(hash_part) = parse_narinfo(path) {
        return narinfo(&hash_part).await;
    }

    if let Some((hash_part, expected_hash)) = parse_nar_with_hash(path) {
        return nar_stream(&hash_part, Some(expected_hash)).await;
    }

    if let Some(hash_part) = parse_nar_without_hash(path) {
        return nar_stream(&hash_part, None).await;
    }

    if let Some(store_name) = parse_log(path) {
        return log_stream(&store_name).await;
    }

    Err(StatusCode::NOT_FOUND)
}

fn parse_narinfo(path: &str) -> Option<String> {
    if !path.ends_with(".narinfo") {
        return None;
    }
    let trimmed = path.trim_start_matches('/');
    trimmed.strip_suffix(".narinfo").map(|s| s.to_string())
}

fn parse_nar_with_hash(path: &str) -> Option<(String, String)> {
    if !path.starts_with("/nar/") || !path.ends_with(".nar") {
        return None;
    }
    let body = &path[5..path.len() - 4]; // strip "/nar/" and ".nar"
    let (hash_part, expected_hash) = body.split_once('-')?;
    Some((hash_part.to_string(), expected_hash.to_string()))
}

fn parse_nar_without_hash(path: &str) -> Option<String> {
    if !path.starts_with("/nar/") || !path.ends_with(".nar") {
        return None;
    }
    let body = &path[5..path.len() - 4];
    if body.contains('-') {
        return None; // handled by parse_nar_with_hash
    }
    Some(body.to_string())
}

fn parse_log(path: &str) -> Option<String> {
    if !path.starts_with("/log/") {
        return None;
    }
    let body = &path[5..];
    if body.is_empty() {
        return None;
    }
    Some(body.to_string())
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

async fn narinfo(hash_part: &str) -> Result<Response<Body>, StatusCode> {
    let store_path = query_store_path(hash_part).await?;
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

async fn nar_stream(
    hash_part: &str,
    expected_hash: Option<String>,
) -> Result<Response<Body>, StatusCode> {
    let store_path = query_store_path(hash_part).await?;
    let info = query_path_info(&store_path).await?;

    if let Some(expected) = expected_hash.as_ref() {
        let actual = info
            .nar_hash
            .strip_prefix("sha256:")
            .unwrap_or(&info.nar_hash);
        if actual != expected {
            return Err(StatusCode::NOT_FOUND);
        }
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

async fn log_stream(store_name: &str) -> Result<Response<Body>, StatusCode> {
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
