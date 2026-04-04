use std::fs;
use std::path::Path;

/// Compare a directory's fingerprint against a stored stamp. Returns true if changed.
/// Fingerprint is based on file mtimes and sizes for speed.
pub fn dir_changed(dir: &Path, stamp_path: &Path) -> bool {
    if !stamp_path.exists() {
        return true;
    }
    let current = fingerprint_dir(dir);
    let stored = fs::read_to_string(stamp_path).unwrap_or_default();
    current != stored
}

/// Write a directory's fingerprint to a stamp file.
pub fn write_dir_stamp(dir: &Path, stamp_path: &Path) {
    if let Some(parent) = stamp_path.parent() {
        fs::create_dir_all(parent).ok();
    }
    let fp = fingerprint_dir(dir);
    fs::write(stamp_path, fp).ok();
}


fn fingerprint_dir(dir: &Path) -> String {
    let mut entries: Vec<String> = Vec::new();
    collect_entries(dir, &mut entries);
    entries.sort();
    entries.join("\n")
}

fn collect_entries(dir: &Path, entries: &mut Vec<String>) {
    let Ok(read_dir) = fs::read_dir(dir) else { return };
    for entry in read_dir.flatten() {
        let path = entry.path();
        if path.is_dir() {
            let name = path.file_name().unwrap().to_string_lossy().to_string();
            if name.starts_with('.') || name == "target" {
                continue;
            }
            collect_entries(&path, entries);
        } else if path.extension().is_some_and(|e| e == "rs" || e == "toml" || e == "h") {
            if let Ok(meta) = fs::metadata(&path) {
                let size = meta.len();
                let mtime = meta
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|d| d.as_nanos())
                    .unwrap_or(0);
                entries.push(format!("{}:{}:{}", path.display(), size, mtime));
            }
        }
    }
}
