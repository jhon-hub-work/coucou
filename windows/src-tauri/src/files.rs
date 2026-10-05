// Dropped files are copied into %LOCALAPPDATA%\Boo\inbox so the original is
// never touched and the copy survives the drag source going away.
// The inbox is swept of anything older than a week, as on macOS.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use serde::Serialize;

use crate::settings;

const KEEP_FOR: Duration = Duration::from_secs(7 * 24 * 60 * 60);

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct DroppedFile {
    pub name: String,
    pub path: String,
    pub size: u64,
}

pub fn inbox_dir() -> PathBuf {
    settings::local_dir().join("inbox")
}

/// A dropped file the page read itself. Windows hands a file dragged onto the
/// island to WebView2, not to Tauri (2026-10-05: the OLE target Tauri registers is
/// never reached on Jhon's PC), so the page sends the bytes and they land here.
pub fn ingest_bytes(name: &str, bytes: &[u8]) -> Result<DroppedFile, String> {
    // Only the file's own name, never a path someone slipped into it.
    let name = Path::new(name)
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| "file".into());
    let dest = free_spot(&name)?;
    std::fs::write(&dest, bytes).map_err(|e| format!("cannot save: {e}"))?;
    sweep(&inbox_dir());
    Ok(DroppedFile { name, path: dest.to_string_lossy().to_string(), size: bytes.len() as u64 })
}

/// Where `name` goes in the inbox: itself, or "name (2).ext" and so on if taken.
fn free_spot(name: &str) -> Result<PathBuf, String> {
    let dir = inbox_dir();
    crate::platform::ensure_private_dir(&settings::local_dir()).map_err(|e| e.to_string())?;
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let mut dest = dir.join(name);
    if dest.exists() {
        let p = Path::new(name);
        let stem = p.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
        let ext = p.extension().map(|s| format!(".{}", s.to_string_lossy())).unwrap_or_default();
        for i in 2..1000 {
            let candidate = dir.join(format!("{stem} ({i}){ext}"));
            if !candidate.exists() {
                dest = candidate;
                break;
            }
        }
    }
    Ok(dest)
}

/// Drops anything copied here more than a week ago. `ingest` stamps every copy
/// with the time it landed, so this really is the age of the copy and not the
/// age of whatever the user happened to drag in.
fn sweep(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    let now = SystemTime::now();
    for entry in entries.flatten() {
        let Ok(meta) = entry.metadata() else { continue };
        let Ok(copied) = meta.modified() else { continue };
        if now.duration_since(copied).map(|age| age > KEEP_FOR).unwrap_or(false) {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dropped_bytes_land_in_the_inbox_and_never_overwrite() {
        let name = format!("boo-test-{}.txt", std::process::id());
        let first = ingest_bytes(&name, b"hello").unwrap();
        assert_eq!(first.name, name);
        assert_eq!(std::fs::read(&first.path).unwrap(), b"hello");
        // A second drop of the same name must not clobber the first copy.
        let second = ingest_bytes(&name, b"second").unwrap();
        assert_ne!(first.path, second.path);
        assert_eq!(std::fs::read(&first.path).unwrap(), b"hello");
        // A name carrying a path keeps only the file name: nothing lands outside the inbox.
        let sneaky = ingest_bytes(&format!("../../{name}"), b"x").unwrap();
        assert_eq!(Path::new(&sneaky.path).parent(), Some(inbox_dir().as_path()));
        for f in [&first, &second, &sneaky] {
            let _ = std::fs::remove_file(&f.path);
        }
    }
}
