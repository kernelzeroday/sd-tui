//! The full-screen interface: the session's image on top (←/→ to browse, the prompt that made
//! it on the frame's bottom edge), a status line (profile, progress, short notices), and a prompt
//! box that grows as the prompt gets longer. Every line typed is an image request; lines
//! starting with "/" are commands. Longer output (/help, /show) opens a scrollable panel.
//! Diagnostics go to the system log (`journalctl -t sd`), not the screen.

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
    history,
    menu::{self, Action, Menu},
    prompts,
    runner::{self, Event, Job},
    session::{self, Entry, Session},
    syslog,
};

const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
const ACCENT: Color = Color::Rgb(255, 160, 200);

const COMMANDS: &[(&str, &str)] = &[
    ("/help", "show commands and settings"),
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
        "/file",
        "PATH: load a prompt from a text file into the input (lines are joined)",
    ),
    (
        "/batch",
        "PATH: queue one image per line of a text file (# lines skipped)",
    ),
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
    ("/resume", "[ID]: switch to another session"),
    ("/new", "start a new session"),
    ("/config", "show the config and session file paths"),
    ("/quit", "exit (also Ctrl+C twice)"),
];

const SET_HELP: &str = "Every setting is also a command: /KEY VALUE sets it, a bare /KEY opens a menu or \
shows it. /set KEY=VALUE ... changes several at once. Settings: size=WxH aspect=W:H width height \
steps cfg sampler seed=N|random negative=\"...\" upscale=PATH|NAME|off vae_tiling=on|off \
conv_direct=auto|on|off model diffusion_model llm vae tokenizer lora_dir sd_cli \
target=local|server url recipe hires_scale hires_denoise extra=\"--flag value\"";

const KEYS: &[(&str, &str)] = &[
    ("Enter", "generate (or run a /command)"),
    (
        "←/→",
        "browse images when the prompt is empty; otherwise move the cursor",
    ),
    ("Home/End", "first/last image when the prompt is empty"),
    (
        "↑/↓",
        "move between the prompt's lines, then prompt history",
    ),
    (
        "Esc",
        "cancel the running image and the queue; otherwise clear the input",
    ),
    (
        "Tab",
        "complete a /command, or a file path (/file, /batch, /load, ...)",
    ),
    ("Ctrl+U", "clear the input"),
    (
        "Ctrl+C",
        "clear the input; press twice quickly to quit (also Ctrl+D on an empty prompt)",
    ),
];

/// Commands whose argument is a file path, so Tab completes paths.
const PATH_COMMANDS: &[&str] = &[
    "/file",
    "/batch",
    "/load",
    "/upscale",
    "/model",
    "/diffusion_model",
    "/llm",
    "/vae",
    "/tokenizer",
    "/lora_dir",
    "/sd_cli",
];

/// How long a non-error notice stays on the status line.
const NOTICE_SECS: u64 = 6;

/// Ctrl+C / Ctrl+D must be pressed twice within this long to quit.
const QUIT_WINDOW: Duration = Duration::from_millis(1500);

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

#[derive(Clone, Copy, PartialEq)]
enum Tone {
    Info,
    Good,
    Bad,
}

/// One-line message on the status line. Errors stay until Esc or the next submit.
struct Notice {
    tone: Tone,
    text: String,
    at: Instant,
    /// How long it stays up (errors: until dismissed).
    ttl: Duration,
}

/// Scrollable text that temporarily replaces the image (/help, /show, /config).
struct Panel {
    title: String,
    lines: Vec<Line<'static>>,
    scroll: u16,
    /// Rows the panel had when last drawn, to keep scrolling in range.
    rows: u16,
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

    /// Width of the prompt box's text area when last drawn (for ↑/↓ across wrapped lines).
    input_width: usize,
    /// Text rows the prompt box currently has. It grows with two rows to spare and only
    /// shrinks back once the prompt is a single row (or much shorter), so typing across a
    /// line break doesn't resize — and redraw — the image every time.
    input_box_rows: usize,
    /// Set once a prompt has taken more than three rows; from then on the box keeps its
    /// size for the rest of the session instead of shrinking back.
    input_box_sticky: bool,
    /// Where typed prompts and commands are saved for later sessions (None if unknown).
    history_file: Option<PathBuf>,

