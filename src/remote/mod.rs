use axum::{
    body::Body,
    http::{Request, Response},
};
use once_cell::sync::Lazy;
use std::collections::HashMap;
use std::time::Instant;
use tokio::sync::{Mutex, oneshot};

const REACHABLE_CACHE_TTL_SECS: u64 = 300;
const REACHABLE_TIMEOUT_MS: u64 = 500;

static CLIENT: Lazy<reqwest::Client> = Lazy::new(|| {
    reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_millis(REACHABLE_TIMEOUT_MS))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap()
});

struct CacheEntry {
    value: bool,
    checked_at: Instant,
    checking: bool,
    waiters: Vec<oneshot::Sender<bool>>,
}

static CACHE: Lazy<Mutex<HashMap<String, CacheEntry>>> = Lazy::new(|| Mutex::new(HashMap::new()));

async fn check_reachable(host: &str) -> bool {
    // at first, check the host is valid
    if host.is_empty() {
        return false;
    }
    if host.contains('/') || host.contains('\\') {
        return false;
    }

    // HTTP GET {host}/nix-store-info with timeout
    let req = CLIENT
        .get(format!("http://{}/nix-cache-info", host))
        .build()
        .unwrap();
    let res = CLIENT.execute(req).await;
    match res {
        Ok(r) => r.status().is_success(),
        Err(e) => {
            eprintln!("{:?}", e);
            false
        }
    }
}

async fn refresh_task(host: String) -> bool {
    {
        let mut cache = CACHE.lock().await;
        let entry = cache.get_mut(&host);
        if let Some(e) = entry {
            if e.checking {
                // someone else is checking: wait for its result
                let (tx, rx) = oneshot::channel();
                e.waiters.push(tx);
                drop(cache);
                return rx.await.unwrap_or(false);
            }
            e.checking = true;
        } else {
            // insert new entry
            cache.insert(
                host.clone(),
                CacheEntry {
                    value: false,
                    checked_at: Instant::now(),
                    checking: true,
                    waiters: Vec::new(),
                },
            );
        }
    }

    let res = check_reachable(&host).await;
    let mut cache = CACHE.lock().await;
    if let Some(e) = cache.get_mut(&host) {
        e.value = res;
        e.checked_at = Instant::now();
        e.checking = false;
        for s in e.waiters.drain(..) {
            let _ = s.send(res);
        }
    } else {
        cache.insert(
            host.clone(),
            CacheEntry {
                value: res,
                checked_at: Instant::now(),
                checking: false,
                waiters: Vec::new(),
            },
        );
    }
    res
}

async fn invalidate_cache(host: &str) {
    let mut cache = CACHE.lock().await;
    cache.remove(host);
}

/// Checks if the given host is reachable over the network.
/// results are cached for performance.
async fn reachable(host: &str) -> bool {
    // validate host quickly
    if host.is_empty() || host.contains('/') || host.contains('\\') {
        return false;
    }

    let now = Instant::now();
    let ttl = std::time::Duration::from_secs(REACHABLE_CACHE_TTL_SECS);

    // Scope for lock
    {
        let mut cache = CACHE.lock().await;
        if let Some(entry) = cache.get_mut(host)
            && entry.checked_at + ttl > now
        {
            // cache still valid: return cached value immediately
            if !entry.value {
                // refresh in background
                tokio::spawn(refresh_task(host.to_string()));
            }
            entry.value
        } else {
            // cache miss
            drop(cache);
            refresh_task(host.to_string()).await
        }
    }
}

pub async fn try_forward(req: Request<Body>) -> Option<Response<Body>> {
    let path = req.uri().path();
    let path = &path[1..]; // strip leading '/'
    println!("{}", path);
    println!(
        "accept-encoding: {:?}",
        req.headers().get("accept-encoding")
    );
    let host = path.split_once('/')?.0;
    if !reachable(host).await {
        println!("Host {} is not reachable", host);
        return None;
    }
    println!("Forwarding request to host: {}", host);

    let freq = CLIENT
        .request(req.method().clone(), format!("http://{}", path))
        .build()
        .unwrap();

    let res = match CLIENT.execute(freq).await {
        Ok(r) => r,
        Err(e) => {
            if e.is_connect() {
                // invalidate cache
                invalidate_cache(host).await;
            }
            return None;
        }
    };

    let mut builder = Response::builder().status(res.status());
    for (k, v) in res.headers() {
        builder = builder.header(k, v);
    }
    let body = Body::from_stream(res.bytes_stream());
    builder.body(body).ok()
}
