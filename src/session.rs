//! Sessions: an append-only JSONL log of every request (prompt, settings, seed, file, timing,
//! outcome) in ~/.local/share/sd-tui/sessions/<id>.jsonl. Resuming a session restores its
//! image gallery and prompt history and keeps appending to the same file.

use std::{
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::config::Profile;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Entry {
    /// Local time, RFC 3339.
    pub time: String,
    pub prompt: String,
    pub profile: String,
    /// "ok", "failed" or "cancelled".
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seed: Option<i64>,
    /// Absolute path of the saved image (status "ok").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub elapsed_s: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// The full settings used, so an image can be reproduced.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub settings: Option<Profile>,
}

impl Entry {
    pub fn new(prompt: &str, profile_name: &str, profile: &Profile, status: &str) -> Entry {
        Entry {
            time: chrono::Local::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, false),
            prompt: prompt.to_string(),
            profile: profile_name.to_string(),
            status: status.to_string(),
            seed: None,
            path: None,
            elapsed_s: None,
            error: None,
            settings: Some(profile.clone()),
        }
    }

    pub fn is_image(&self) -> bool {
        self.status == "ok" && self.path.is_some()
    }
}

pub struct Session {
    pub id: String,
    pub file: PathBuf,
    pub entries: Vec<Entry>,
}

/// Summary row for /sessions and `sd --sessions`.
pub struct Summary {
    pub id: String,
    pub images: usize,
    pub requests: usize,
    pub last_prompt: String,
}

pub fn dir() -> Result<PathBuf> {
    let dirs = directories::ProjectDirs::from("", "", "sd-tui").context("no home directory")?;
    Ok(dirs.data_dir().join("sessions"))
}

impl Session {
    /// A new, empty session. Its file is created on the first append.
    pub fn new(dir: &Path) -> Session {
        let base = chrono::Local::now().format("%Y%m%d-%H%M%S").to_string();
        let mut id = base.clone();
        let mut n = 2;
        while dir.join(format!("{id}.jsonl")).exists() {
            id = format!("{base}-{n}");
            n += 1;
        }
        Session {
            file: dir.join(format!("{id}.jsonl")),
            id,
            entries: Vec::new(),
        }
    }

    pub fn open(dir: &Path, id: &str) -> Result<Session> {
        let id = id.trim_end_matches(".jsonl");
        let file = dir.join(format!("{id}.jsonl"));
        if !file.exists() {
            bail!("no session '{id}' in {}", dir.display());
        }
        let text =
            fs::read_to_string(&file).with_context(|| format!("reading {}", file.display()))?;
        // Skip unreadable lines rather than refusing the whole session.
        let entries = text
            .lines()
            .filter_map(|l| serde_json::from_str(l).ok())
            .collect();
        Ok(Session {
            id: id.to_string(),
            file,
            entries,
        })
    }

    /// The most recently written session, if any.
    pub fn latest(dir: &Path) -> Result<Option<Session>> {
        let newest = list_files(dir)?.into_iter().max_by_key(|(_, mtime)| *mtime);
        match newest {
            Some((path, _)) => {
                let id = path
                    .file_stem()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into_owned();
                Ok(Some(Session::open(dir, &id)?))
            }
            None => Ok(None),
        }
    }

    pub fn append(&mut self, entry: Entry) -> Result<()> {
        if let Some(parent) = self.file.parent() {
            fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
        }
        let mut f = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.file)
            .with_context(|| format!("opening {}", self.file.display()))?;
        writeln!(f, "{}", serde_json::to_string(&entry)?)
            .with_context(|| format!("writing {}", self.file.display()))?;
        self.entries.push(entry);
        Ok(())
    }

    pub fn images(&self) -> Vec<&Entry> {
        self.entries.iter().filter(|e| e.is_image()).collect()
    }
}

fn list_files(dir: &Path) -> Result<Vec<(PathBuf, std::time::SystemTime)>> {
    if !dir.exists() {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    for entry in fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))? {
        let path = entry?.path();
        if path.extension().is_some_and(|e| e == "jsonl") {
            let mtime = fs::metadata(&path)
                .and_then(|m| m.modified())
                .unwrap_or(std::time::UNIX_EPOCH);
            out.push((path, mtime));
        }
    }
    Ok(out)
}

/// Sessions, most recent first.
pub fn list(dir: &Path) -> Result<Vec<Summary>> {
    let mut files = list_files(dir)?;
    files.sort_by_key(|(_, mtime)| std::cmp::Reverse(*mtime));
    let mut out = Vec::new();
    for (path, _) in files {
        let id = path
            .file_stem()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned();
        if let Ok(s) = Session::open(dir, &id) {
            out.push(Summary {
                images: s.images().len(),
                requests: s.entries.len(),
                last_prompt: s
                    .entries
                    .last()
                    .map(|e| e.prompt.clone())
                    .unwrap_or_default(),
                id,
            });
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, DEFAULT_CONFIG};

    fn tmpdir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("sd-tui-test-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        d
    }

    #[test]
    fn append_open_and_list() {
        let dir = tmpdir("sessions");
        let profile = toml::from_str::<Config>(DEFAULT_CONFIG)
            .unwrap()
            .profile("quick")
            .unwrap();
        let mut s = Session::new(&dir);
        assert!(!s.file.exists(), "file is only created on first append");
        let mut ok = Entry::new("a fox", "quick", &profile, "ok");
        ok.seed = Some(7);
        ok.path = Some(PathBuf::from("/tmp/fox.png"));
        s.append(ok).unwrap();
        let mut bad = Entry::new("a whale", "quick", &profile, "failed");
        bad.error = Some("boom".into());
        s.append(bad).unwrap();

        let again = Session::open(&dir, &s.id).unwrap();
        assert_eq!(again.entries.len(), 2);
        assert_eq!(again.images().len(), 1);
        assert_eq!(again.images()[0].seed, Some(7));
        assert_eq!(again.entries[0].settings.as_ref().unwrap().width, 384);

        let latest = Session::latest(&dir).unwrap().unwrap();
        assert_eq!(latest.id, s.id);
        let listed = list(&dir).unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!((listed[0].images, listed[0].requests), (1, 2));
        assert_eq!(listed[0].last_prompt, "a whale");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn bad_lines_are_skipped() {
        let dir = tmpdir("badlines");
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("x.jsonl"),
            "not json\n{\"time\":\"t\",\"prompt\":\"p\",\"profile\":\"q\",\"status\":\"ok\"}\n",
        )
        .unwrap();
        let s = Session::open(&dir, "x").unwrap();
        assert_eq!(s.entries.len(), 1);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn missing_session_errors() {
        assert!(Session::open(&tmpdir("missing"), "nope").is_err());
    }
}