    notice: Option<Notice>,
    panel: Option<Panel>,
    running: Option<Running>,
    queue: VecDeque<Request>,
    tick: usize,
    quit: bool,
    /// The first Ctrl+C / Ctrl+D of a possible double press, and when it came.
    quit_armed: Option<(char, Instant)>,
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
        input_width: 0,
        input_box_rows: 1,
        input_box_sticky: false,
        history_file: history::file().ok(),
        notice: None,
        panel: None,
        running: None,
        queue: VecDeque::new(),
        tick: 0,
        quit: false,
        quit_armed: None,
        menu: None,
    };
    syslog::debug(&format!(
        "started: profile '{}', images to {}, display {:?}",
        app.profile_name,
        app.out_dir.display(),
        app.picker.protocol_type()
    ));
    app.adopt_session();
    let result = app.main_loop(&mut terminal);
    let _ = crossterm::execute!(std::io::stdout(), crossterm::event::DisableBracketedPaste);
    ratatui::restore();
    if !app.session.entries.is_empty() {
        println!("resume this session with: sd --resume {}", app.session.id);
    }
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
        // Everything typed in earlier sessions, then any of this session's prompts that
        // aren't in it (a session from before the shared history existed).
        self.history = match &self.history_file {
            Some(path) => history::load(path).unwrap_or_else(|e| {
                syslog::err(&format!("couldn't read the prompt history: {e:#}"));
                Vec::new()
            }),
            None => Vec::new(),
        };
        for e in &self.session.entries {
            if !self.history.contains(&e.prompt) {
                self.history.push(e.prompt.clone());
            }
        }
        self.history.dedup();
        self.input_box_sticky = false;
        self.history_pos = None;
        let n = self.session.images().len();
        self.view = n.checked_sub(1);
        self.shown = None;
        self.unreadable = None;
        if !self.session.entries.is_empty() {
            self.info(format!(
                "resumed session {} · {n} images from {} requests",
                self.session.id,
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
                // The new image, its name and its time are on screen; nothing else to say.
                syslog::info(&format!(
                    "saved {} ({:.0}s, seed {seed})",
                    path.display(),
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
        if self.panel.is_some() && self.panel_key(key) {
            return;
        }
        let empty = self.input.is_empty();
        match key.code {
            KeyCode::Char('c') if ctrl => {
                // Like a shell prompt: the first Ctrl+C clears what you typed.
                self.input.clear();
                self.cursor = 0;
                self.history_pos = None;
                self.quit_key('c');
            }
            KeyCode::Char('d') if ctrl && empty => self.quit_key('d'),
            KeyCode::Char('u') if ctrl => {
                self.input.clear();
                self.cursor = 0;
            }
            KeyCode::Esc => self.on_escape(),
            KeyCode::Enter => self.submit(),
            KeyCode::Tab => self.complete(),
            KeyCode::Up => {
                if !self.move_row(-1) {
                    self.history_step(-1)
                }
            }
            KeyCode::Down => {
                if !self.move_row(1) {
                    self.history_step(1)
                }
            }
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

    /// Keys for the open panel. Returns false for keys the panel doesn't use: those close it
    /// and then do what they normally do, so typing a prompt just works.
    fn panel_key(&mut self, key: KeyEvent) -> bool {
        let Some(p) = self.panel.as_mut() else {
            return false;
        };
        let max = (p.lines.len() as u16).saturating_sub(p.rows.max(1));
        let page = p.rows.saturating_sub(1).max(1);
        match key.code {
            KeyCode::Up => p.scroll = p.scroll.saturating_sub(1),
            KeyCode::Down => p.scroll = (p.scroll + 1).min(max),
            KeyCode::PageUp => p.scroll = p.scroll.saturating_sub(page),
            KeyCode::PageDown | KeyCode::Char(' ') => p.scroll = (p.scroll + page).min(max),
            KeyCode::Home => p.scroll = 0,
            KeyCode::End => p.scroll = max,
            KeyCode::Esc | KeyCode::Enter => self.panel = None,
            _ => {
                self.panel = None;
                return false;
            }
        }
        true
    }

    fn menu_key(&mut self, key: KeyEvent, ctrl: bool) {
        let Some(m) = self.menu.as_mut() else { return };
        match key.code {
            KeyCode::Char('c') if ctrl => {
                self.menu = None;
                self.quit_key('c');
            }
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
                    Action::File => format!("/file {value}"),
                    Action::Batch => format!("/batch {value}"),
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
            self.notice = None;
            self.input.clear();
            self.cursor = 0;
        }
    }

    fn submit(&mut self) {
        let text = self.input.trim().to_string();
        self.input.clear();
        self.cursor = 0;
        self.history_pos = None;
        if self.notice.as_ref().is_some_and(|n| n.tone == Tone::Bad) {
            self.notice = None;
        }
        if text.is_empty() {
            return;
        }
        if self.history.last() != Some(&text) {
            self.history.push(text.clone());
            if let Some(path) = &self.history_file
                && let Err(e) = history::append(path, &text)
            {
                syslog::err(&format!("couldn't save the prompt history: {e:#}"));
            }
        }
        if text.starts_with('/') {
            if let Err(e) = self.command(&text) {
                self.bad(format!("{e:#}"));
            }
        } else {
            // The status line shows the queue length; no message needed.
            self.enqueue(text);
        }
    }

    fn enqueue(&mut self, prompt: String) {
        self.queue.push_back(Request {
            prompt,
            profile_name: self.profile_name.clone(),
            profile: self.profile.clone(),
        });
    }

    fn complete(&mut self) {
        if !self.input.starts_with('/') {
            return;
        }
        if let Some((cmd, arg)) = self.input.split_once(' ') {
            if PATH_COMMANDS.contains(&cmd) && self.cursor == self.input.chars().count() {
                let (cmd, arg) = (cmd.to_string(), arg.trim_start().to_string());
                self.complete_path_arg(&cmd, &arg);
            }
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
            _ => {
                // Several matches: the command menu, filtered by what was typed.
                let mut m = menu::commands(COMMANDS);
                self.input.chars().for_each(|ch| m.push(ch));
                self.menu = Some(m);
                self.input.clear();
                self.cursor = 0;
            }
        }
    }

    /// Shell-style Tab for a path argument: finish the name when it's unique, otherwise extend
    /// to the longest shared prefix, otherwise list the candidates on the status line.
    fn complete_path_arg(&mut self, cmd: &str, arg: &str) {
        let candidates = path_candidates(arg);
        let shared = common_prefix(&candidates);
        if shared.chars().count() > arg.chars().count() {
            self.input = format!("{cmd} {shared}");
            self.cursor = self.input.chars().count();
        } else if candidates.len() > 1 {
            let names: Vec<&str> = candidates
                .iter()
                .map(|c| {
                    let t = c.trim_end_matches('/');
                    let start = t.rfind('/').map_or(0, |i| i + 1);
                    &c[start..]
                })
                .collect();
            self.info(names.join("  "));
        } else if candidates.is_empty() {
            self.info(format!("no files match {arg}"));
        }
    }

    /// Move the cursor to the wrapped line above/below, keeping its column. False when there's
    /// no such line (then ↑/↓ step through history instead).
    fn move_row(&mut self, dir: i32) -> bool {
        let len = self.input.chars().count();
        let starts = wrap_starts(&self.input, self.input_width);
        let (row, col) = cursor_row_col(&starts, self.cursor, self.input_width);
        let target = row as i64 + dir as i64;
        if target < 0 || target as usize >= starts.len() {
            return false;
        }
        let t = target as usize;
        let row_end = starts
            .get(t + 1)
            .map_or(len, |&next| next.saturating_sub(1));
        self.cursor = (starts[t] + col).min(row_end.max(starts[t]));
        true
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
            "/help" | "/h" | "/?" => self.panel = Some(Panel::new("help", help_lines())),
            "/profiles" | "/ls" => {
                self.menu = Some(menu::profiles(&self.config, &self.profile_name))
            }
            "/" => self.menu = Some(menu::commands(COMMANDS)),
            "/profile" | "/p" => {
                if rest.is_empty() {
                    self.menu = Some(menu::profiles(&self.config, &self.profile_name));
                } else {
                    self.profile = self.config.profile(rest)?;
                    self.profile_name = rest.to_string();
                    self.modified = false;
                    self.good(format!("profile '{rest}'"));
                }
            }
            "/show" => {
                let text = toml::to_string_pretty(&self.profile).context("formatting profile")?;
                let mut lines = vec![Line::from(
                    format!(
                        "[profiles.{}]{}",
                        self.profile_name,
                        if self.modified {
                            "  # modified, /save to keep"
                        } else {
                            ""
                        }
                    )
                    .fg(ACCENT),
                )];
                lines.extend(text.lines().map(|l| Line::from(l.to_string())));
                self.panel = Some(Panel::new("/show", lines));
            }
            "/set" => {
                if rest.is_empty() {
                    self.panel = Some(Panel::new("/set", vec![Line::from(SET_HELP)]));
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
                self.good(changed.join(", "));
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
            }
            "/sessions" => return self.command("/resume"),
            "/file" | "/batch" => {
                let batch = cmd == "/batch";
                if rest.is_empty() {
                    let here = std::env::current_dir().unwrap_or_else(|_| self.out_dir.clone());
                    let files = prompts::files_in(&here);
                    anyhow::ensure!(
                        !files.is_empty(),
                        "usage: {cmd} PATH (no .txt files in {})",
                        here.display()
                    );
                    self.menu = Some(menu::prompt_files(files, batch));
                    return Ok(());
                }
                let text = prompts::read(rest)?;
                if batch {
                    let list = prompts::batch(&text);
                    anyhow::ensure!(!list.is_empty(), "{rest} has no prompts in it");
                    let n = list.len();
                    list.into_iter().for_each(|p| self.enqueue(p));
                    self.good(format!("queued {n} prompts from {rest}"));
                } else {
                    let prompt = prompts::one(&text);
                    anyhow::ensure!(!prompt.is_empty(), "{rest} has no prompt in it");
                    self.cursor = prompt.chars().count();
                    self.input = prompt;
                    self.info(format!("loaded {rest} · Enter generates"));
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
                let row = |k: &str, v: String| {
                    Line::from(vec![
                        Span::styled(format!("{k:<9}"), Style::default().fg(ACCENT)),
                        Span::raw(v),
                    ])
                };
                self.panel = Some(Panel::new(
                    "/config",
                    vec![
                        row("config", self.config_path.display().to_string()),
                        row("session", self.session.file.display().to_string()),
                        row("images", self.out_dir.display().to_string()),
                        row("display", format!("{:?}", self.picker.protocol_type())),
                        row("logs", "journalctl -t sd".into()),
                    ],
                ));
            }
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
                    self.good(changed);
                }
            }
        }
        Ok(())
    }

    /// Quit on the second Ctrl+C (or Ctrl+D) in quick succession; the first one only says so.
    fn quit_key(&mut self, ch: char) {
        if self
            .quit_armed
            .is_some_and(|(prev, at)| prev == ch && at.elapsed() < QUIT_WINDOW)
        {
            self.quit = true;
            return;
        }
        self.quit_armed = Some((ch, Instant::now()));
        let what = if self.running.is_some() {
            " (cancels the running image)"
        } else {
            ""
        };
        let key = ch.to_ascii_uppercase();
        self.notice = Some(Notice {
            tone: Tone::Info,
            text: format!("press Ctrl+{key} again to quit{what}"),
            at: Instant::now(),
            ttl: QUIT_WINDOW,
        });
    }

    // ---- notices --------------------------------------------------------------------

    fn notify(&mut self, tone: Tone, text: String) {
        if tone == Tone::Bad {
            syslog::err(&text);
        }
        self.notice = Some(Notice {
            tone,
            text,
            at: Instant::now(),
            ttl: if tone == Tone::Bad {
                Duration::MAX
            } else {
                Duration::from_secs(NOTICE_SECS)
            },
        });
    }
    fn info(&mut self, s: String) {
        self.notify(Tone::Info, s);
    }
    fn good(&mut self, s: String) {
        self.notify(Tone::Good, s);
    }
    fn bad(&mut self, s: String) {
        self.notify(Tone::Bad, s);
    }

    /// The notice to show now, if it hasn't expired.
    fn live_notice(&self) -> Option<&Notice> {
        self.notice.as_ref().filter(|n| n.at.elapsed() < n.ttl)
    }

    // ---- drawing ----------------------------------------------------------------------

    fn draw(&mut self, f: &mut Frame) {
        let area = f.area();
        // The prompt box grows with the prompt, up to a third of the screen.
        self.input_width = area.width.saturating_sub(2) as usize;
        let rows = input_rows(&self.input, self.cursor, self.input_width);
        let max_rows = (area.height / 3).max(1) as usize;
        self.input_box_sticky |= rows > STICKY_ROWS;
        self.input_box_rows = box_rows(rows, self.input_box_rows, self.input_box_sticky);
        let input_height = self.input_box_rows.clamp(1, max_rows) as u16 + 2;
        let [image_area, status_area, input_area] = Layout::vertical([
            Constraint::Min(4),
            Constraint::Length(1),
            Constraint::Length(input_height),
        ])
        .areas(area);

        self.draw_image(f, image_area);
        self.draw_status(f, status_area);
        self.draw_input(f, input_area);
    }

    fn draw_panel(&mut self, f: &mut Frame, area: Rect) {
        let Some(p) = self.panel.as_mut() else { return };
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(Color::Cyan))
            .title(Span::styled(
                format!(" {} ", p.title),
                Style::default().fg(Color::Cyan),
            ))
            .title_bottom(Line::from(" ↑/↓ PgUp/PgDn scroll · Esc close ".dim()));
        let inner = pad(block.inner(area));
        f.render_widget(block, area);
        p.rows = inner.height;
        f.render_widget(
            Paragraph::new(p.lines.clone())
                .wrap(Wrap { trim: false })
                .scroll((p.scroll, 0)),
            inner,
        );
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
        if self.panel.is_some() {
            self.draw_panel(f, area);
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
                // The prompt that made this image, then its seed, profile and time.
                let width = area.width.saturating_sub(6) as usize;
                let secs = e
                    .elapsed_s
                    .map(|s| format!(" · {s:.0}s"))
                    .unwrap_or_default();
                let tail = format!(
                    " · seed {} · {}{secs}",
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
                    Line::from("/ for commands · /file or /batch to load prompts · /help".dim()),
                ];
                center_text(f, inner, hello);
            }
        }
    }

    fn draw_status(&self, f: &mut Frame, area: Rect) {
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
            None => String::new(),
        };
        let right_width = right.chars().count() as u16;
        let left_area = Rect {
            width: area.width.saturating_sub(right_width + 1),
            ..area
        };
        let name = format!(
            " {}{} ",
            self.profile_name,
            if self.modified { "*" } else { "" }
        );
        let room = (left_area.width as usize).saturating_sub(name.chars().count() + 1);
        // A notice takes the place of the settings summary while it's fresh.
        let detail = match self.live_notice() {
            Some(n) => Span::styled(
                ellipsize(&n.text, room),
                Style::default().fg(match n.tone {
                    Tone::Info => Color::White,
                    Tone::Good => Color::Green,
                    Tone::Bad => Color::Red,
                }),
            ),
            None => Span::styled(
                ellipsize(&self.profile.summary(), room),
                Style::default().fg(Color::DarkGray),
            ),
        };
        let left = Line::from(vec![
            Span::styled(
                name,
                Style::default()
                    .fg(Color::Black)
                    .bg(ACCENT)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(" "),
            detail,
        ]);
        f.render_widget(Paragraph::new(left), left_area);
        f.render_widget(
            Paragraph::new(Line::from(right.fg(ACCENT))).alignment(Alignment::Right),
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
        let width = inner.width as usize;
        let chars: Vec<char> = self.input.chars().collect();
        let starts = wrap_starts(&self.input, width);
        let (row, col) = cursor_row_col(&starts, self.cursor, width);
        // Scroll so the cursor's line is visible when the prompt outgrows the box.
        let height = inner.height as usize;
        let top = row.saturating_sub(height.saturating_sub(1));
        let lines: Vec<Line> = (top..starts.len())
            .take(height)
            .map(|i| {
                let end = starts.get(i + 1).copied().unwrap_or(chars.len());
                Line::from(chars[starts[i]..end].iter().collect::<String>())
            })
            .collect();
        f.render_widget(Paragraph::new(lines), inner);
        f.set_cursor_position((inner.x + col as u16, inner.y + (row - top) as u16));
    }
}

impl Panel {
    fn new(title: &str, lines: Vec<Line<'static>>) -> Panel {
        Panel {
            title: title.to_string(),
            lines,
            scroll: 0,
            rows: 0,
        }
    }
}

fn help_lines() -> Vec<Line<'static>> {
    let heading = |s: &'static str| Line::from(s.fg(Color::Cyan).bold());
    let row = |k: &str, v: &str, w: usize| {
        Line::from(vec![
            Span::styled(format!("  {k:<w$} "), Style::default().fg(ACCENT)),
            Span::raw(v.to_string()),
        ])
    };
    let mut lines = vec![
        heading("Commands"),
        Line::from("  a bare command opens a menu of values".dim()),
    ];
    lines.extend(COMMANDS.iter().map(|(c, d)| row(c, d, 9)));
    lines.push(Line::from(""));
    lines.push(heading("Keys"));
    lines.extend(KEYS.iter().map(|(k, d)| row(k, d, 9)));
    lines.push(Line::from(""));
    lines.push(heading("Settings"));
    lines.push(Line::from(format!("  {SET_HELP}")));
    lines.push(Line::from(""));
    lines.push(heading("Prompt files"));
    lines.push(Line::from(
        "  /file reads one prompt (lines joined); /batch queues one image per line. Blank lines and # lines are skipped. From the shell: sd -f FILE, sd -b FILE.",
    ));
    lines.push(Line::from(""));
    lines.push(Line::from(
        "  Warnings and errors from sd-cli are in the system log: journalctl -t sd".dim(),
    ));
    lines
}

/// Paths that `partial` could complete to, as typed (a leading `~/` is kept). Directories end
/// in `/`. Hidden files only show up once the name being typed starts with a dot.
fn path_candidates(partial: &str) -> Vec<String> {
    let partial = if partial == "~" { "~/" } else { partial };
    let (dir, prefix) = match partial.rfind('/') {
        Some(i) => (&partial[..=i], &partial[i + 1..]),
        None => ("", partial),
    };
    let read_from = if dir.is_empty() {
        ".".to_string()
    } else {
        runner::expand_home(dir)
    };
    let Ok(entries) = std::fs::read_dir(&read_from) else {
        return Vec::new();
    };
    let mut out: Vec<String> = entries
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            if !name.starts_with(prefix) || (name.starts_with('.') && !prefix.starts_with('.')) {
                return None;
            }
            // Follow symlinks so a linked folder completes like a folder.
            let is_dir = std::fs::metadata(e.path()).is_ok_and(|m| m.is_dir());
            Some(format!("{dir}{name}{}", if is_dir { "/" } else { "" }))
        })
        .collect();
    out.sort();
    out
}

