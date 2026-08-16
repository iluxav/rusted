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
use rolldown_common::{ModuleType, Output};

/// A file with `import` statements can't run as-is — the runtime resolves no
/// modules — so it gets bundled first.
pub fn needs_bundling(source: &str) -> bool {
    source.lines().any(|line| {
        let line = line.trim_start();
        (line.starts_with("import ") || line.starts_with("import{") || line.starts_with("import("))
            && !line.starts_with("import.meta")
    }) || source.contains("} from \"")
        || source.contains("} from '")
}

pub struct Bundled {
    pub code: String,
    /// The source map as JSON, used to map stack frames back to your files.
    pub sourcemap: Option<String>,
}

/// The source to deploy for `entry`: bundled when it has imports, read as-is
/// otherwise. Deploying goes through the same pipeline as developing, so what
/// runs in `rusted run` is what lands on the server.
pub async fn source_for(entry: &Path) -> Result<String, String> {
    let source = std::fs::read_to_string(entry)
        .map_err(|e| format!("cannot read {}: {e}", entry.display()))?;
    if !should_bundle(entry, &source) {
        return Ok(source);
    }
    Ok(bundle(entry, false).await?.code)
}

/// The one place that decides whether a file needs the bundler. Deploying and
/// `rusted run` both ask here: when they each decided for themselves, they
/// disagreed about TypeScript and only the deploy path transpiled it.
pub fn should_bundle(entry: &Path, source: &str) -> bool {
    is_typescript(entry) || needs_bundling(source)
}

/// TypeScript always goes through the transpiler, imports or not: the engine
/// runs JavaScript, and a lone `interface` is a syntax error to it. Keying this
/// off the extension rather than the content is deliberate — deciding by
/// looking for `interface` or a `:` would misread ordinary JavaScript.
fn is_typescript(entry: &Path) -> bool {
    entry
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| matches!(e, "ts" | "tsx" | "mts" | "cts"))
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
        // Template files import as strings — the htmx project shape. Keyed by
        // extension without the dot.
        module_types: Some(rustc_hash::FxHashMap::from_iter([(
            "html".to_string(),
            ModuleType::Text,
        )])),
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A .ts file with no imports still has to reach the transpiler: the engine
    /// speaks JavaScript, and `interface` is a syntax error to it.
    #[tokio::test]
    async fn typescript_without_imports_is_still_transpiled() {
        let dir = tempfile::tempdir().unwrap();
        let entry = dir.path().join("solo.ts");
        std::fs::write(
            &entry,
            "interface Input { name: string }\n\
             export default async function handler(request: Request, context: any): Promise<Response> {\n\
             \x20 const input = await request.json() as Input;\n\
             \x20 return context.json({ message: `Hello, ${input.name}` });\n\
             }\n",
        )
        .unwrap();

        let source = source_for(&entry).await.expect("should transpile");
        assert!(
            !source.contains("interface "),
            "type declarations survived into the deployed source:\n{source}"
        );
        assert!(
            !source.contains(": Input"),
            "type annotations survived into the deployed source:\n{source}"
        );
        assert!(
            source.contains("handler"),
            "the handler was lost:\n{source}"
        );
    }

    /// Plain JavaScript with no imports must not pay for a bundle it does not
    /// need — that path is the fast one and should stay byte-for-byte.
    #[tokio::test]
    async fn plain_javascript_is_passed_through_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let entry = dir.path().join("plain.js");
        let original = "export default async function handler(request, context) {\n  return context.json({ ok: true });\n}\n";
        std::fs::write(&entry, original).unwrap();

        assert_eq!(source_for(&entry).await.unwrap(), original);
    }
}
