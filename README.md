# scr.vas3k.club

A small Rust + Takumi service that fetches an approved HTML page and returns a PNG. No Chromium, JavaScript, screenshot cache, document cache or remote asset cache. All supported fonts in `fonts/` are loaded once at startup.

## Run

Use Rust 1.99 or later (`brew install rust` on macOS):

```sh
cargo run --release --locked
```

Or run `docker compose up --build -d`. The service listens on `127.0.0.1:8202`. `GET /healthz` returns `ok`; `scr --healthcheck` checks it without loading config or fonts.

## Render

```sh
URL=$(python3 -c 'import base64; print(base64.urlsafe_b64encode(b"https://vas3k.club/post/26085/og/").decode().rstrip("="))')
curl --fail "http://127.0.0.1:8202/$URL" --output /tmp/preview.png
```

The URL must serve an actual UTF-8 HTML preview page. A post containing an `og:image` meta tag is not itself a preview template. Encode it with URL-safe base64, preferably without padding. The encoded URL is the path; `size` is the only supported query parameter.

`config.yml` defines approved domains and allowed output sizes. The first size is the default:

```yaml
domains: [vas3k.club, vas3k.ru, vas3k.blog, vas3k.com, basa.club]

sizes:
  - 1200x630
  - 900x1000
```

Choose another configured size with `/<base64-url>?size=900x1000`. Malformed or unconfigured sizes return 400. Each size must contain between 1 and 4,000,000 pixels.

Domains include their subdomains, with a dot boundary. Approval is checked for the page, each redirect, and every image. DNS addresses are checked and pinned before connection; private and reserved networks are blocked. TLS certificates are verified. Environment proxies are ignored. Only successful 2xx upstream responses are rendered; upstream error responses and failed image downloads return an error without an image.

## Templates

The input page is hosted by your website; this service only fetches and renders it. Put CSS inside `<style>`, use absolute or document-relative image URLs, and make `html`, `body` and the root layout fill 100% width and height. PNG, JPEG and WebP images are decoded as a static frame and validated before rendering. SVG images use Takumi’s native decoder, which disables external file and URL references. CSS image URLs are supported and use the document URL as their base.

Takumi supports a subset of browser CSS. CSS inside `<style>` is used; linked stylesheets are ignored. Remote font loading, scripts and browser APIs are outside this MVP. Add fonts to the `fonts/` directory before starting the service: TTF, OTF, TTC, OTC, WOFF and WOFF2 files are discovered automatically in filename order. Other files, including font licenses, are ignored. Startup fails if no fonts are found or a font cannot be loaded. Takumi parses CSS; this service has no custom CSS parser.

The service admits one request at a time, including fetching and rendering; concurrent requests receive 503. Pages are limited to 1 MiB and 64 nested elements. At most 16 unique images, 8 MiB per image, 24 MiB combined downloads and 16 million combined image pixels are allowed. The output is limited to 4 million pixels. Loading has a 20-second deadline. These limits bound normal MVP workloads; this is intended for templates you control.

## Production

Place the checkout in `/home/vas3k/src.vas3k.club` and run:

```sh
cd ~/src.vas3k.club
docker compose up --build -d
```

Compose publishes `127.0.0.1:8202` and mounts config and fonts relative to this checkout. The club repository's `etc/nginx/scr.vas3k.club.conf` serves `scr.vas3k.club`, redirects HTTP to HTTPS, uses `/home/vas3k/certs/pubkey.pem` and `/home/vas3k/certs/privkey.pem`, and proxies the full path and query string to this port.

Successful PNG responses have `Cache-Control: public, max-age=31536000, immutable` (one year). Errors and health checks have `Cache-Control: no-store`. The renderer keeps no local image cache; nginx passes the response headers through without caching.

Cloudflare requires a Cache Rule because these URLs have no image extension: match hostname `scr.vas3k.club`, exclude `/healthz`, select **Eligible for cache**, respect origin cache-control headers, and retain the full path and query string in the cache key, including `size`. Do not override origin TTLs for errors. See [Cloudflare's default cache behavior](https://developers.cloudflare.com/cache/concepts/default-cache-behavior/).

A cached URL represents one preview version. To refresh an edited post or layout, encode a versioned source URL such as `https://vas3k.club/post/26085/og/?v=2`, or purge the existing URL from Cloudflare. The encoded source query changes the image path; `size` remains the only query parameter accepted by this service.

The container runs without root privileges, has a read-only filesystem, a 256 MiB memory limit and bounded logs. Config and fonts are mounted read-only. Health checks query `/healthz`; SIGTERM and Ctrl-C drain active requests, with a 30-second container shutdown grace period.

Keep `SCR_ALLOW_LOOPBACK` unset in production. Explicitly approve every page and asset domain in `config.yml`. The default list approves `vas3k.club`, `vas3k.ru`, `vas3k.blog`, `vas3k.com` and `basa.club`, including all their subdomains. Restart the service after changing config or fonts. Check readiness and logs with `docker compose ps` and `docker compose logs --tail=100 scr`.

## Render local club posts

Run Django from `~/Dev/vas3k.club` on port 8000. Its preview pages are `/<post_type>/<post_slug>/og/`, including questions and other post types. Copy `config.yml` to the ignored `config.local.yml` and set its first line to:

```yaml
domains: [vas3k.club, vas3k.ru, vas3k.blog, vas3k.com, basa.club, localhost, 127.0.0.1]
```

Start this service from the project directory with `SCR_CONFIG=config.local.yml SCR_ALLOW_LOOPBACK=1 cargo run --release --locked`. Local sources require both explicit domain approval and `SCR_ALLOW_LOOPBACK=1`. In another terminal:

```sh
for page in post/26085 question/10641 post/28086 question/25342; do
  encoded_url=$(python3 -c 'import base64, sys; print(base64.urlsafe_b64encode(sys.argv[1].encode()).decode().rstrip("="))' "http://127.0.0.1:8000/$page/og/")
  curl --fail "http://127.0.0.1:8202/$encoded_url" --output "/tmp/og-${page##*/}.png"
done
```

Environment variables: `SCR_CONFIG` selects the YAML file; `SCR_LISTEN` changes the bind address; `SCR_ALLOW_LOOPBACK=1` permits approved `localhost` or `127.0.0.1` sources for local development only.

The bundled Ubuntu font is distributed under the Ubuntu Font Licence; Noto Color Emoji is distributed under the SIL Open Font License. See `fonts/LICENSE`.
