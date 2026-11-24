//! Prompts from text files. A prompt file is either one prompt (its lines are joined with
//! spaces, so a long prompt can be wrapped over several lines) or a batch (one prompt per line).
//! In both, blank lines and lines starting with `#` are ignored. `-` reads standard input.

use std::{io::Read, path::PathBuf};

use anyhow::{Context, Result};

pub fn read(path: &str) -> Result<String> {
    if path == "-" {
        let mut text = String::new();
        std::io::stdin()
            .read_to_string(&mut text)
            .context("reading prompts from standard input")?;
        return Ok(text);
    }
    let path = PathBuf::from(crate::runner::expand_home(path));
    std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))
}

fn lines(text: &str) -> impl Iterator<Item = &str> {
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
}

/// The whole file as one prompt.
pub fn one(text: &str) -> String {
    lines(text)
        .flat_map(str::split_whitespace)
        .collect::<Vec<_>>()
        .join(" ")
}

/// One prompt per line.
pub fn batch(text: &str) -> Vec<String> {
    lines(text)
        .map(|l| l.split_whitespace().collect::<Vec<_>>().join(" "))
        .collect()
}

/// Text files in `dir`, for the /file and /batch menus.
pub fn files_in(dir: &std::path::Path) -> Vec<String> {
    let mut out: Vec<String> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_file()))
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.ends_with(".txt") || n.ends_with(".prompt") || n.ends_with(".prompts"))
        .collect();
    out.sort();
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const FILE: &str =
        "# my prompts\n1girl, rain,\n  kyoto street\n\n# second\na fox in the snow\n";

    #[test]
    fn one_prompt_joins_lines_and_skips_comments() {
        assert_eq!(one(FILE), "1girl, rain, kyoto street a fox in the snow");
    }

    #[test]
    fn batch_is_one_prompt_per_line() {
        assert_eq!(
            batch(FILE),
            vec!["1girl, rain,", "kyoto street", "a fox in the snow"]
        );
    }
}
