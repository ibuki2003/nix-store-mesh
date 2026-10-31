use std::{error::Error, net::SocketAddr, path::PathBuf};

use axum::{Router, body::Body, extract::ConnectInfo, http::Request};
use nix_store_mesh::{config::Config, remote::Mesh, store::nix_serve_app};
use tower::{ServiceExt as _, service_fn};
use tower_http::compression::{CompressionLayer, CompressionLevel};

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    env_logger::Builder::from_env(
        env_logger::Env::default().default_filter_or("warn,nix_store_mesh=info"),
    )
    .format_timestamp(None)
    .init();

    let mut args = std::env::args_os().skip(1);
    let path = args.next().map(PathBuf::from);
    if args.next().is_some() {
        return Err("usage: nix-store-mesh [config.toml]".into());
    }
    let path = match path {
        Some(path) => path,
        None => default_config_path(
            std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from),
            std::env::var_os("HOME").map(PathBuf::from),
        )?,
    };
    let config = Config::load(path).await?;
    let listener = tokio::net::TcpListener::bind(config.listen).await?;
    let hostname = hostname::get()?
        .into_string()
        .map_err(|_| "hostname is not UTF-8")?;
    let app_local = Mesh::new(&config, &hostname).router();
    let app_peer = nix_serve_app()
        .await
        .layer(CompressionLayer::new().quality(CompressionLevel::Fastest));

    let app =
        dispatch_app(app_local, app_peer).layer(axum::middleware::from_fn(logging_middleware));
    log::debug!("Listening on {} as {hostname}", config.listen);
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .await?;
    Ok(())
}

fn dispatch_app(local: Router, peer: Router) -> Router {
    Router::new().fallback_service(service_fn(move |req: Request<Body>| {
        let is_loopback = req
            .extensions()
            .get::<ConnectInfo<SocketAddr>>()
            .is_some_and(|ConnectInfo(addr)| match addr.ip() {
                std::net::IpAddr::V4(ip) => ip.is_loopback(),
                std::net::IpAddr::V6(ip) => ip
                    .to_ipv4_mapped()
                    .map_or_else(|| ip.is_loopback(), |ip| ip.is_loopback()),
            });
        let app = if is_loopback {
            local.clone()
        } else {
            peer.clone()
        };
        async move { app.oneshot(req).await }
    }))
}

fn default_config_path(
    xdg_config_home: Option<PathBuf>,
    home: Option<PathBuf>,
) -> Result<PathBuf, &'static str> {
    let base = xdg_config_home
        .filter(|path| path.is_absolute())
        .or_else(|| {
            home.filter(|path| path.is_absolute())
                .map(|path| path.join(".config"))
        })
        .ok_or("cannot locate config: set XDG_CONFIG_HOME or HOME, or specify a config path")?;
    Ok(base.join("nix-store-mesh/config.toml"))
}

pub async fn logging_middleware(
    req: Request<Body>,
    next: axum::middleware::Next,
) -> impl axum::response::IntoResponse {
    log::trace!("Received request: {} {}", req.method(), req.uri().path());
    next.run(req).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn dispatches_only_loopback_to_the_local_cache() {
        let local = Router::new().route("/test", axum::routing::get(|| async { "local" }));
        let peer = Router::new().route("/test", axum::routing::get(|| async { "peer" }));
        let app = dispatch_app(local, peer);
        for (address, expected) in [
            ("127.0.0.1:1234", "local"),
            ("[::1]:1234", "local"),
            ("[::ffff:127.0.0.1]:1234", "local"),
            ("100.100.1.2:1234", "peer"),
            ("192.168.1.2:1234", "peer"),
        ] {
            let mut request = Request::builder().uri("/test").body(Body::empty()).unwrap();
            request
                .extensions_mut()
                .insert(ConnectInfo(address.parse::<SocketAddr>().unwrap()));
            let response = app.clone().oneshot(request).await.unwrap();
            let bytes = axum::body::to_bytes(response.into_body(), 100)
                .await
                .unwrap();
            assert_eq!(bytes.as_ref(), expected.as_bytes(), "{address}");
        }
    }

    #[test]
    fn default_config_uses_xdg_then_home() {
        let home = Some(PathBuf::from("/home/test"));
        assert_eq!(
            default_config_path(Some("/custom/config".into()), home.clone()).unwrap(),
            PathBuf::from("/custom/config/nix-store-mesh/config.toml")
        );
        for xdg in [None, Some(PathBuf::new()), Some("relative".into())] {
            assert_eq!(
                default_config_path(xdg, home.clone()).unwrap(),
                PathBuf::from("/home/test/.config/nix-store-mesh/config.toml")
            );
        }
        assert!(default_config_path(None, None).is_err());
    }
}
