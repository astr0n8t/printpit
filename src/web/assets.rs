//! Static assets embedded in the binary, served under content-hashed URLs.
//!
//! URLs look like `/static/{hash}/css/tui.css`. When the hash matches the embedded file the
//! response is cached forever; a stale hash still gets the current file, just uncached.

use std::collections::HashMap;
use std::sync::LazyLock;

use axum::body::Body;
use axum::extract::Path;
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};

#[derive(rust_embed::Embed)]
#[folder = "static/"]
struct Static;

const HASH_LEN: usize = 16;

static HASHES: LazyLock<HashMap<String, String>> = LazyLock::new(|| {
    Static::iter()
        .filter_map(|path| {
            let file = Static::get(&path)?;
            let hash = hex::encode(file.metadata.sha256_hash());
            Some((path.into_owned(), hash[..HASH_LEN].to_string()))
        })
        .collect()
});

/// Returns the versioned URL for an embedded asset. Panics in debug builds on unknown paths so
/// typos in templates surface immediately.
pub fn url(path: &str) -> String {
    match HASHES.get(path) {
        Some(hash) => format!("/static/{hash}/{path}"),
        None => {
            debug_assert!(false, "unknown static asset: {path}");
            format!("/static/0/{path}")
        }
    }
}

pub async fn serve(Path((hash, path)): Path<(String, String)>) -> Response {
    let Some(file) = Static::get(&path) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let current = hex::encode(file.metadata.sha256_hash());
    let cache = if current.starts_with(&hash) && hash.len() == HASH_LEN {
        "public, max-age=31536000, immutable"
    } else {
        "no-cache"
    };
    let mime = mime_guess::from_path(&path).first_or_octet_stream();
    let mut res = Response::new(Body::from(file.data.into_owned()));
    let headers = res.headers_mut();
    if let Ok(v) = HeaderValue::from_str(mime.as_ref()) {
        headers.insert(header::CONTENT_TYPE, v);
    }
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static(cache));
    res
}

/// The web app manifest, at a fixed URL (`/static/manifest.webmanifest`) so browsers can find it
/// across releases, with versioned icon URLs. Public like the rest of `/static/`: browsers fetch
/// manifests without cookies. It holds no printer data.
pub async fn manifest() -> Response {
    let icon = |path: &str, size: u32, purpose: &str| {
        serde_json::json!({
            "src": url(path),
            "sizes": format!("{size}x{size}"),
            "type": "image/png",
            "purpose": purpose,
        })
    };
    let body = serde_json::json!({
        "id": "/",
        "name": "printpit",
        "short_name": "printpit",
        "description": "Control your Klipper printers.",
        "start_url": "/",
        "scope": "/",
        "display": "standalone",
        "background_color": "#0b0d0b",
        "theme_color": "#0b0d0b",
        "icons": [
            icon("icons/icon-192.png", 192, "any"),
            icon("icons/icon-512.png", 512, "any"),
            icon("icons/maskable-512.png", 512, "maskable"),
        ],
    });
    (
        [
            (header::CONTENT_TYPE, "application/manifest+json"),
            (header::CACHE_CONTROL, "no-cache"),
        ],
        body.to_string(),
    )
        .into_response()
}
