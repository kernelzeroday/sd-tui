//! What was typed at the prompt (prompts and commands), shared by every session, like a
//! shell's history: ~/.local/share/sd-tui/history, one entry per line, oldest first.

use std::{
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};

/// Entries kept; the file is trimmed to this when it grows to twice as many.
const KEEP: usize = 1000;

pub fn file() -> Result<PathBuf> {
    let dirs = directories::ProjectDirs::from("", "", "sd-tui").context("no home directory")?;
    Ok(dirs.data_dir().join("history"))
}

/// The saved history, oldest first, without blank lines or repeats in a row. A missing file
/// is an empty history.
pub fn load(path: &Path) -> Result<Vec<String>> {
    let text = match fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    let mut out: Vec<String> = text
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(String::from)
        .collect();
    out.dedup();
    if out.len() >= 2 * KEEP {
        out.drain(..out.len() - KEEP);
        let mut text = out.join("\n");
        text.push('\n');
        fs::write(path, text).with_context(|| format!("writing {}", path.display()))?;
    }
    if out.len() > KEEP {
        out.drain(..out.len() - KEEP);
    }
    Ok(out)
}

/// Add one entry to the end of the file.
pub fn append(path: &Path, entry: &str) -> Result<()> {
    let entry = entry.replace(['\n', '\r'], " ");
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    }
    let mut f = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .with_context(|| format!("opening {}", path.display()))?;
    writeln!(f, "{}", entry.trim()).with_context(|| format!("writing {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_and_trim() {
        let dir = std::env::temp_dir().join(format!("sd-tui-test-history-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let path = dir.join("history");
        assert!(load(&path).unwrap().is_empty());

        append(&path, "a cat").unwrap();
        append(&path, "a cat").unwrap();
        append(&path, "two\nlines").unwrap();
        assert_eq!(load(&path).unwrap(), ["a cat", "two lines"]);

        for i in 0..2 * KEEP {
            append(&path, &format!("p{i}")).unwrap();
        }
        let h = load(&path).unwrap();
        assert_eq!(h.len(), KEEP);
        assert_eq!(h.last().unwrap(), &format!("p{}", 2 * KEEP - 1));
        // The file itself was trimmed too.
        assert_eq!(fs::read_to_string(&path).unwrap().lines().count(), KEEP);
        let _ = fs::remove_dir_all(&dir);
    }
}