fn common_prefix(items: &[String]) -> String {
    let Some(first) = items.first() else {
        return String::new();
    };
    let mut len = first.len();
    for s in &items[1..] {
        len = first
            .char_indices()
            .zip(s.chars())
            .take_while(|((_, a), b)| a == b)
            .last()
            .map_or(0, |((i, a), _)| i + a.len_utf8())
            .min(len);
    }
    first[..len].to_string()
}

/// Where each line of `text` starts (in chars) when word-wrapped to `width` columns. Lines
/// break after a space when there is one; a word longer than the line is split.
fn wrap_starts(text: &str, width: usize) -> Vec<usize> {
    let chars: Vec<char> = text.chars().collect();
    let mut starts = vec![0];
    if width == 0 {
        return starts;
    }
    let mut start = 0;
    while chars.len() - start > width {
        let limit = start + width;
        let brk = (start + 1..limit)
            .rev()
            .find(|&i| chars[i] == ' ')
            .map_or(limit, |i| i + 1);
        starts.push(brk);
        start = brk;
    }
    starts
}

/// The cursor's wrapped line and column. A cursor just past a full last line goes to the
/// start of an extra line below it.
fn cursor_row_col(starts: &[usize], cursor: usize, width: usize) -> (usize, usize) {
    let row = starts.partition_point(|&s| s <= cursor).saturating_sub(1);
    let col = cursor - starts[row];
    if width > 0 && col >= width {
        (row + 1, col - width)
    } else {
        (row, col)
    }
}

