//! The full-screen interface: the session's images on top (newest first shown, ←/→ to browse,
//! the prompt that made each one underneath), a short log, a status line, and a prompt.
//! Every line typed is an image request; lines starting with "/" are commands.

use std::{
    collections::VecDeque,
    path::{Path, PathBuf},
    sync::mpsc::{Receiver, TryRecvError, channel},
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
use crossterm::event::{self, Event as TermEvent, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::{
    DefaultTerminal, Frame,
    layout::{Alignment, Constraint, Layout, Rect},
    style::{Color, Modifier, Style, Stylize},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Paragraph, Wrap},
};
use ratatui_image::{
    FilterType, Resize, StatefulImage,
    picker::{Picker, ProtocolType},
    protocol::StatefulProtocol,
};

use crate::{
    config::{Config, Profile},
    menu::{self, Action, Menu},
    runner::{self, Event, Job},
    session::{self, Entry, Session},
};

const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
const ACCENT: Color = Color::Rgb(255, 160, 200);

const COMMANDS: &[(&str, &str)] = &[
    ("/help", "show commands and settings"),
    ("/profiles", "list profiles"),
    (
        "/profile",
        "NAME: switch profile (drops unsaved /set changes)",
    ),
    ("/size", "WxH: image size, e.g. /size 512x768"),
    (
        "/aspect",
        "W:H|square|portrait|landscape|wide|tall: reshape, keeping the pixel count",
    ),
    ("/steps", "N: sampling steps"),
    ("/cfg", "N: guidance scale"),
    ("/seed", "N|random: fixed seed, or a new one per image"),
    ("/sampler", "NAME: euler, euler_a, dpm++2m, er_sde, ..."),
    ("/neg", "TEXT: negative prompt (/neg off clears it)"),
    (
        "/upscale",
        "PATH|NAME|off: upscale after generating (local: ESRGAN file, server: hires upscaler)",
    ),
    ("/show", "show all current settings"),
    (
        "/set",
        "KEY=VALUE ...: change several settings at once (any setting: /help)",
    ),
    ("/save", "[NAME]: save the current settings as a profile"),
    ("/load", "PATH: load another config file"),
    ("/reload", "re-read the config file"),
    (
        "/reuse",
        "put the shown image's prompt in the input to edit",
    ),
    ("/open", "open the shown image in the system viewer"),
    ("/sessions", "list recent sessions"),
    (
        "/resume",
        "[ID]: switch to a session (default: the most recent other one)",
    ),
    ("/new", "start a new session"),
    ("/config", "show the config and session file paths"),
    ("/clear", "clear the log"),
    ("/quit", "exit (also Ctrl+C)"),
];

const SET_HELP: &str = "every setting is also a command (/KEY VALUE, bare /KEY shows it): size=WxH aspect=W:H width height steps cfg sampler seed=N|random negative=\"...\" \
upscale=PATH|NAME|off vae_tiling=on|off model diffusion_model llm vae tokenizer lora_dir sd_cli \
target=local|server url recipe hires_scale hires_denoise extra=\"--flag value\" (local only)";

const KEYS_HELP: &str = "bare / or /SETTING opens a menu · keys: Enter generate · ←/→ browse images (empty prompt) · Home/End first/last · \
Esc cancel (and clear queue) · ↑/↓ prompt history · Tab complete · Ctrl+C quit";

struct Request {
    prompt: String,
    profile_name: String,
    profile: Profile,
}

struct Running {
    job: Job,
    rx: Receiver<Event>,
    started: Instant,
    request: Request,
    stage: String,
    progress: Option<(u32, u32)>,
}

/// The decoded image currently on screen, keyed by its file path.
struct Shown {
    path: PathBuf,
    dims: (u32, u32),
    protocol: StatefulProtocol,
}

enum LogKind {
    Info,
    Good,
    Bad,
    Dim,
}

pub struct App {
    config: Config,
    config_path: PathBuf,
    profile_name: String,
    profile: Profile,
    modified: bool,
    out_dir: PathBuf,
    picker: Picker,

    session: Session,
    sessions_dir: PathBuf,
    /// Index into `session.images()` of the image on screen.
    view: Option<usize>,
    shown: Option<Shown>,
    /// Path we failed to load, so we don't retry it every frame.
    unreadable: Option<PathBuf>,

    input: String,
    cursor: usize,
    history: Vec<String>,
    history_pos: Option<usize>,

    log: Vec<(LogKind, String)>,
    running: Option<Running>,
    queue: VecDeque<Request>,
    tick: usize,
    quit: bool,
    /// Pop-up menu for a bare command, when open.
    menu: Option<Menu>,
}

pub struct Start {
    pub config: Config,
    pub config_path: PathBuf,
    pub profile_name: String,
    pub out_dir: PathBuf,
    pub session: Session,
    pub sessions_dir: PathBuf,
}

pub fn run(start: Start) -> Result<()> {
    let profile = start.config.profile(&start.profile_name)?;
    let mut terminal = ratatui::init();
    // A paste arrives as one event instead of keystrokes, so its newlines can't press Enter.
    let _ = crossterm::execute!(std::io::stdout(), crossterm::event::EnableBracketedPaste);
    let picker = make_picker(&start.config.display.protocol);
    let mut app = App {
        config: start.config,
        config_path: start.config_path,
        profile_name: start.profile_name,
        profile,
        modified: false,
        out_dir: start.out_dir,
        picker,
        session: start.session,
        sessions_dir: start.sessions_dir,
        view: None,
        shown: None,
        unreadable: None,
        input: String::new(),
        cursor: 0,
        history: Vec::new(),
        history_pos: None,
        log: Vec::new(),
        running: None,
        queue: VecDeque::new(),
        tick: 0,
        quit: false,
        menu: None,
    };
    app.info(format!(
        "sd · profile '{}' · images go to {} · /help for commands",
        app.profile_name,
        app.out_dir.display()
    ));
    app.dim(format!("display: {:?}", app.picker.protocol_type()));
    app.adopt_session();
    let result = app.main_loop(&mut terminal);
    let _ = crossterm::execute!(std::io::stdout(), crossterm::event::DisableBracketedPaste);
    ratatui::restore();
    result
}

fn make_picker(protocol: &str) -> Picker {
    let mut picker = Picker::from_query_stdio().unwrap_or_else(|_| Picker::halfblocks());
    let forced = match protocol {
        "kitty" => Some(ProtocolType::Kitty),
        "sixel" => Some(ProtocolType::Sixel),
        "iterm2" => Some(ProtocolType::Iterm2),
        "halfblocks" => Some(ProtocolType::Halfblocks),
        _ => None,
    };
    if let Some(p) = forced {
        picker.set_protocol_type(p);
    }
    picker
}

impl App {
    fn main_loop(&mut self, terminal: &mut DefaultTerminal) -> Result<()> {
        while !self.quit {
            self.drain_events();
            self.start_next();
            self.load_view();
            terminal.draw(|f| self.draw(f))?;
            if event::poll(Duration::from_millis(80))? {
                match event::read()? {
                    TermEvent::Key(key) if key.kind == KeyEventKind::Press => self.on_key(key),
                    TermEvent::Paste(text) => self.on_paste(&text),
                    _ => {}
                }
            }
            self.tick = self.tick.wrapping_add(1);
        }
        if let Some(r) = self.running.take() {
            r.job.cancel();
            self.record(&r.request, "cancelled", |e| {
                e.error = Some("quit while generating".into())
            });
        }
        Ok(())
    }

    // ---- session & gallery ------------------------------------------------------------

    /// Show the newest image of the (possibly resumed) session and load its prompts into the
    /// history.
    fn adopt_session(&mut self) {
        self.history = self
            .session
            .entries
            .iter()
            .map(|e| e.prompt.clone())
            .collect();
        self.history.dedup();
        self.history_pos = None;
        let n = self.session.images().len();
        self.view = n.checked_sub(1);
        self.shown = None;
        self.unreadable = None;
        if self.session.entries.is_empty() {
            self.dim(format!("new session {}", self.session.id));
        } else {
            self.good(format!(
                "resumed session {} · {} images from {} requests",
                self.session.id,
                n,
                self.session.entries.len()
            ));
        }
    }

    fn viewed(&self) -> Option<&Entry> {
        self.view
            .and_then(|i| self.session.images().get(i).copied())
    }

    /// Decode the viewed image if it isn't the one already on screen.
    fn load_view(&mut self) {
        let Some(path) = self.viewed().and_then(|e| e.path.clone()) else {
            self.shown = None;
            return;
        };
        if self.shown.as_ref().is_some_and(|s| s.path == path)
            || self.unreadable.as_ref() == Some(&path)
        {
            return;
        }
        match decode(&path) {
            Ok(img) => {
                let dims = (img.width(), img.height());
                let protocol = self.picker.new_resize_protocol(img);
                self.shown = Some(Shown {
                    path,
                    dims,
                    protocol,
                });
                self.unreadable = None;
            }
            Err(e) => {
                self.shown = None;
                self.bad(format!("can't show {}: {e:#}", path.display()));
                self.unreadable = Some(path);
            }
        }
    }

    fn browse(&mut self, to: Browse) {
        let n = self.session.images().len();
        if n == 0 {
            return;
        }
        let cur = self.view.unwrap_or(n - 1);
        self.view = Some(match to {
            Browse::Prev => cur.saturating_sub(1),
            Browse::Next => (cur + 1).min(n - 1),
            Browse::First => 0,
            Browse::Last => n - 1,
        });
    }

    fn record(&mut self, req: &Request, status: &str, fill: impl FnOnce(&mut Entry)) {
        let mut entry = Entry::new(&req.prompt, &req.profile_name, &req.profile, status);
        fill(&mut entry);
        if let Err(e) = self.session.append(entry) {
            self.bad(format!("couldn't write the session log: {e:#}"));
        }
    }

    // ---- generation -------------------------------------------------------------------

    fn start_next(&mut self) {
        if self.running.is_some() {
            return;
        }
        let Some(request) = self.queue.pop_front() else {
            return;
        };
        let (tx, rx) = channel();
        let job = runner::start(
            request.profile.clone(),
            request.prompt.clone(),
            self.out_dir.clone(),
            tx,
        );
        self.running = Some(Running {
            job,
            rx,
            started: Instant::now(),
            request,
            stage: "starting".into(),
            progress: None,
        });
    }

    fn drain_events(&mut self) {
        let mut finished = None;
        if let Some(r) = self.running.as_mut() {
            loop {
                match r.rx.try_recv() {
                    Ok(Event::Stage(s)) => {
                        r.stage = s;
                        r.progress = None;
                    }
                    Ok(Event::Progress { done, total }) => r.progress = Some((done, total)),
                    Ok(Event::Log(l)) => {
                        let kind = if l.starts_with("warning:") {
                            LogKind::Dim
                        } else {
                            LogKind::Bad
                        };
                        self.log.push((kind, l));
                    }
                    Ok(done @ (Event::Done { .. } | Event::Failed(_))) => {
                        finished = Some(done);
                        break;
                    }
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => {
                        finished = Some(Event::Failed("generation stopped unexpectedly".into()));
                        break;
                    }
                }
            }
        }
        let Some(event) = finished else { return };
        let Some(r) = self.running.take() else { return };
        match event {
            Event::Done {
                path,
                elapsed,
                seed,
            } => {
                self.good(format!(
                    "saved {} · {:.0}s · seed {seed}",
                    display_path(&path, &self.out_dir),
                    elapsed.as_secs_f64()
                ));
                self.record(&r.request, "ok", |e| {
                    e.seed = Some(seed);
                    e.path = Some(path);
                    e.elapsed_s = Some((elapsed.as_secs_f64() * 10.0).round() / 10.0);
                });
                self.view = self.session.images().len().checked_sub(1);
            }
            Event::Failed(err) => {
                let status = if err.contains("cancelled") {
                    "cancelled"
                } else {
                    "failed"
                };
                if status == "failed" {
                    self.bad(format!("failed: {err}"));
                }
                self.record(&r.request, status, |e| e.error = Some(err));
            }
            _ => {}
        }
    }

    // ---- input ------------------------------------------------------------------------

    fn on_key(&mut self, key: KeyEvent) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        if self.menu.is_some() {
            self.menu_key(key, ctrl);
            return;
        }
        let empty = self.input.is_empty();
        match key.code {
            KeyCode::Char('c') if ctrl => self.quit = true,
            KeyCode::Char('d') if ctrl && empty => self.quit = true,
            KeyCode::Char('u') if ctrl => {
                self.input.clear();
                self.cursor = 0;
            }
            KeyCode::Esc => self.on_escape(),
            KeyCode::Enter => self.submit(),
            KeyCode::Tab => self.complete(),
            KeyCode::Up => self.history_step(-1),
            KeyCode::Down => self.history_step(1),
            // With an empty prompt the arrows browse the session's images.
            KeyCode::Left if empty => self.browse(Browse::Prev),
            KeyCode::Right if empty => self.browse(Browse::Next),
            KeyCode::Home if empty => self.browse(Browse::First),
            KeyCode::End if empty => self.browse(Browse::Last),
            KeyCode::Left => self.cursor = self.cursor.saturating_sub(1),
            KeyCode::Right => self.cursor = (self.cursor + 1).min(self.input.chars().count()),
            KeyCode::Home => self.cursor = 0,
            KeyCode::Char('a') if ctrl => self.cursor = 0,
            KeyCode::End => self.cursor = self.input.chars().count(),
            KeyCode::Char('e') if ctrl => self.cursor = self.input.chars().count(),
            KeyCode::Backspace => {
                if self.cursor > 0 {
                    self.cursor -= 1;
                    let at = byte_index(&self.input, self.cursor);
                    self.input.remove(at);
                }
            }
            KeyCode::Delete => {
                if self.cursor < self.input.chars().count() {
                    let at = byte_index(&self.input, self.cursor);
                    self.input.remove(at);
                }
            }
            KeyCode::Char(c) if !ctrl => {
                let at = byte_index(&self.input, self.cursor);
                self.input.insert(at, c);
                self.cursor += 1;
                self.history_pos = None;
            }
            _ => {}
        }
    }

    /// Pasted text goes into the prompt (or the open menu's filter) with its lines joined by
    /// spaces; it never submits.
    fn on_paste(&mut self, text: &str) {
        let flat = flatten_paste(text);
        if let Some(m) = self.menu.as_mut() {
            flat.chars().for_each(|ch| m.push(ch));
            return;
        }
        let at = byte_index(&self.input, self.cursor);
        self.input.insert_str(at, &flat);
        self.cursor += flat.chars().count();
        self.history_pos = None;
    }

    fn menu_key(&mut self, key: KeyEvent, ctrl: bool) {
        let Some(m) = self.menu.as_mut() else { return };
        match key.code {
            KeyCode::Char('c') if ctrl => self.quit = true,
            KeyCode::Esc => self.menu = None,
            KeyCode::Up => m.move_by(-1),
            KeyCode::Down => m.move_by(1),
            KeyCode::PageUp => m.move_by(-8),
            KeyCode::PageDown => m.move_by(8),
            KeyCode::Backspace => m.pop(),
            KeyCode::Char(ch) if !ctrl => m.push(ch),
            KeyCode::Enter => {
                let Some(m) = self.menu.take() else { return };
                let Some(value) = m.chosen() else { return };
                // Choosing runs the same command as typing it, so validation is shared.
                let line = match &m.action {
                    Action::Set(key) => format!("/{key} {value}"),
                    Action::Profile => format!("/profile {value}"),
                    Action::Resume => format!("/resume {value}"),
                    Action::Command => value,
                };
                if let Err(e) = self.command(&line) {
                    self.bad(format!("{e:#}"));
                }
            }
            _ => {}
        }
    }

    fn on_escape(&mut self) {
        // The running job stays in `running` until its thread reports back, so the cancelled
        // request is logged once, by drain_events.
        let running_prompt = self.running.as_ref().map(|r| {
            r.job.cancel();
            ellipsize(&r.request.prompt, 40)
        });
        if let Some(prompt) = running_prompt {
            let dropped = self.queue.len();
            for req in std::mem::take(&mut self.queue) {
                self.record(&req, "cancelled", |e| {
                    e.error = Some("dropped from the queue".into())
                });
            }
            let extra = if dropped > 0 {
                format!(" and dropped {dropped} queued")
            } else {
                String::new()
            };
            self.info(format!("cancelling \"{prompt}\"{extra}"));
        } else {
            self.input.clear();
            self.cursor = 0;
        }
    }

    fn submit(&mut self) {
        let text = self.input.trim().to_string();
        self.input.clear();
        self.cursor = 0;
        self.history_pos = None;
        if text.is_empty() {
            return;
        }
        if self.history.last() != Some(&text) {
            self.history.push(text.clone());
        }
        if text.starts_with('/') {
            if let Err(e) = self.command(&text) {
                self.bad(format!("{e:#}"));
            }
        } else {
            if self.running.is_some() {
                self.dim(format!("queued \"{}\"", ellipsize(&text, 50)));
            }
            self.queue.push_back(Request {
                prompt: text,
                profile_name: self.profile_name.clone(),
                profile: self.profile.clone(),
            });
        }
    }

    fn complete(&mut self) {
        if !self.input.starts_with('/') || self.input.contains(' ') {
            return;
        }
        let matches: Vec<_> = COMMANDS
            .iter()
            .filter(|(c, _)| c.starts_with(self.input.as_str()))
            .collect();
        match matches.as_slice() {
            [(only, _)] => {
                self.input = format!("{only} ");
                self.cursor = self.input.chars().count();
            }
            [] => {}
            many => {
                let names: Vec<_> = many.iter().map(|(c, _)| *c).collect();
                self.dim(names.join("  "));
            }
        }
    }

    fn history_step(&mut self, dir: i32) {
        if self.history.is_empty() {
            return;
        }
        let next = match (self.history_pos, dir) {
            (None, -1) => Some(self.history.len() - 1),
            (None, _) => None,
            (Some(0), -1) => Some(0),
            (Some(i), -1) => Some(i - 1),
            (Some(i), _) if i + 1 < self.history.len() => Some(i + 1),
            (Some(_), _) => None,
        };
        self.history_pos = next;
        self.input = next.map(|i| self.history[i].clone()).unwrap_or_default();
        self.cursor = self.input.chars().count();
    }

    // ---- commands ---------------------------------------------------------------------

    fn command(&mut self, text: &str) -> Result<()> {
        let (cmd, rest) = text.split_once(' ').unwrap_or((text, ""));
        let rest = rest.trim();
        match cmd {
            "/help" | "/h" | "/?" => {
                for (c, d) in COMMANDS {
                    self.dim(format!("{c:<10} {d}"));
                }
                self.dim(SET_HELP.to_string());
                self.dim(KEYS_HELP.to_string());
            }
            "/profiles" | "/ls" => {
                let lines: Vec<String> = self
                    .config
                    .profiles
                    .iter()
                    .map(|(name, p)| {
                        let mark = if *name == self.profile_name { "*" } else { " " };
                        format!(
                            "{mark} {name:<18} {}",
                            p.description.clone().unwrap_or_else(|| p.summary())
                        )
                    })
                    .collect();
                for l in lines {
                    self.info(l);
                }
            }
            "/" => self.menu = Some(menu::commands(COMMANDS)),
            "/profile" | "/p" => {
                if rest.is_empty() {
                    self.menu = Some(menu::profiles(&self.config, &self.profile_name));
                } else {
                    self.profile = self.config.profile(rest)?;
                    self.profile_name = rest.to_string();
                    self.modified = false;
                    self.good(format!("profile '{rest}': {}", self.profile.summary()));
                }
            }
            "/show" => {
                let text = toml::to_string_pretty(&self.profile).context("formatting profile")?;
                self.info(format!(
                    "[profiles.{}]{}",
                    self.profile_name,
                    if self.modified { " (modified)" } else { "" }
                ));
                for l in text.lines() {
                    self.dim(l.to_string());
                }
            }
            "/set" => {
                if rest.is_empty() {
                    self.info(SET_HELP.to_string());
                    return Ok(());
                }
                let words = shlex::split(rest)
                    .context("couldn't parse /set arguments (unbalanced quotes?)")?;
                let mut changed = Vec::new();
                for w in words {
                    let (k, v) = w
                        .split_once('=')
                        .with_context(|| format!("'{w}' is not KEY=VALUE"))?;
                    changed.push(self.profile.set(k.trim(), v)?);
                }
                self.modified = true;
                self.good(format!(
                    "{} · {}",
                    changed.join(", "),
                    self.profile.summary()
                ));
            }
            "/save" => {
                let name = if rest.is_empty() {
                    self.profile_name.clone()
                } else {
                    rest.to_string()
                };
                self.config
                    .profiles
                    .insert(name.clone(), self.profile.clone());
                self.config.save(&self.config_path)?;
                self.profile_name = name.clone();
                self.modified = false;
                self.good(format!(
                    "saved profile '{name}' to {}",
                    self.config_path.display()
                ));
            }
            "/load" => {
                anyhow::ensure!(!rest.is_empty(), "usage: /load PATH");
                let path = PathBuf::from(runner::expand_home(rest));
                let config = Config::load(&path)?;
                let name = config.default_profile.clone();
                self.profile = config.profile(&name)?;
                self.config = config;
                self.config_path = path;
                self.profile_name = name.clone();
                self.modified = false;
                self.good(format!(
                    "loaded {} · profile '{name}'",
                    self.config_path.display()
                ));
            }
            "/reload" => {
                self.config = Config::load(&self.config_path)?;
                self.profile = self.config.profile(&self.profile_name)?;
                self.modified = false;
                self.good(format!("reloaded {}", self.config_path.display()));
            }
            "/reuse" => {
                let prompt = self
                    .viewed()
                    .map(|e| e.prompt.clone())
                    .context("no image shown")?;
                self.cursor = prompt.chars().count();
                self.input = prompt;
            }
            "/open" => {
                let path = self
                    .viewed()
                    .and_then(|e| e.path.clone())
                    .context("no image shown")?;
                std::process::Command::new("xdg-open")
                    .arg(&path)
                    .stdin(std::process::Stdio::null())
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .spawn()
                    .context("could not run xdg-open")?;
                self.dim(format!("opened {}", path.display()));
            }
            "/sessions" => {
                let rows = session::list(&self.sessions_dir)?;
                if rows.is_empty() {
                    self.info("no saved sessions yet".into());
                }
                for s in rows.iter().take(15) {
                    let mark = if s.id == self.session.id { "*" } else { " " };
                    self.info(format!(
                        "{mark} {:<17} {:>3} images {:>3} requests  \"{}\"",
                        s.id,
                        s.images,
                        s.requests,
                        ellipsize(&s.last_prompt, 40)
                    ));
                }
            }
            "/resume" => {
                anyhow::ensure!(
                    self.running.is_none() && self.queue.is_empty(),
                    "wait for (or Esc) the running image first"
                );
                if rest.is_empty() {
                    let rows = session::list(&self.sessions_dir)?;
                    anyhow::ensure!(!rows.is_empty(), "no saved sessions yet");
                    self.menu = Some(menu::sessions(&rows, &self.session.id));
                    return Ok(());
                }
                self.session = Session::open(&self.sessions_dir, rest)?;
                self.adopt_session();
            }
            "/new" => {
                anyhow::ensure!(
                    self.running.is_none() && self.queue.is_empty(),
                    "wait for (or Esc) the running image first"
                );
                self.session = Session::new(&self.sessions_dir);
                self.adopt_session();
            }
            "/config" => {
                self.info(format!("config: {}", self.config_path.display()));
                self.info(format!("session: {}", self.session.file.display()));
            }
            "/clear" => self.log.clear(),
            "/quit" | "/q" | "/exit" => self.quit = true,
            other => {
                // Any setting works as a command: `/cfg 4.5`, `/seed random`; bare `/cfg` shows it.
                let key = other.trim_start_matches('/');
                let Some(current) = self.profile.get(key) else {
                    anyhow::bail!("unknown command {other} (/help)");
                };
                if rest.is_empty() {
                    let shown_seed = self.viewed().and_then(|e| e.seed);
                    match menu::for_setting(key, &self.profile, shown_seed) {
                        Some(m) => self.menu = Some(m),
                        None => self.info(format!("{key} = {current}")),
                    }
                } else {
                    let changed = self.profile.set(key, rest)?;
                    self.modified = true;
                    self.good(format!("{changed} · {}", self.profile.summary()));
                }
            }
        }
        Ok(())
    }

    // ---- log --------------------------------------------------------------------------

    fn push(&mut self, kind: LogKind, s: String) {
        self.log.push((kind, s));
        if self.log.len() > 500 {
            self.log.drain(..100);
        }
    }
    fn info(&mut self, s: String) {
        self.push(LogKind::Info, s);
    }
    fn good(&mut self, s: String) {
        self.push(LogKind::Good, s);
    }
    fn bad(&mut self, s: String) {
        self.push(LogKind::Bad, s);
    }
    fn dim(&mut self, s: String) {
        self.push(LogKind::Dim, s);
    }

    // ---- drawing ----------------------------------------------------------------------

    fn draw(&mut self, f: &mut Frame) {
        let has_image = self.view.is_some();
        let log_height = if has_image { 4 } else { 12 }
            .min(f.area().height / 3)
            .max(3);
        let [image_area, log_area, status_area, input_area] = Layout::vertical([
            Constraint::Min(4),
            Constraint::Length(log_height),
            Constraint::Length(1),
            Constraint::Length(3),
        ])
        .areas(f.area());

        self.draw_image(f, image_area);
        self.draw_log(f, log_area);
        self.draw_status(f, status_area);
        self.draw_input(f, input_area);
    }

    fn draw_menu(&self, f: &mut Frame, area: Rect) {
        let Some(m) = &self.menu else { return };
        let rows = m.visible();
        let height = (rows.len() as u16 + 4).clamp(6, area.height);
        let width = area.width.min(100);
        let popup = Rect {
            x: area.x + (area.width - width) / 2,
            y: area.y + (area.height - height) / 2,
            width,
            height,
        };
        f.render_widget(ratatui::widgets::Clear, popup);
        let filter = if m.filter.is_empty() {
            String::new()
        } else {
            format!(" · filter: {}", m.filter)
        };
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(Color::Cyan))
            .title(Span::styled(format!(" {}{filter} ", m.title), Style::default().fg(Color::Cyan)))
            .title_bottom(Line::from(
                " ↑/↓ move · Enter choose · type to filter (no match = use what you typed) · Esc close ".dim(),
            ));
        let inner = block.inner(popup);
        f.render_widget(block, popup);
        let visible = inner.height as usize;
        let start = m.cursor.saturating_sub(visible.saturating_sub(1));
        let value_width = rows
            .iter()
            .map(|&i| m.choices[i].value.chars().count())
            .max()
            .unwrap_or(0)
            .min(40);
        let mut lines: Vec<Line> = rows
            .iter()
            .enumerate()
            .skip(start)
            .take(visible)
            .map(|(row, &i)| {
                let ch = &m.choices[i];
                let selected = row == m.cursor;
                let current = m.current.as_deref() == Some(ch.value.as_str());
                let mark = if current { "•" } else { " " };
                let style = if selected {
                    Style::default().fg(Color::Black).bg(ACCENT)
                } else {
                    Style::default()
                };
                Line::from(vec![
                    Span::styled(
                        format!(" {mark} {:<value_width$} ", ellipsize(&ch.value, 40)),
                        style,
                    ),
                    Span::styled(
                        format!(" {}", ch.note),
                        Style::default().fg(Color::DarkGray),
                    ),
                ])
            })
            .collect();
        if lines.is_empty() {
            let typed = m.filter.trim();
            lines.push(Line::from(format!("   Enter uses \"{typed}\"").fg(ACCENT)));
        }
        f.render_widget(Paragraph::new(lines), inner);
    }

    fn draw_image(&mut self, f: &mut Frame, area: Rect) {
        if self.menu.is_some() {
            // Graphics protocols draw over text, so the menu replaces the image while open.
            self.draw_menu(f, area);
            return;
        }
        let count = self.session.images().len();
        let entry = self.viewed().cloned();
        let mut block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(Color::DarkGray));
        match (&entry, &self.shown) {
            (Some(e), shown) => {
                let i = self.view.unwrap_or(0) + 1;
                let path = e.path.as_deref().unwrap_or(Path::new("?"));
                let dims = shown
                    .as_ref()
                    .map(|s| format!(" · {}x{}", s.dims.0, s.dims.1))
                    .unwrap_or_default();
                let nav = if count > 1 { " · ←/→" } else { "" };
                block = block.title(Span::styled(
                    format!(
                        " {i}/{count} · {}{dims}{nav} ",
                        display_path(path, &self.out_dir)
                    ),
                    Style::default().fg(ACCENT),
                ));
                // The prompt that made this image, then its seed and profile.
                let width = area.width.saturating_sub(6) as usize;
                let tail = format!(
                    " · seed {} · {}",
                    e.seed.map_or("?".into(), |s| s.to_string()),
                    e.profile
                );
                let prompt = ellipsize(
                    &e.prompt,
                    width.saturating_sub(tail.chars().count() + 2).max(8),
                );
                block = block.title_bottom(Line::from(vec![
                    Span::raw(" "),
                    Span::styled(prompt, Style::default().fg(Color::White)),
                    Span::styled(format!("{tail} "), Style::default().fg(Color::DarkGray)),
                ]));
            }
            (None, _) => block = block.title(Span::styled(" sd ", Style::default().fg(ACCENT))),
        }
        let inner = block.inner(area);
        f.render_widget(block, area);

        match (entry, self.shown.as_mut()) {
            (Some(_), Some(s)) => {
                let widget = StatefulImage::<StatefulProtocol>::default()
                    .resize(Resize::Scale(Some(FilterType::Lanczos3)));
                let font = self.picker.font_size();
                let area = centered(inner, s.dims, (font.width, font.height));
                f.render_stateful_widget(widget, area, &mut s.protocol);
            }
            (Some(e), None) => {
                let msg = format!(
                    "image not found: {}",
                    e.path.as_deref().unwrap_or(Path::new("?")).display()
                );
                center_text(f, inner, vec![Line::from(msg.dim())]);
            }
            (None, _) => {
                let hello = vec![
                    Line::from("type a prompt and press Enter".fg(ACCENT)),
                    Line::from(""),
                    Line::from(
                        format!(
                            "profile '{}': {}",
                            self.profile_name,
                            self.profile.summary()
                        )
                        .dim(),
                    ),
                    Line::from(
                        "/help for commands · /profiles to switch · /sessions to resume".dim(),
                    ),
                ];
                center_text(f, inner, hello);
            }
        }
    }

    fn draw_log(&self, f: &mut Frame, area: Rect) {
        let start = self.log.len().saturating_sub(area.height as usize);
        let lines: Vec<Line> = self.log[start..]
            .iter()
            .map(|(kind, s)| {
                let style = match kind {
                    LogKind::Info => Style::default(),
                    LogKind::Good => Style::default().fg(Color::Green),
                    LogKind::Bad => Style::default().fg(Color::Red),
                    LogKind::Dim => Style::default().fg(Color::DarkGray),
                };
                Line::from(Span::styled(s.clone(), style))
            })
            .collect();
        f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), pad(area));
    }

    fn draw_status(&self, f: &mut Frame, area: Rect) {
        let name = format!(
            " {}{} ",
            self.profile_name,
            if self.modified { "*" } else { "" }
        );
        let left = Line::from(vec![
            Span::styled(
                name,
                Style::default()
                    .fg(Color::Black)
                    .bg(ACCENT)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(" "),
            Span::styled(self.profile.summary(), Style::default().fg(Color::DarkGray)),
        ]);
        let right = match &self.running {
            Some(r) => {
                let spin = SPINNER[self.tick % SPINNER.len()];
                let prog = r
                    .progress
                    .map(|(d, t)| format!(" {d}/{t}"))
                    .unwrap_or_default();
                let queued = if self.queue.is_empty() {
                    String::new()
                } else {
                    format!(" · {} queued", self.queue.len())
                };
                format!(
                    "{spin} {}{prog} · {:.0}s{queued} · Esc cancels ",
                    ellipsize(&r.stage, 40),
                    r.started.elapsed().as_secs_f64()
                )
            }
            None => format!("session {} ", self.session.id),
        };
        f.render_widget(Paragraph::new(left), area);
        f.render_widget(
            Paragraph::new(Line::from(right.fg(if self.running.is_some() {
                ACCENT
            } else {
                Color::DarkGray
            })))
            .alignment(Alignment::Right),
            area,
        );
    }

    fn draw_input(&self, f: &mut Frame, area: Rect) {
        let is_cmd = self.input.starts_with('/');
        let title = if is_cmd { " command " } else { " prompt " };
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(if is_cmd { Color::Cyan } else { ACCENT }))
            .title(title);
        let inner = block.inner(area);
        f.render_widget(block, area);
        // Scroll horizontally so the cursor stays visible.
        let width = inner.width.saturating_sub(1) as usize;
        let skip = self.cursor.saturating_sub(width);
        let visible: String = self.input.chars().skip(skip).take(width + 1).collect();
        f.render_widget(Paragraph::new(visible), inner);
        f.set_cursor_position((inner.x + (self.cursor - skip) as u16, inner.y));
    }
}

