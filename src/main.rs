use std::{error::Error, path::PathBuf};

use axum::{body::Body, http::Request};
use nix_store_mesh::{config::Config, remote::Mesh, store::nix_serve_app};
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
    let local_listener = tokio::net::TcpListener::bind(config.local_listen).await?;
    let peer_listener = tokio::net::TcpListener::bind(config.peer_listen).await?;

    let app_local = Mesh::new(&config)
        .router()
        .layer(axum::middleware::from_fn(logging_middleware));
    let app_peer = nix_serve_app()
        .await
        .layer(CompressionLayer::new().quality(CompressionLevel::Fastest))
        .layer(axum::middleware::from_fn(logging_middleware));

    log::debug!("Local cache: http://{}", config.local_listen);
    log::debug!("Peer service: http://{}", config.peer_listen);
    tokio::try_join!(
        async { axum::serve(local_listener, app_local).await },
        async { axum::serve(peer_listener, app_peer).await },
    )?;
    Ok(())
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