/// Lines the prompt box needs for `text` with the cursor at `cursor`.
fn input_rows(text: &str, cursor: usize, width: usize) -> usize {
    let starts = wrap_starts(text, width);
    let (row, _) = cursor_row_col(&starts, cursor, width);
    starts.len().max(row + 1)
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

/// A prompt taller than this makes the prompt box keep its size for the session.
const STICKY_ROWS: usize = 3;

/// Prompt box height (in text rows) for a prompt of `rows` rows, given the current height.
/// Up to two rows the box fits the prompt exactly. From three rows it grows with two spare
/// rows, so the next lines don't resize it. A sticky box never shrinks; otherwise it shrinks
/// to fit a prompt of one or two rows, or when the prompt has dropped well below the box
/// (more than double the headroom spare).
fn box_rows(rows: usize, current: usize, sticky: bool) -> usize {
    const HEADROOM: usize = 2;
    let rows = rows.max(1);
    if sticky {
        if rows > current {
            rows + HEADROOM
        } else {
            current
        }
    } else if rows <= 2 {
        rows
    } else if rows > current || rows + 2 * HEADROOM < current {
        rows + HEADROOM
    } else {
        current
    }
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
    fn box_rows_has_headroom() {
        assert_eq!(box_rows(1, 1, false), 1);
        assert_eq!(box_rows(2, 1, false), 2); // two rows fit exactly, no spare rows
        assert_eq!(box_rows(3, 2, false), 5); // from three rows: grow with two spare rows
        assert_eq!(box_rows(5, 5, false), 5); // fills the spare rows without resizing
        assert_eq!(box_rows(6, 5, false), 8);
        assert_eq!(box_rows(4, 8, false), 8); // small deletions keep the size
        assert_eq!(box_rows(3, 8, false), 5); // a big drop shrinks
        assert_eq!(box_rows(2, 8, false), 2); // two rows or fewer fit exactly
        assert_eq!(box_rows(1, 8, false), 1);
        // After a prompt taller than three rows, the box only grows.
        assert_eq!(box_rows(1, 6, true), 6);
        assert_eq!(box_rows(7, 6, true), 9);
    }

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
    fn long_prompts_wrap_at_spaces() {
        // "aaaa bbbb cccc" in 6 columns: "aaaa ", "bbbb ", "cccc"
        assert_eq!(wrap_starts("aaaa bbbb cccc", 6), vec![0, 5, 10]);
        // a word longer than the line is split
        assert_eq!(wrap_starts("abcdefghij", 4), vec![0, 4, 8]);
        assert_eq!(wrap_starts("short", 20), vec![0]);
    }

    #[test]
    fn cursor_moves_to_a_new_line_after_a_full_one() {
        let starts = wrap_starts("abcd", 4);
        assert_eq!(cursor_row_col(&starts, 4, 4), (1, 0));
        assert_eq!(input_rows("abcd", 4, 4), 2);
        assert_eq!(input_rows("abcd", 2, 4), 1);
        let starts = wrap_starts("aaaa bbbb cccc", 6);
        assert_eq!(cursor_row_col(&starts, 7, 6), (1, 2));
    }

    #[test]
    fn paths_complete_like_a_shell() {
        let dir = std::env::temp_dir().join(format!("sd-tui-complete-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("Documents")).unwrap();
        std::fs::create_dir_all(dir.join("Downloads")).unwrap();
        std::fs::write(dir.join("prompts.txt"), "x").unwrap();
        std::fs::write(dir.join(".hidden"), "x").unwrap();
        let base = format!("{}/", dir.display());
        assert_eq!(
            path_candidates(&format!("{base}Doc")),
            vec![format!("{base}Documents/")]
        );
        let d = path_candidates(&format!("{base}D"));
        assert_eq!(d.len(), 2);
        assert_eq!(common_prefix(&d), format!("{base}Do"));
        assert_eq!(path_candidates(&base).len(), 3, "dotfiles hidden");
        assert_eq!(path_candidates(&format!("{base}.h")).len(), 1);
        assert_eq!(
            path_candidates("~").first().map(|s| s.starts_with("~/")),
            Some(true)
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn ellipsize_counts_chars() {
        assert_eq!(ellipsize("abcdef", 4), "abc…");
        assert_eq!(ellipsize("abc", 4), "abc");
    }
}