enum Browse {
    Prev,
    Next,
    First,
    Last,
}

fn decode(path: &Path) -> Result<image::DynamicImage> {
    Ok(image::ImageReader::open(path)?
        .with_guessed_format()?
        .decode()?)
}

fn center_text(f: &mut Frame, inner: Rect, lines: Vec<Line>) {
    let h = lines.len() as u16;
    let y = inner.y + inner.height.saturating_sub(h) / 2;
    f.render_widget(
        Paragraph::new(lines).alignment(Alignment::Center),
        Rect {
            y,
            height: h.min(inner.height),
            ..inner
        },
    );
}

/// Largest area with the image's aspect ratio that fits `area`, centered.
fn centered(area: Rect, dims: (u32, u32), font: (u16, u16)) -> Rect {
    let (fw, fh) = (font.0.max(1) as f64, font.1.max(1) as f64);
    let (iw, ih) = (dims.0.max(1) as f64, dims.1.max(1) as f64);
    let (aw, ah) = (area.width as f64 * fw, area.height as f64 * fh);
    let scale = (aw / iw).min(ah / ih);
    let cols = ((iw * scale) / fw).floor().max(1.0) as u16;
    let rows = ((ih * scale) / fh).floor().max(1.0) as u16;
    Rect {
        x: area.x + (area.width.saturating_sub(cols)) / 2,
        y: area.y + (area.height.saturating_sub(rows)) / 2,
        width: cols.min(area.width),
        height: rows.min(area.height),
    }
}

