//! Walk the vault for markdown files.

use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use anyhow::{Context, Result};
use walkdir::WalkDir;

/// One markdown file found in the vault.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScannedFile {
    /// Path relative to the vault root, `/`-separated; this is the index key.
    pub rel_path: String,
    pub abs_path: PathBuf,
    /// Modification time in seconds since the Unix epoch.
    pub mtime: i64,
}

/// Every `*.md` under `root`, skipping dot-directories such as `.grain` and `.obsidian`.
pub fn scan_vault(root: &Path) -> Result<Vec<ScannedFile>> {
    let mut files = Vec::new();
    let walker = WalkDir::new(root)
        .follow_links(false)
        .into_iter()
        .filter_entry(|e| e.depth() == 0 || !is_hidden(e.file_name()));
    for entry in walker {
        let entry = entry.with_context(|| format!("scanning {}", root.display()))?;
        if !entry.file_type().is_file() || entry.path().extension().is_none_or(|x| x != "md") {
            continue;
        }
        let rel = entry
            .path()
            .strip_prefix(root)
            .with_context(|| format!("relativizing {}", entry.path().display()))?;
        let rel_path = rel
            .components()
            .map(|c| c.as_os_str().to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join("/");
        files.push(ScannedFile {
            rel_path,
            abs_path: entry.path().to_path_buf(),
            mtime: mtime_of(entry.path())?,
        });
    }
    files.sort_by(|a, b| a.rel_path.cmp(&b.rel_path));
    Ok(files)
}

/// Modification time of a file, in whole seconds since the Unix epoch.
pub fn mtime_of(path: &Path) -> Result<i64> {
    let modified = std::fs::metadata(path)
        .and_then(|m| m.modified())
        .with_context(|| format!("reading mtime of {}", path.display()))?;
    let secs = modified
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    Ok(secs)
}

fn is_hidden(name: &std::ffi::OsStr) -> bool {
    name.to_string_lossy().starts_with('.')
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use std::fs;

    #[test]
    fn finds_markdown_recursively_and_skips_hidden_dirs() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("a.md"), "x").unwrap();
        fs::create_dir_all(root.join("sub/deeper")).unwrap();
        fs::write(root.join("sub/deeper/b.md"), "x").unwrap();
        fs::write(root.join("sub/notes.txt"), "x").unwrap();
        fs::create_dir_all(root.join(".grain")).unwrap();
        fs::write(root.join(".grain/c.md"), "x").unwrap();
        fs::create_dir_all(root.join(".obsidian")).unwrap();
        fs::write(root.join(".obsidian/d.md"), "x").unwrap();

        let files = scan_vault(root).unwrap();
        let mut paths: Vec<&str> = files.iter().map(|f| f.rel_path.as_str()).collect();
        paths.sort();
        assert_eq!(paths, ["a.md", "sub/deeper/b.md"]);
        assert!(files.iter().all(|f| f.mtime > 0));
    }
}
