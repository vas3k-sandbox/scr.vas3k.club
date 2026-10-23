mod remote;
mod render;

use std::{
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    sync::Arc,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use axum::{
    Router,
    body::HttpBody,
    extract::{ConnectInfo, Path, Query, Request, State},
    http::{HeaderValue, StatusCode, header},
    middleware::{self, Next},
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
    trust_proxy: bool,
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
        trust_proxy: std::env::var("SCR_TRUST_PROXY").as_deref() == Ok("1"),
    });
    let routes = Router::new()
        .route("/{encoded_url}", get(preview))
        .route("/healthz", get(|| async { "ok\n" }))
        .layer(middleware::from_fn_with_state(app.clone(), log_request))
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

    axum::serve(
        listener,
        routes.into_make_service_with_connect_info::<SocketAddr>(),
    )
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
    let runtime = tokio::runtime::Handle::current();
    let png = tokio::task::spawn_blocking(move || {
        let _slot = slot;
        let document = runtime
            .block_on(tokio::time::timeout(
                Duration::from_secs(20),
                render::prepare(&app, url),
            ))
            .map_err(|_| Error(StatusCode::GATEWAY_TIMEOUT, "Page loading timed out".into()))??;
        render::image(&app, document, size)
    })
    .await
    .map_err(|_| Error(StatusCode::INTERNAL_SERVER_ERROR, "Renderer failed".into()))??;

    Ok(([(header::CONTENT_TYPE, "image/png")], png).into_response())
}

async fn log_request(
    State(app): State<Arc<App>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    request: Request,
    next: Next,
) -> Response {
    if request.uri().path() == "/healthz" {
        return next.run(request).await;
    }

    let start = Instant::now();
    let timestamp_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    let proxy_peer = peer.ip().is_loopback()
        || matches!(peer.ip().to_canonical(), IpAddr::V4(ip) if ip.is_private());
    let forwarded_ip = request
        .headers()
        .get("x-real-ip")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<IpAddr>().ok());
    let ip = if app.trust_proxy && proxy_peer {
        forwarded_ip.unwrap_or(peer.ip())
    } else {
        peer.ip()
    };
    let user_agent = request
        .headers()
        .get(header::USER_AGENT)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .chars()
        .take(1024)
        .collect::<String>();
    let encoded = request.uri().path().trim_start_matches('/');
    let url = (encoded.len() <= 8192)
        .then(|| {
            general_purpose::URL_SAFE_NO_PAD
                .decode(encoded)
                .or_else(|_| general_purpose::URL_SAFE.decode(encoded))
                .ok()
        })
        .flatten()
        .and_then(|bytes| String::from_utf8(bytes).ok());
    let size = Query::<PreviewOptions>::try_from_uri(request.uri())
        .ok()
        .map(|options| options.size.unwrap_or(app.config.sizes[0]))
        .map(|size| format!("{}x{}", size.width, size.height));
    let method = request.method().to_string();
    let response = next.run(request).await;
    let bytes = response.body().size_hint().exact().unwrap_or_default();
    eprintln!(
        "{}",
        serde_json::json!({
            "timestamp_ms": timestamp_ms,
            "event": "request",
            "method": method,
            "url": url,
            "size": size,
            "ip": ip.to_string(),
            "peer_ip": peer.ip().to_string(),
            "user_agent": user_agent,
            "status": response.status().as_u16(),
            "bytes": bytes,
            "duration_ms": (start.elapsed().as_secs_f64() * 10000.0).round() / 10.0,
        })
    );
    response
}
