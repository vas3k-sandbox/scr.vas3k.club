mod remote;
mod render;

use std::{
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    sync::Arc,
    time::{Duration, Instant},
};

use axum::{
    Router,
    extract::{Path, Query, State},
    http::{HeaderValue, StatusCode, header},
    middleware,
    response::{IntoResponse, Response},
    routing::get,
};
use base64::{Engine, engine::general_purpose};
use serde::Deserialize;
use takumi::prelude::{FontResource, Fonts};
use tokio::sync::Semaphore;
use url::Url;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {
    domains: Vec<String>,
    sizes: Vec<Size>,
}

#[derive(Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(try_from = "String")]
struct Size {
    width: u32,
    height: u32,
}

impl TryFrom<String> for Size {
    type Error = String;

    fn try_from(value: String) -> std::result::Result<Self, Self::Error> {
        let (width, height) = value.split_once('x').ok_or("Size must use WIDTHxHEIGHT")?;
        let width: u32 = width.parse().map_err(|_| "Invalid image width")?;
        let height: u32 = height.parse().map_err(|_| "Invalid image height")?;
        let pixels = u64::from(width) * u64::from(height);

        if pixels == 0 || pixels > 4_000_000 {
            return Err("Image size must contain between 1 and 4,000,000 pixels".into());
        }
        Ok(Self { width, height })
    }
}

struct App {
    config: Config,
    fonts: Fonts,
    slots: Arc<Semaphore>,
    allow_loopback: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PreviewOptions {
    size: Option<Size>,
}

struct Error(StatusCode, String);

type Result<T> = std::result::Result<T, Error>;

impl IntoResponse for Error {
    fn into_response(self) -> Response {
        (self.0, self.1).into_response()
    }
}

#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() -> std::result::Result<(), Box<dyn std::error::Error>> {
    let address: SocketAddr = std::env::var("SCR_LISTEN")
        .unwrap_or_else(|_| "127.0.0.1:8202".into())
        .parse()?;

    if std::env::args().nth(1).as_deref() == Some("--healthcheck") {
        let mut endpoint = address;

        if endpoint.ip().is_unspecified() {
            endpoint.set_ip(if endpoint.is_ipv4() {
                IpAddr::V4(Ipv4Addr::LOCALHOST)
            } else {
                IpAddr::V6(Ipv6Addr::LOCALHOST)
            });
        }

        reqwest::Client::builder()
            .no_proxy()
            .timeout(Duration::from_secs(3))
            .build()?
            .get(format!("http://{endpoint}/healthz"))
            .send()
            .await?
            .error_for_status()?;
        return Ok(());
    }

    let config_path = std::env::var("SCR_CONFIG").unwrap_or_else(|_| "config.yml".into());
    let config: Config = serde_saphyr::from_str(&std::fs::read_to_string(&config_path)?)?;

    if config.domains.is_empty() || config.sizes.is_empty() {
        return Err("config requires approved domains and at least one image size".into());
    }

    for domain in &config.domains {
        let parsed = Url::parse(&format!("https://{domain}"))?;
        if parsed.host_str() != Some(domain.as_str())
            || parsed.as_str() != format!("https://{domain}/")
        {
            return Err(format!("invalid domain in config: {domain}").into());
        }
    }

    let mut font_paths = std::fs::read_dir("fonts")?
        .map(|entry| entry.map(|entry| entry.path()))
        .collect::<std::io::Result<Vec<_>>>()?;
    font_paths.retain(|path| {
        path.is_file()
            && path
                .extension()
                .and_then(|extension| extension.to_str())
                .is_some_and(|extension| {
                    matches!(
                        extension.to_ascii_lowercase().as_str(),
                        "ttf" | "otf" | "ttc" | "otc" | "woff" | "woff2"
                    )
                })
    });
    font_paths.sort();

    if font_paths.is_empty() {
        return Err("fonts/ must contain at least one supported font".into());
    }

    let mut fonts = Fonts::default();
    for path in font_paths {
        let bytes = std::fs::read(path)?;
        fonts.register(FontResource::new(bytes.as_slice()))?;
    }

    let app = Arc::new(App {
        config,
        fonts,
        slots: Arc::new(Semaphore::new(1)),
        allow_loopback: std::env::var("SCR_ALLOW_LOOPBACK").as_deref() == Ok("1"),
    });
    let routes = Router::new()
        .route("/{encoded_url}", get(preview))
        .route("/healthz", get(|| async { "ok\n" }))
        .with_state(app)
        .layer(middleware::map_response(
            |mut response: Response| async move {
                let cache_control = if response.status() == StatusCode::OK
                    && response.headers().get(header::CONTENT_TYPE)
                        == Some(&HeaderValue::from_static("image/png"))
                {
                    "public, max-age=31536000, immutable"
                } else {
                    "no-store"
                };
                let headers = response.headers_mut();
                headers.insert(
                    header::CACHE_CONTROL,
                    HeaderValue::from_static(cache_control),
                );
                headers.insert(
                    "x-content-type-options",
                    HeaderValue::from_static("nosniff"),
                );
                response
            },
        ));
    let listener = tokio::net::TcpListener::bind(address).await?;
    eprintln!("Listening on {address}");

    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;

    axum::serve(listener, routes)
        .with_graceful_shutdown(async move {
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {},
                _ = terminate.recv() => {},
            }
        })
        .await?;
    Ok(())
}

async fn preview(
    State(app): State<Arc<App>>,
    Path(encoded_url): Path<String>,
    Query(options): Query<PreviewOptions>,
) -> Result<Response> {
    if encoded_url.len() > 8192 {
        return Err(Error(StatusCode::BAD_REQUEST, "URL is too long".into()));
    }

    let decoded = general_purpose::URL_SAFE_NO_PAD
        .decode(&encoded_url)
        .or_else(|_| general_purpose::URL_SAFE.decode(&encoded_url))
        .map_err(|_| {
            Error(
                StatusCode::BAD_REQUEST,
                "Invalid URL-safe base64 URL".into(),
            )
        })?;
    let text = std::str::from_utf8(&decoded)
        .map_err(|_| Error(StatusCode::BAD_REQUEST, "URL must be UTF-8".into()))?;
    let url = Url::parse(text).map_err(|_| Error(StatusCode::BAD_REQUEST, "Invalid URL".into()))?;
    remote::validate(&app, &url)?;

    let size = options.size.unwrap_or(app.config.sizes[0]);

    if !app.config.sizes.contains(&size) {
        return Err(Error(
            StatusCode::BAD_REQUEST,
            "Image size is not configured".into(),
        ));
    }

    let slot = app.slots.clone().try_acquire_owned().map_err(|_| {
        Error(
            StatusCode::SERVICE_UNAVAILABLE,
            "Renderer is busy; retry shortly".into(),
        )
    })?;
    let start = Instant::now();
    let document = tokio::time::timeout(
        std::time::Duration::from_secs(20),
        render::prepare(&app, url),
    )
    .await
    .map_err(|_| Error(StatusCode::GATEWAY_TIMEOUT, "Page loading timed out".into()))??;
    let png = tokio::task::spawn_blocking(move || {
        let _slot = slot;
        render::image(&app, document, size)
    })
    .await
    .map_err(|_| Error(StatusCode::INTERNAL_SERVER_ERROR, "Renderer failed".into()))??;

    eprintln!(
        "Rendered {} bytes in {:.1} ms",
        png.len(),
        start.elapsed().as_secs_f64() * 1000.0
    );
    Ok(([(header::CONTENT_TYPE, "image/png")], png).into_response())
}
