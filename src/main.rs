//! sd: a streamlined terminal UI for stable-diffusion.cpp.
//!
//! `sd` opens the interface: each line you type is an image request, shown in the terminal
//! (kitty/sixel/iTerm2) and saved to the current folder. Profiles run sd-cli locally or send
//! requests to an sd-server. Every request is logged to a resumable session.
//! `sd "a prompt"` generates one image without the interface and prints its path; `-f FILE`
//! reads that prompt from a file and `-b FILE` generates one image per line of a file.

mod config;
mod history;
mod menu;
mod prompts;
mod runner;
mod session;
mod syslog;
mod tui;

use std::{path::PathBuf, time::Instant};

use anyhow::{Context, Result};
use clap::Parser;

use crate::{
    config::Config,
    session::{Entry, Session},
};

#[derive(Parser)]
#[command(
    name = "sd",
    version,
    about = "Type a prompt, get an image (stable-diffusion.cpp)"
)]
struct Cli {
    /// Generate one image from this prompt and exit, without the interface
    prompt: Vec<String>,

    /// Generate one image from the prompt in FILE (its lines are joined; "-" = stdin)
    #[arg(short, long, value_name = "FILE", conflicts_with_all = ["prompt", "batch"])]
    file: Option<String>,

    /// Generate one image per line of FILE ("#" lines and blank lines are skipped; "-" = stdin)
    #[arg(short, long, value_name = "FILE", conflicts_with = "prompt")]
    batch: Option<String>,

    /// Profile to use (default: default_profile from the config)
    #[arg(short, long)]
    profile: Option<String>,

    /// Config file (default: ~/.config/sd-tui/config.toml, created on first run)
    #[arg(short, long, value_name = "PATH")]
    config: Option<PathBuf>,

    /// Folder to save images in (default: the current folder)
    #[arg(short, long, value_name = "DIR")]
    out: Option<PathBuf>,

    /// Resume a session: its images, prompts and log (default: the most recent one)
    #[arg(short, long, value_name = "ID", num_args = 0..=1, default_missing_value = "")]
    resume: Option<String>,

    /// List recent sessions and exit
    #[arg(long)]
    sessions: bool,

    /// List profiles and exit
    #[arg(short, long)]
    list: bool,
}

/// One-shot runs all go into this rolling session.
const CLI_SESSION: &str = "cli";

fn main() -> Result<()> {
    let cli = Cli::parse();
    let config_path = match cli.config {
        Some(p) => p,
        None => config::default_path()?,
    };
    let config = Config::load_or_init(&config_path)?;
    let profile_name = cli
        .profile
        .unwrap_or_else(|| config.default_profile.clone());
    let sessions_dir = session::dir()?;

    if cli.list {
        for (name, p) in &config.profiles {
            let mark = if *name == profile_name { "*" } else { " " };
            println!(
                "{mark} {name:<18} {}",
                p.description.clone().unwrap_or_else(|| p.summary())
            );
        }
        return Ok(());
    }
    if cli.sessions {
        for s in session::list(&sessions_dir)? {
            println!(
                "{:<17} {:>3} images {:>3} requests  {}",
                s.id, s.images, s.requests, s.last_prompt
            );
        }
        return Ok(());
    }

    let out_dir = match cli.out {
        Some(d) => d,
        None => std::env::current_dir().context("no current directory")?,
    };
    std::fs::create_dir_all(&out_dir).with_context(|| format!("creating {}", out_dir.display()))?;
    let out_dir = out_dir.canonicalize().unwrap_or(out_dir);

    let one_shot: Option<Vec<String>> = if let Some(f) = &cli.file {
        let prompt = prompts::one(&prompts::read(f)?);
        anyhow::ensure!(!prompt.is_empty(), "{f} has no prompt in it");
        Some(vec![prompt])
    } else if let Some(f) = &cli.batch {
        let list = prompts::batch(&prompts::read(f)?);
        anyhow::ensure!(!list.is_empty(), "{f} has no prompts in it");
        Some(list)
    } else if !cli.prompt.is_empty() {
        Some(vec![cli.prompt.join(" ")])
    } else {
        None
    };
    if let Some(list) = one_shot {
        let profile = config.profile(&profile_name)?;
        eprintln!("{profile_name}: {}", profile.summary());
        let mut log = Session::open(&sessions_dir, CLI_SESSION).unwrap_or_else(|_| Session {
            id: CLI_SESSION.into(),
            file: sessions_dir.join(format!("{CLI_SESSION}.jsonl")),
            entries: Vec::new(),
        });
        let total = list.len();
        let mut failed = 0;
        for (i, prompt) in list.into_iter().enumerate() {
            if total > 1 {
                eprintln!("[{}/{total}] {prompt}", i + 1);
            }
            match one_shot_image(&profile_name, &profile, &prompt, &out_dir, &mut log) {
                Ok(path) => println!("{}", path.display()),
                // A batch keeps going; a single prompt reports its error as the exit status.
                Err(e) if total > 1 => {
                    eprintln!("failed: {e:#}");
                    failed += 1;
                }
                Err(e) => return Err(e),
            }
        }
        anyhow::ensure!(failed == 0, "{failed} of {total} images failed");
        return Ok(());
    }

    let session = match cli.resume.as_deref() {
        None => Session::new(&sessions_dir),
        Some("") => Session::latest(&sessions_dir)?.context("no session to resume yet")?,
        Some(id) => Session::open(&sessions_dir, id)?,
    };
    tui::run(tui::Start {
        config,
        config_path,
        profile_name,
        out_dir,
        session,
        sessions_dir,
    })
}

fn one_shot_image(
    profile_name: &str,
    profile: &config::Profile,
    prompt: &str,
    out_dir: &std::path::Path,
    log: &mut Session,
) -> Result<PathBuf> {
    let started = Instant::now();
    let result = runner::run_blocking(profile.clone(), prompt.to_string(), out_dir.to_path_buf());
    let mut entry = Entry::new(
        prompt,
        profile_name,
        profile,
        if result.is_ok() { "ok" } else { "failed" },
    );
    match &result {
        Ok(path) => {
            entry.path = Some(path.clone());
            entry.seed = seed_from_name(path);
            entry.elapsed_s = Some((started.elapsed().as_secs_f64() * 10.0).round() / 10.0);
        }
        Err(e) => entry.error = Some(format!("{e:#}")),
    }
    if let Err(e) = log.append(entry) {
        syslog::err(&format!("couldn't write the session log: {e:#}"));
        eprintln!("warning: couldn't write the session log: {e:#}");
    }
    result
}

/// Seeds are in the file name: sd-YYYYmmdd-HHMMSS-sSEED.png
fn seed_from_name(path: &std::path::Path) -> Option<i64> {
    let stem = path.file_stem()?.to_str()?;
    let after = &stem[stem.rfind("-s")? + 2..];
    after.split('-').next()?.parse().ok()
}

#[cfg(test)]
mod tests {
    #[test]
    fn seed_from_file_name() {
        assert_eq!(
            super::seed_from_name("sd-20261123-200518-s878661746.png".as_ref()),
            Some(878661746)
        );
        assert_eq!(
            super::seed_from_name("sd-20261123-200518-s7-2.png".as_ref()),
            Some(7)
        );
        assert_eq!(super::seed_from_name("other.png".as_ref()), None);
    }
}