fn pad(area: Rect) -> Rect {
    Rect {
        x: area.x + 1,
        width: area.width.saturating_sub(2),
        ..area
    }
}

/// Join a multi-line paste into one prompt line: newlines and tabs become single spaces.
fn flatten_paste(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn byte_index(s: &str, char_idx: usize) -> usize {
    s.char_indices().nth(char_idx).map_or(s.len(), |(i, _)| i)
}

fn ellipsize(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        format!(
            "{}…",
            s.chars().take(max.saturating_sub(1)).collect::<String>()
        )
    }
}

fn display_path(path: &Path, base: &Path) -> String {
    path.strip_prefix(base)
        .map(|p| format!("./{}", p.display()))
        .unwrap_or_else(|_| path.display().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn centered_keeps_aspect() {
        let r = centered(Rect::new(0, 0, 100, 50), (1536, 1536), (8, 16));
        // square image: cols*8 == rows*16
        assert_eq!(r.width * 8, r.height * 16);
        assert!(r.width <= 100 && r.height <= 50);
    }

    #[test]
    fn pastes_become_one_line() {
        assert_eq!(
            flatten_paste("1girl,\n  rain\r\n\tkyoto style\n"),
            "1girl, rain kyoto style"
        );
    }

    #[test]
    fn byte_index_handles_multibyte() {
        assert_eq!(byte_index("héllo", 2), 3);
        assert_eq!(byte_index("abc", 9), 3);
    }

    #[test]
    fn ellipsize_counts_chars() {
        assert_eq!(ellipsize("abcdef", 4), "abc…");
        assert_eq!(ellipsize("abc", 4), "abc");
    }
}
