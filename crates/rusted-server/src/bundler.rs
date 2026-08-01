//! In-process bundling with rolldown, so developing a function needs no node,
//! no npx, and no temp files.
//!
//! Rolldown's Rust crates are explicitly unstable — their docs state the
//! crates "will not follow the semver contract" — so the version is pinned
//! exactly and this module keeps the API surface it touches deliberately
//! small: options in, code and map out.

use std::path::Path;

use rolldown::{
    Bundler, BundlerOptions, BundlerTransformOptions, Either, OutputFormat, Platform, SourceMapType,
};
use rolldown_common::Output;

pub struct Bundled {
    pub code: String,
    /// The source map as JSON, used to map stack frames back to your files.
    pub sourcemap: Option<String>,
}

/// Bundles `entry` into a single ES2020 ES module, entirely in memory.
/// `with_sourcemap` also returns the map — dev uses it to place stack frames;
/// a deploy artifact usually doesn't want one.
pub async fn bundle(entry: &Path, with_sourcemap: bool) -> Result<Bundled, String> {
    let cwd = entry
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .map(|p| p.to_path_buf())
        .unwrap_or(std::env::current_dir().map_err(|e| e.to_string())?);
    let file = entry
        .file_name()
        .ok_or_else(|| format!("{} is not a file", entry.display()))?
        .to_string_lossy()
        .to_string();

    let mut bundler = Bundler::new(BundlerOptions {
        input: Some(vec![format!("./{file}").into()]),
        cwd: Some(cwd),
        format: Some(OutputFormat::Esm),
        // Neutral, not the ESM default of Browser: a dependency reaching for a
        // Node builtin should fail here, not mysteriously at runtime.
        platform: Some(Platform::Neutral),
        sourcemap: with_sourcemap.then_some(SourceMapType::File),
        transform: Some(BundlerTransformOptions {
            target: Some(Either::Left("es2020".to_string())),
            ..Default::default()
        }),
        ..Default::default()
    })
    .map_err(|e| format!("{e}"))?;

    let output = bundler.generate().await.map_err(|e| format!("{e}"))?;
    let _ = bundler.close().await;

    let chunk = output
        .assets
        .iter()
        .find_map(|asset| match asset {
            Output::Chunk(chunk) if chunk.is_entry => Some(chunk),
            _ => None,
        })
        .ok_or_else(|| "the bundler produced no entry chunk".to_string())?;

    Ok(Bundled {
        code: chunk.code.clone(),
        sourcemap: chunk.map.as_ref().map(|map| map.to_json_string()),
    })
}
