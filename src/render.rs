use std::{
    collections::{HashMap, HashSet},
    io::Cursor,
    sync::Arc,
};

use axum::http::StatusCode;
use scraper::{Html, Selector};
use takumi::prelude::{
    FromHtmlOptions, ImageSource, Node, OutputFormat, RenderOptions, StyleSheet, Viewport,
};
use takumi_core::resources::image_buffer::ImageBuffer;
use url::Url;

use crate::{App, Error, Result, Size, remote};

pub struct Document {
    node: Node,
    stylesheet: StyleSheet,
    images: Vec<(String, remote::Resource)>,
}

pub async fn prepare(app: &App, url: Url) -> Result<Document> {
    let page = remote::fetch(app, url, 1024 * 1024).await?;
    if page.mime != "text/html" {
        return Err(Error(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "Page must be text/html".into(),
        ));
    }
    let html = std::str::from_utf8(&page.bytes).map_err(|_| {
        Error(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "HTML must be UTF-8".into(),
        )
    })?;
    let (node, stylesheet, sources) = parse(html)?;
    if sources.len() > 16 {
        return Err(Error(
            StatusCode::PAYLOAD_TOO_LARGE,
            "Page may reference at most 16 images".into(),
        ));
    }

    let mut images = Vec::new();
    let mut remaining = 24 * 1024 * 1024;
    for source in sources {
        let url = page
            .url
            .join(&source)
            .map_err(|_| Error(StatusCode::BAD_REQUEST, "Invalid image URL".into()))?;
        let resource = remote::fetch(app, url, remaining.min(8 * 1024 * 1024)).await?;
        if !matches!(
            resource.mime.as_str(),
            "image/png" | "image/jpeg" | "image/webp" | "image/svg+xml"
        ) {
            return Err(Error(
                StatusCode::UNSUPPORTED_MEDIA_TYPE,
                "Images must be PNG, JPEG, WebP or SVG".into(),
            ));
        }
        remaining -= resource.bytes.len();
        images.push((source, resource));
    }
    Ok(Document {
        node,
        stylesheet,
        images,
    })
}

fn parse(html: &str) -> Result<(Node, StyleSheet, HashSet<String>)> {
    let document = Html::parse_document(html);
    if document.tree.nodes().count() > 4096 {
        return Err(Error(
            StatusCode::PAYLOAD_TOO_LARGE,
            "HTML exceeds 4096 nodes".into(),
        ));
    }

    let style = Selector::parse("style").expect("valid style selector");
    let css: String = document
        .select(&style)
        .map(|element| element.inner_html() + "\n")
        .collect();
    let stylesheet = StyleSheet::parse(&css).map_err(|error| {
        Error(
            StatusCode::UNPROCESSABLE_ENTITY,
            format!("Invalid CSS: {error}"),
        )
    })?;
    let node = takumi::from_html(html, FromHtmlOptions::builder().max_depth(64).build()).map_err(
        |error| {
            Error(
                StatusCode::UNPROCESSABLE_ENTITY,
                format!("Invalid HTML: {error}"),
            )
        },
    )?;
    let image_selector = Selector::parse("img[src]").expect("valid image selector");
    let relative_images = document
        .select(&image_selector)
        .filter_map(|element| element.attr("src"))
        .filter(|source| !source.trim().is_empty());
    let sources = node
        .image_urls()
        .chain(stylesheet.image_urls())
        .chain(relative_images)
        .map(str::to_owned)
        .collect();
    Ok((node, stylesheet, sources))
}

pub fn image(app: &App, document: Document, size: Size) -> Result<Vec<u8>> {
    let mut images = HashMap::new();
    let mut remaining_pixels = 16_000_000_u64;
    for (source, resource) in document.images {
        let bytes = resource.bytes;

        if resource.mime == "image/svg+xml" {
            let image = ImageSource::from_bytes(&bytes).map_err(|_| {
                Error(
                    StatusCode::UNSUPPORTED_MEDIA_TYPE,
                    "Invalid SVG image".into(),
                )
            })?;
            let ImageSource::Svg(svg) = &image else {
                return Err(Error(
                    StatusCode::UNSUPPORTED_MEDIA_TYPE,
                    "Invalid SVG image".into(),
                ));
            };
            let (width, height) = svg.dimensions();

            if width > 8192.0 || height > 8192.0 {
                return Err(Error(
                    StatusCode::PAYLOAD_TOO_LARGE,
                    "SVG exceeds decoding dimensions".into(),
                ));
            }

            let pixels = width.ceil() as u64 * height.ceil() as u64;
            remaining_pixels = remaining_pixels.checked_sub(pixels).ok_or_else(|| {
                Error(
                    StatusCode::PAYLOAD_TOO_LARGE,
                    "Images exceed 16 million pixels combined".into(),
                )
            })?;
            images.insert(Arc::<str>::from(source), image);
            continue;
        }

        let header = image::ImageReader::new(Cursor::new(&bytes))
            .with_guessed_format()
            .map_err(|_| Error(StatusCode::UNSUPPORTED_MEDIA_TYPE, "Invalid image".into()))?;
        let format = header
            .format()
            .filter(|format| {
                matches!(
                    format,
                    image::ImageFormat::Png | image::ImageFormat::Jpeg | image::ImageFormat::WebP
                )
            })
            .ok_or_else(|| {
                Error(
                    StatusCode::UNSUPPORTED_MEDIA_TYPE,
                    "Unsupported image encoding".into(),
                )
            })?;
        let (width, height) = header.into_dimensions().map_err(|_| {
            Error(
                StatusCode::UNSUPPORTED_MEDIA_TYPE,
                "Invalid image dimensions".into(),
            )
        })?;
        let pixels = u64::from(width) * u64::from(height);
        remaining_pixels = remaining_pixels.checked_sub(pixels).ok_or_else(|| {
            Error(
                StatusCode::PAYLOAD_TOO_LARGE,
                "Images exceed 16 million pixels combined".into(),
            )
        })?;

        let mut reader = image::ImageReader::with_format(Cursor::new(bytes), format);
        let mut limits = image::Limits::default();
        limits.max_image_width = Some(8192);
        limits.max_image_height = Some(8192);
        limits.max_alloc = Some(64 * 1024 * 1024);
        reader.limits(limits);
        let decoded = reader.decode().map_err(|_| {
            Error(
                StatusCode::UNSUPPORTED_MEDIA_TYPE,
                "Image is invalid or exceeds decoding limits".into(),
            )
        })?;
        let rgba = decoded.into_rgba8();
        let (width, height) = rgba.dimensions();
        let buffer =
            ImageBuffer::from_rgba_bytes(rgba.into_raw(), width, height).ok_or_else(|| {
                Error(
                    StatusCode::UNSUPPORTED_MEDIA_TYPE,
                    "Invalid image buffer".into(),
                )
            })?;
        images.insert(Arc::<str>::from(source), ImageSource::from(buffer));
    }

    let options = RenderOptions::builder()
        .viewport(Viewport::new((size.width, size.height)))
        .node(document.node)
        .stylesheet(Arc::new(document.stylesheet))
        .images(images)
        .fonts(&app.fonts)
        .build();
    let bitmap = takumi::render(options).map_err(|error| {
        Error(
            StatusCode::UNPROCESSABLE_ENTITY,
            format!("Rendering failed: {error}"),
        )
    })?;
    let mut png = Vec::new();
    takumi::write_image(&bitmap, &mut png, OutputFormat::Png).map_err(|_| {
        Error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "PNG encoding failed".into(),
        )
    })?;
    Ok(png)
}
