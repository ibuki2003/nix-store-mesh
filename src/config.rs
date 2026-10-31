use axum::http::uri::Authority;
use serde::{Deserialize, Deserializer, de::Error as _};
use std::{collections::BTreeMap, error::Error, net::IpAddr, net::SocketAddr, path::Path};

const DEFAULT_PORT: u16 = 9722;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(deserialize_with = "deserialize_listen")]
    pub listen: SocketAddr,
    #[serde(default, deserialize_with = "deserialize_peers")]
    pub hosts: BTreeMap<String, String>,
}

fn deserialize_listen<'de, D: Deserializer<'de>>(deserializer: D) -> Result<SocketAddr, D::Error> {
    let address = String::deserialize(deserializer)?;
    address
        .parse::<IpAddr>()
        .map(|ip| SocketAddr::new(ip, DEFAULT_PORT))
        .or_else(|_| address.parse::<SocketAddr>())
        .map_err(D::Error::custom)
}

fn deserialize_peers<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<BTreeMap<String, String>, D::Error> {
    BTreeMap::<String, String>::deserialize(deserializer)?
        .into_iter()
        .map(|(name, address)| {
            let authority: Authority = address.parse().map_err(D::Error::custom)?;
            let address = if authority.port().is_none() {
                format!("{authority}:{DEFAULT_PORT}")
            } else {
                address
            };
            Ok((name, address))
        })
        .collect()
}

impl Config {
    pub async fn load(path: impl AsRef<Path>) -> Result<Self, Box<dyn Error>> {
        let config: Self = toml::from_str(&tokio::fs::read_to_string(path).await?)?;
        config.validate()?;
        Ok(config)
    }

    fn validate(&self) -> Result<(), String> {
        for (name, address) in &self.hosts {
            if name.is_empty()
                || !name
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
            {
                return Err(format!("invalid peer name: {name}"));
            }
            let authority: Authority = address
                .parse()
                .map_err(|_| format!("invalid address for peer {name}: {address}"))?;
            if authority.port_u16().is_none() || authority.as_str().contains('@') {
                return Err(format!("peer {name} must have a host:port address"));
            }
        }
        Ok(())
    }
}

pub(crate) fn is_tailnet_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => u32::from(ip) & 0xffc0_0000 == 0x6440_0000,
        IpAddr::V6(ip) => ip.segments()[..3] == [0xfd7a, 0x115c, 0xa1e0],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> Config {
        toml::from_str(
            r#"
listen = "0.0.0.0:3000"
[hosts]
desktop = "desktop.example.ts.net:3000"
"#,
        )
        .unwrap()
    }

    #[test]
    fn accepts_tailnet_config() {
        config().validate().unwrap();
        assert!(is_tailnet_ip("100.64.0.1".parse().unwrap()));
        assert!(is_tailnet_ip("100.127.255.254".parse().unwrap()));
        assert!(is_tailnet_ip("fd7a:115c:a1e0::1".parse().unwrap()));
        assert!(!is_tailnet_ip("100.128.0.1".parse().unwrap()));
        assert!(!is_tailnet_ip("100.63.255.254".parse().unwrap()));
        assert!(!is_tailnet_ip("192.168.1.1".parse().unwrap()));
        assert!(!is_tailnet_ip("fd7a:115c:a1e1::1".parse().unwrap()));
    }

    #[test]
    fn defaults_ports_and_preserves_explicit_ports() {
        let c: Config = toml::from_str(
            r#"
listen = "0.0.0.0"
[hosts]
desktop = "desktop.example.ts.net"
ipv4 = "100.100.1.2"
ipv6 = "[fd7a:115c:a1e0::2]"
custom = "other.example.ts.net:1234"
"#,
        )
        .unwrap();
        c.validate().unwrap();
        assert_eq!(c.listen, "0.0.0.0:9722".parse().unwrap());
        assert_eq!(c.hosts["desktop"], "desktop.example.ts.net:9722");
        assert_eq!(c.hosts["ipv4"], "100.100.1.2:9722");
        assert_eq!(c.hosts["ipv6"], "[fd7a:115c:a1e0::2]:9722");
        assert_eq!(c.hosts["custom"], "other.example.ts.net:1234");
        let explicit = config();
        assert_eq!(explicit.listen.port(), 3000);
    }

    #[test]
    fn rejects_invalid_hosts() {
        for address in ["http://desktop:3000", "desktop", "user@desktop:3000"] {
            let mut c = config();
            c.hosts.insert("desktop".into(), address.into());
            assert!(c.validate().is_err(), "{address}");
        }
        let mut c = config();
        c.hosts.insert("../desktop".into(), "desktop:3000".into());
        assert!(c.validate().is_err());
    }
}
