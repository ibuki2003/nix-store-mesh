use axum::{
    Router,
    body::{Body, to_bytes},
    extract::{OriginalUri, Path, State},
    http::{HeaderValue, Request, Response, StatusCode, Uri, header, uri::Authority},
    routing::get,
};
use hyper::{body::Incoming, client::conn::http2};
use hyper_util::rt::{TokioExecutor, TokioIo, TokioTimer};
use std::{
    collections::BTreeMap,
    io,
    net::IpAddr,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
    time::Duration,
};
use tokio::{
    net::{TcpStream, lookup_host},
    sync::{OwnedSemaphorePermit, Semaphore, watch},
    time::{sleep, timeout},
};
use tokio_util::{sync::CancellationToken, task::AbortOnDropHandle};

use crate::config::{Config, is_tailnet_ip};

#[cfg(test)]
mod tests;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(2);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
const PING_INTERVAL: Duration = Duration::from_secs(5);
const PING_TIMEOUT: Duration = Duration::from_secs(15);
const MAX_RETRY_DELAY: Duration = Duration::from_secs(30);
const MAX_NARINFO_SIZE: usize = 1024 * 1024;
const MAX_TRANSFERS: usize = 4;

type PeerResponse = Response<Incoming>;

struct Connection {
    sender: http2::SendRequest<Body>,
    failed: CancellationToken,
    transfers: Arc<Semaphore>,
}

struct Peer {
    authority: Authority,
    state: watch::Receiver<Option<Arc<Connection>>>,
    _task: AbortOnDropHandle<()>,
}

impl Peer {
    fn spawn(name: String, authority: Authority, allowed_ip: fn(IpAddr) -> bool) -> Self {
        let (state_tx, state) = watch::channel(None);
        let address = authority.clone();
        let task = tokio::spawn(async move {
            let mut retry_delay = Duration::from_secs(1);
            loop {
                if maintain_connection(&name, &address, &state_tx, allowed_ip).await {
                    retry_delay = Duration::from_secs(1);
                }
                state_tx.send_replace(None);
                sleep(retry_delay).await;
                retry_delay = (retry_delay * 2).min(MAX_RETRY_DELAY);
            }
        });
        Self {
            authority,
            state,
            _task: AbortOnDropHandle::new(task),
        }
    }

    fn connection(&self) -> Option<Arc<Connection>> {
        self.state.borrow().clone().filter(|connection| {
            !connection.failed.is_cancelled() && !connection.sender.is_closed()
        })
    }

    async fn request(
        &self,
        connection: &Connection,
        path: &str,
        accept_encoding: Option<HeaderValue>,
    ) -> Option<PeerResponse> {
        let mut request = Request::builder().uri(format!("http://{}{path}", self.authority));
        if let Some(encoding) = accept_encoding {
            request = request.header(header::ACCEPT_ENCODING, encoding);
        }
        let request = request.body(Body::empty()).ok()?;
        let mut sender = connection.sender.clone();
        tokio::select! {
            _ = connection.failed.cancelled() => None,
            result = timeout(REQUEST_TIMEOUT, sender.send_request(request)) => {
                match result {
                    Ok(Ok(response)) => Some(response),
                    _ => {
                        connection.failed.cancel();
                        None
                    }
                }
            }
        }
    }
}

async fn connect(authority: &Authority, allowed_ip: fn(IpAddr) -> bool) -> io::Result<TcpStream> {
    let mut last_error = io::Error::new(
        io::ErrorKind::AddrNotAvailable,
        "peer did not resolve to an allowed address",
    );
    for address in lookup_host(authority.as_str()).await? {
        if !allowed_ip(address.ip()) {
            continue;
        }
        match TcpStream::connect(address).await {
            Ok(stream) => {
                stream.set_nodelay(true)?;
                return Ok(stream);
            }
            Err(error) => last_error = error,
        }
    }
    Err(last_error)
}

