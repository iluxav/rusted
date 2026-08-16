//! Static files, compiled into the binary.
//!
//! Embedded rather than read from disk because the deployment is one file: the
//! release ships a single binary, upgrading is an `install` of it, and systemd
//! runs it under `ProtectSystem=strict` with no asset directory to point at.
//! Reading from the filesystem would add a second thing to ship and a way for
//! the two to disagree.
//!
//! The alternative these replace is a base64 `data:` URI in the template, which
//! costs a third more bytes than the file, re-sends the image on every page
//! load because it cannot be cached separately, and blocks the HTML from
//! streaming until the whole payload has arrived.

use axum::{
    extract::Path,
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::get,
    Router,
};
use sha2::{Digest, Sha256};
use std::sync::OnceLock;

struct Asset {
    name: &'static str,
    bytes: &'static [u8],
    content_type: &'static str,
    /// Immutable assets carry a year-long cache lifetime; anything that could
    /// change under its URL gets an hour plus ETag revalidation. Fonts and the
    /// pinned htmx build only ever change by getting a new filename.
    immutable: bool,
}

const ASSETS: &[Asset] = &[
    Asset {
        name: "rusted-logo.png",
        bytes: include_bytes!("../assets/rusted-logo.png"),
        content_type: "image/png",
        immutable: false,
    },
    Asset {
        name: "rusted-logo2.png",
        bytes: include_bytes!("../assets/rusted-logo2.png"),
        content_type: "image/png",
        immutable: false,
    },
    Asset {
        name: "rusted-metal.jpg",
        bytes: include_bytes!("../assets/rusted-metal.jpg"),
        content_type: "image/jpeg",
        immutable: true,
    },
    Asset {
        name: "bricolage-grotesque-latin.woff2",
        bytes: include_bytes!("../assets/bricolage-grotesque-latin.woff2"),
        content_type: "font/woff2",
        immutable: true,
    },
    Asset {
        name: "jetbrains-mono-latin.woff2",
        bytes: include_bytes!("../assets/jetbrains-mono-latin.woff2"),
        content_type: "font/woff2",
        immutable: true,
    },
    Asset {
        name: "htmx.min.js",
        bytes: include_bytes!("../assets/htmx.min.js"),
        content_type: "text/javascript; charset=utf-8",
        immutable: true,
    },
    Asset {
        // CodeMirror for the console editor, built by `make editor-js` from
        // crates/rusted-server/editor/. Mutable: the name stays put across
        // rebuilds, so the ETag does the versioning.
        name: "editor.js",
        bytes: include_bytes!("../assets/editor.js"),
        content_type: "text/javascript; charset=utf-8",
        immutable: false,
    },
];

/// Content hashes, in the same order as `ASSETS`. Hashing at first use rather
/// than per request; the bytes never change for a given binary.
fn etags() -> &'static [String] {
    static ETAGS: OnceLock<Vec<String>> = OnceLock::new();
    ETAGS.get_or_init(|| {
        ASSETS
            .iter()
            .map(|asset| format!("\"{:x}\"", Sha256::digest(asset.bytes)))
            .collect()
    })
}

fn lookup(name: &str) -> Option<(&'static Asset, &'static str)> {
    // A linear scan over a handful of names. The path never reaches the
    // filesystem, so `..` and absolute paths are names that simply don't match.
    let index = ASSETS.iter().position(|asset| asset.name == name)?;
    Some((&ASSETS[index], etags()[index].as_str()))
}

async fn serve(Path(name): Path<String>, headers: HeaderMap) -> Response {
    let Some((asset, etag)) = lookup(&name) else {
        return StatusCode::NOT_FOUND.into_response();
    };

    let known = headers
        .get(header::IF_NONE_MATCH)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.split(',').any(|candidate| candidate.trim() == etag));
    if known {
        return (StatusCode::NOT_MODIFIED, [(header::ETAG, etag)]).into_response();
    }

    // Mutable URLs get an hour of blind caching plus the ETag, so repeat
    // visits cost a 304 and a changed file appears within the hour rather
    // than whenever the browser feels like asking.
    let cache = if asset.immutable {
        "public, max-age=31536000, immutable"
    } else {
        "public, max-age=3600"
    };
    (
        [
            (header::CONTENT_TYPE, asset.content_type),
            (header::CACHE_CONTROL, cache),
            (header::ETAG, etag),
        ],
        asset.bytes,
    )
        .into_response()
}

/// Generic over the caller's state so this can be merged into a router that
/// has not had `with_state` applied yet; these handlers need no state.
pub fn router<S>() -> Router<S>
where
    S: Clone + Send + Sync + 'static,
{
    Router::new().route("/assets/{name}", get(serve))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_asset_is_reachable_and_non_empty() {
        for asset in ASSETS {
            let (found, etag) = lookup(asset.name).expect("asset resolves by name");
            assert!(!found.bytes.is_empty(), "{} is empty", asset.name);
            assert!(etag.starts_with('"') && etag.ends_with('"'));
        }
    }

    #[test]
    fn unknown_names_and_traversal_resolve_to_nothing() {
        assert!(lookup("nope.png").is_none());
        assert!(lookup("../src/main.rs").is_none());
        assert!(lookup("/etc/passwd").is_none());
    }

    #[test]
    fn etags_differ_between_assets() {
        // Same ETag on different bytes would serve one image in place of the
        // other once a client had cached either.
        let first = lookup(ASSETS[0].name).unwrap().1;
        let second = lookup(ASSETS[1].name).unwrap().1;
        assert_ne!(first, second);
    }

    #[test]
    fn bytes_match_their_content_type() {
        for asset in ASSETS {
            let magic: &[u8] = match asset.content_type {
                "image/png" => b"\x89PNG\r\n\x1a\n",
                "font/woff2" => b"wOF2",
                _ => continue,
            };
            assert_eq!(
                &asset.bytes[..magic.len()],
                magic,
                "{} does not match its content type {}",
                asset.name,
                asset.content_type
            );
        }
    }
}
