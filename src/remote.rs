use std::{net::IpAddr, sync::LazyLock, time::Duration};

use axum::http::StatusCode;
use ipnet::IpNet;
use reqwest::{Client, redirect::Policy};
use url::Url;

use crate::{App, Error, Result};

static BLOCKED_NETWORKS: LazyLock<Vec<IpNet>> = LazyLock::new(|| {
    [
        "0.0.0.0/8",
        "10.0.0.0/8",
        "100.64.0.0/10",
        "127.0.0.0/8",
        "169.254.0.0/16",
        "172.16.0.0/12",
        "192.0.0.0/24",
        "192.0.2.0/24",
        "192.168.0.0/16",
        "198.18.0.0/15",
        "198.51.100.0/24",
        "203.0.113.0/24",
        "224.0.0.0/3",
        "2001::/23",
        "2001:db8::/32",
        "2002::/16",
        "3fff::/20",
    ]
    .into_iter()
    .map(|network| network.parse().expect("valid network constant"))
    .collect()
});

pub struct Resource {
    pub url: Url,
    pub mime: String,
    pub bytes: Vec<u8>,
}

pub fn validate(app: &App, url: &Url) -> Result<()> {
    let host = url.host_str().unwrap_or_default();
    let approved = app.config.domains.iter().any(|domain| {
        host == domain
            || host
                .strip_suffix(domain)
                .is_some_and(|prefix| prefix.ends_with('.'))
    });

    if !matches!(url.scheme(), "http" | "https")
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return Err(Error(
            StatusCode::BAD_REQUEST,
            "Only HTTP(S) URLs without credentials are supported".into(),
        ));
    }
    if !approved {
        return Err(Error(
            StatusCode::FORBIDDEN,
            "URL domain is not approved".into(),
        ));
    }
    Ok(())
}

pub async fn fetch(app: &App, mut url: Url, limit: usize) -> Result<Resource> {
    for _ in 0..=4 {
        validate(app, &url)?;
        let host = url.host_str().unwrap_or_default();
        let port = url.port_or_known_default().unwrap_or(443);
        let addresses: Vec<_> = tokio::net::lookup_host((host, port))
            .await
            .map_err(|_| Error(StatusCode::BAD_GATEWAY, "DNS lookup failed".into()))?
            .collect();

        if addresses.is_empty() {
            return Err(Error(
                StatusCode::BAD_GATEWAY,
                "Host has no addresses".into(),
            ));
        }
        for address in &addresses {
            let ip = address.ip().to_canonical();
            let loopback =
                app.allow_loopback && matches!(host, "localhost" | "127.0.0.1") && ip.is_loopback();
            let public = match ip {
                IpAddr::V4(_) => !BLOCKED_NETWORKS.iter().any(|network| network.contains(&ip)),
                IpAddr::V6(value) => {
                    value.segments()[0] & 0xe000 == 0x2000
                        && !BLOCKED_NETWORKS.iter().any(|network| network.contains(&ip))
                }
            };
            if !public && !loopback {
                return Err(Error(
                    StatusCode::FORBIDDEN,
                    "Private and reserved addresses are forbidden".into(),
                ));
            }
        }

        let client = Client::builder()
            .no_proxy()
            .redirect(Policy::none())
            .connect_timeout(Duration::from_secs(4))
            .timeout(Duration::from_secs(10))
            .resolve_to_addrs(host, &addresses)
            .user_agent("scr.vas3k.club/0.1")
            .build()
            .map_err(|_| {
                Error(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "HTTP client initialization failed".into(),
                )
            })?;
        let mut response = client
            .get(url.clone())
            .send()
            .await
            .map_err(|_| Error(StatusCode::BAD_GATEWAY, "Upstream request failed".into()))?;

        if response.status().is_redirection() {
            let location = response
                .headers()
                .get("location")
                .and_then(|value| value.to_str().ok())
                .ok_or_else(|| {
                    Error(
                        StatusCode::BAD_GATEWAY,
                        "Redirect has no valid location".into(),
                    )
                })?;
            url = url
                .join(location)
                .map_err(|_| Error(StatusCode::BAD_GATEWAY, "Invalid redirect URL".into()))?;
            continue;
        }
        if !response.status().is_success() {
            return Err(Error(
                StatusCode::BAD_GATEWAY,
                format!("Upstream returned {}", response.status()),
            ));
        }
        if response
            .content_length()
            .is_some_and(|size| size > limit as u64)
        {
            return Err(Error(
                StatusCode::PAYLOAD_TOO_LARGE,
                "Upstream resource is too large".into(),
            ));
        }

        let mime = response
            .headers()
            .get("content-type")
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
            .split(';')
            .next()
            .unwrap_or_default()
            .trim()
            .to_ascii_lowercase();
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(|_| {
            Error(
                StatusCode::BAD_GATEWAY,
                "Failed reading upstream resource".into(),
            )
        })? {
            if chunk.len() > limit.saturating_sub(bytes.len()) {
                return Err(Error(
                    StatusCode::PAYLOAD_TOO_LARGE,
                    "Upstream resource is too large".into(),
                ));
            }
            bytes.extend_from_slice(&chunk);
        }
        return Ok(Resource { url, mime, bytes });
    }
    Err(Error(StatusCode::BAD_GATEWAY, "Too many redirects".into()))
}