// Returns true if the peer reached the online state, resetting the reconnect backoff.
async fn maintain_connection(
    name: &str,
    authority: &Authority,
    state: &watch::Sender<Option<Arc<Connection>>>,
    allowed_ip: fn(IpAddr) -> bool,
) -> bool {
    let stream = match timeout(CONNECT_TIMEOUT, connect(authority, allowed_ip)).await {
        Ok(Ok(stream)) => stream,
        result => {
            log::debug!("Peer {name}: connection failed: {result:?}");
            return false;
        }
    };
    let mut builder = http2::Builder::new(TokioExecutor::new());
    builder
        .timer(TokioTimer::new())
        .adaptive_window(true)
        .keep_alive_interval(PING_INTERVAL)
        .keep_alive_timeout(PING_TIMEOUT)
        .keep_alive_while_idle(true);
    let (mut sender, driver) = match timeout(
        CONNECT_TIMEOUT,
        builder.handshake::<_, Body>(TokioIo::new(stream)),
    )
    .await
    {
        Ok(Ok(connection)) => connection,
        Ok(Err(error)) => {
            log::debug!("Peer {name}: HTTP/2 handshake failed: {error}");
            return false;
        }
        Err(_) => {
            log::debug!("Peer {name}: HTTP/2 handshake timed out");
            return false;
        }
    };
    tokio::pin!(driver);
    let request = Request::builder()
        .uri(format!("http://{authority}/nix-cache-info"))
        .body(Body::empty())
        .expect("valid peer URI");
    let probe = timeout(REQUEST_TIMEOUT, sender.send_request(request));
    let online = tokio::select! {
        result = &mut driver => {
            log::debug!("Peer {name}: disconnected during startup: {result:?}");
            false
        }
        result = probe => matches!(result, Ok(Ok(response)) if response.status().is_success()),
    };
    if !online {
        return false;
    }
    let failed = CancellationToken::new();
    state.send_replace(Some(Arc::new(Connection {
        sender,
        failed: failed.clone(),
        transfers: Arc::new(Semaphore::new(MAX_TRANSFERS)),
    })));
    log::info!("Peer {name}: connected");
    tokio::select! {
        result = &mut driver => log::debug!("Peer {name}: connection ended: {result:?}"),
        _ = failed.cancelled() => log::debug!("Peer {name}: reconnecting after request failure"),
    }
    log::info!("Peer {name}: disconnected");
    failed.cancel();
    true
}

#[derive(Clone)]
pub struct Mesh {
    peers: Arc<BTreeMap<String, Peer>>,
}

impl Mesh {
    pub fn new(config: &Config) -> Self {
        let peers = config
            .peers
            .iter()
            .map(|(name, address)| {
                let peer = Peer::spawn(
                    name.clone(),
                    address.parse().expect("validated peer address"),
                    is_tailnet_ip,
                );
                (name.clone(), peer)
            })
            .collect();
        Self {
            peers: Arc::new(peers),
        }
    }

    pub fn router(self) -> Router {
        Router::new()
            .route("/nix-cache-info", get(crate::store::nix_cache_info))
            .route("/nar/{peer}/{*path}", get(forward_nar))
            .route("/log/{store_name}", get(forward_log))
            .route("/{path}", get(find_narinfo))
            .with_state(self)
    }
}

