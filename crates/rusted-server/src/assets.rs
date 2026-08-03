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
}

const ASSETS: &[Asset] = &[
    Asset {
        name: "rusted-logo.png",
        bytes: include_bytes!("../assets/rusted-logo.png"),
        content_type: "image/png",
    },
    Asset {
        name: "rusted-logo2.png",
        bytes: include_bytes!("../assets/rusted-logo2.png"),
        content_type: "image/png",
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

    (
        [
            (header::CONTENT_TYPE, asset.content_type),
            // The URL is stable across releases, so the image may change under
            // it. An hour of blind caching plus the ETag means repeat visits
            // cost a 304, and a new logo appears within the hour rather than
            // whenever the browser feels like asking.
            (header::CACHE_CONTROL, "public, max-age=3600"),
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
    fn logos_are_actually_pngs() {
        for asset in ASSETS {
            assert_eq!(
                &asset.bytes[..8],
                b"\x89PNG\r\n\x1a\n",
                "{} is not a PNG, but is served as one",
                asset.name
            );
        }
    }
}
