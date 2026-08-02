//! Where `rusted login` keeps the key it was granted.
//!
//! A file rather than the OS keychain: the CLI runs in containers and CI where
//! no secret service exists, and a keychain that fails there would be worse
//! than one that isn't offered. `RUSTED_API_KEY` still wins, so CI is
//! unaffected by anything stored here.

use std::collections::BTreeMap;
use std::path::PathBuf;

/// Keyed by admin URL, so a local server and a hosted one can coexist.
type Store = BTreeMap<String, String>;

fn path() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir().map(|home| home.join(".config")))?;
    Some(base.join("rusted").join("credentials.json"))
}

fn read() -> Store {
    path()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

pub fn get(admin: &str) -> Option<String> {
    read().get(admin.trim_end_matches('/')).cloned()
}

pub fn save(admin: &str, key: &str) -> Result<PathBuf, String> {
    let path = path().ok_or("cannot determine a config directory")?;
    let mut store = read();
    store.insert(admin.trim_end_matches('/').to_string(), key.to_string());
    let parent = path.parent().ok_or("bad config path")?;
    std::fs::create_dir_all(parent).map_err(|e| format!("cannot create {parent:?}: {e}"))?;
    let body = serde_json::to_string_pretty(&store).map_err(|e| e.to_string())?;
    std::fs::write(&path, body).map_err(|e| format!("cannot write {path:?}: {e}"))?;
    restrict(&path);
    Ok(path)
}

pub fn forget(admin: &str) -> Result<bool, String> {
    let Some(path) = path() else {
        return Ok(false);
    };
    let mut store = read();
    if store.remove(admin.trim_end_matches('/')).is_none() {
        return Ok(false);
    }
    let body = serde_json::to_string_pretty(&store).map_err(|e| e.to_string())?;
    std::fs::write(&path, body).map_err(|e| format!("cannot write {path:?}: {e}"))?;
    restrict(&path);
    Ok(true)
}

/// Owner-only. A credential file the rest of the machine can read is not a
/// credential file.
#[cfg(unix)]
fn restrict(path: &std::path::Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
}

#[cfg(not(unix))]
fn restrict(_path: &std::path::Path) {}