async fn find_narinfo(
    State(mesh): State<Mesh>,
    Path(path): Path<String>,
) -> Result<Response<Body>, StatusCode> {
    let Some(hash) = path.strip_suffix(".narinfo") else {
        return Err(StatusCode::NOT_FOUND);
    };
    if hash.len() != 32
        || !hash
            .bytes()
            .all(|b| b"0123456789abcdfghijklmnpqrsvwxyz".contains(&b))
    {
        return Err(StatusCode::NOT_FOUND);
    }
    for (name, peer) in mesh.peers.iter() {
        let Some(connection) = peer.connection() else {
            continue;
        };
        let Some(response) = peer.request(&connection, &format!("/{path}"), None).await else {
            continue;
        };
        if !response.status().is_success() {
            continue;
        }
        let bytes = match timeout(
            REQUEST_TIMEOUT,
            to_bytes(Body::new(response.into_body()), MAX_NARINFO_SIZE),
        )
        .await
        {
            Ok(Ok(bytes)) => bytes,
            _ => {
                connection.failed.cancel();
                continue;
            }
        };
        let Some(body) = std::str::from_utf8(&bytes)
            .ok()
            .and_then(|body| rewrite_narinfo(body, name))
        else {
            continue;
        };
        return Response::builder()
            .header(header::CONTENT_TYPE, "text/x-nix-narinfo")
            .body(Body::from(body))
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR);
    }
    Err(StatusCode::NOT_FOUND)
}

fn rewrite_narinfo(body: &str, peer: &str) -> Option<String> {
    let mut result = String::new();
    let mut found_url = false;
    for line in body.lines() {
        if let Some(url) = line.strip_prefix("URL: ") {
            if found_url {
                return None;
            }
            let nar = url.strip_prefix("nar/")?;
            let uri: Uri = format!("/{url}").parse().ok()?;
            if nar.is_empty()
                || uri.query().is_some()
                || uri.path().split('/').any(|segment| segment == "..")
            {
                return None;
            }
            result.push_str(&format!("URL: nar/{peer}/{nar}\n"));
            found_url = true;
        } else {
            result.push_str(line);
            result.push('\n');
        }
    }
    found_url.then_some(result)
}

async fn forward_nar(
    State(mesh): State<Mesh>,
    Path((name, _)): Path<(String, String)>,
    OriginalUri(uri): OriginalUri,
    request: Request<Body>,
) -> Result<Response<Body>, StatusCode> {
    let peer = mesh.peers.get(&name).ok_or(StatusCode::NOT_FOUND)?;
    let connection = peer.connection().ok_or(StatusCode::NOT_FOUND)?;
    let permit = tokio::select! {
        permit = connection.transfers.clone().acquire_owned() => {
            permit.map_err(|_| StatusCode::NOT_FOUND)?
        }
        _ = connection.failed.cancelled() => return Err(StatusCode::NOT_FOUND),
    };
    let prefix = format!("/nar/{name}/");
    let suffix = uri
        .path()
        .strip_prefix(&prefix)
        .ok_or(StatusCode::NOT_FOUND)?;
    let response = peer
        .request(
            &connection,
            &format!("/nar/{suffix}"),
            request.headers().get(header::ACCEPT_ENCODING).cloned(),
        )
        .await
        .ok_or(StatusCode::NOT_FOUND)?;
    let (parts, body) = response.into_parts();
    Ok(Response::from_parts(
        parts,
        Body::new(TransferBody {
            inner: body,
            _permit: permit,
        }),
    ))
}

async fn forward_log(
    State(mesh): State<Mesh>,
    OriginalUri(uri): OriginalUri,
) -> Result<Response<Body>, StatusCode> {
    for peer in mesh.peers.values() {
        let Some(connection) = peer.connection() else {
            continue;
        };
        if let Some(response) = peer.request(&connection, uri.path(), None).await
            && response.status().is_success()
        {
            return Ok(response.map(Body::new));
        }
    }
    Err(StatusCode::NOT_FOUND)
}

struct TransferBody {
    inner: Incoming,
    _permit: OwnedSemaphorePermit,
}

impl hyper::body::Body for TransferBody {
    type Data = <Incoming as hyper::body::Body>::Data;
    type Error = hyper::Error;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<hyper::body::Frame<Self::Data>, Self::Error>>> {
        Pin::new(&mut self.inner).poll_frame(cx)
    }

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }

    fn size_hint(&self) -> hyper::body::SizeHint {
        self.inner.size_hint()
    }
}
