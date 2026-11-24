//! Runs one prompt and lands the PNG in the output folder. Local profiles call sd-cli
//! (generate, then optionally upscale). Server profiles submit a job to sd-server's
//! /sdcpp/v1 API, poll it for progress, and decode the returned image.

use std::{
    io::Read,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc::Sender,
    },
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, anyhow, bail};
use base64::Engine;
use serde_json::{Value, json};

use crate::config::{Profile, Target};

#[derive(Debug)]
pub enum Event {
    /// What the generator is doing now (sd-cli's latest info line, or the server's stage).
    Stage(String),
    /// Progress within the current phase: done/total.
    Progress {
        done: u32,
        total: u32,
    },
    Done {
        path: PathBuf,
        elapsed: Duration,
        seed: i64,
    },
    Failed(String),
}

/// Handle to a running generation.
pub struct Job {
    child: Arc<Mutex<Option<Child>>>,
    cancelled: Arc<AtomicBool>,
}

impl Job {
    /// Stop the generation: kills a local sd-cli; server jobs get a cancel request.
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
        if let Some(child) = self.child.lock().unwrap().as_mut() {
            let _ = child.kill();
        }
    }
}

/// Start generating `prompt` with `profile` in the background; events arrive on `tx`.
pub fn start(profile: Profile, prompt: String, out_dir: PathBuf, tx: Sender<Event>) -> Job {
    let child = Arc::new(Mutex::new(None));
    let cancelled = Arc::new(AtomicBool::new(false));
    let job = Job {
        child: child.clone(),
        cancelled: cancelled.clone(),
    };
    thread::spawn(move || {
        let started = Instant::now();
        let seed = if profile.seed < 0 {
            random_seed()
        } else {
            profile.seed
        };
        let runner = Runner {
            child,
            cancelled,
            tx: tx.clone(),
        };
        let result = match profile.target {
            Target::Local => runner.local(&profile, &prompt, seed, &out_dir),
            Target::Server => runner.server(&profile, &prompt, seed, &out_dir),
        };
        let _ = tx.send(match result {
            Ok(path) => Event::Done {
                path,
                elapsed: started.elapsed(),
                seed,
            },
            Err(e) => {
                let msg = explain(format!("{e:#}"));
                if msg != "cancelled" {
                    crate::syslog::err(&format!("generation failed: {msg}"));
                }
                Event::Failed(msg)
            }
        });
    });
    job
}

/// Run one prompt to completion on the current thread, printing progress to stderr.
/// Used by the one-shot `sd "prompt"` mode.
pub fn run_blocking(profile: Profile, prompt: String, out_dir: PathBuf) -> Result<PathBuf> {
    let (tx, rx) = std::sync::mpsc::channel();
    let _job = start(profile, prompt, out_dir, tx);
    let mut last_stage = String::new();
    for event in rx {
        match event {
            Event::Stage(s) if s != last_stage => {
                eprintln!("  {s}");
                last_stage = s;
            }
            Event::Done {
                path,
                elapsed,
                seed,
            } => {
                eprintln!("done in {:.0}s (seed {seed})", elapsed.as_secs_f64());
                return Ok(path);
            }
            Event::Failed(e) => bail!(e),
            _ => {}
        }
    }
    bail!("generation thread ended unexpectedly")
}

// ---- local sd-cli ---------------------------------------------------------------------------

fn gen_args(p: &Profile, prompt: &str, seed: i64, out: &str) -> Vec<String> {
    let mut a: Vec<String> = Vec::new();
    for (flag, v) in [
        ("-m", &p.model),
        ("--diffusion-model", &p.diffusion_model),
        ("--llm", &p.llm),
        ("--vae", &p.vae),
        ("--tokenizer", &p.tokenizer),
        ("--lora-model-dir", &p.lora_dir),
    ] {
        if let Some(v) = v {
            a.push(flag.into());
            a.push(expand_home(v));
        }
    }
    a.extend(["-p".into(), prompt.into()]);
    if !p.negative.is_empty() {
        a.extend(["-n".into(), p.negative.clone()]);
    }
    a.extend([
        "-W".into(),
        p.width.to_string(),
        "-H".into(),
        p.height.to_string(),
        "--steps".into(),
        p.steps.to_string(),
        "--cfg-scale".into(),
        p.cfg.to_string(),
        "--sampling-method".into(),
        p.sampler.clone(),
        "--seed".into(),
        seed.to_string(),
    ]);
    if p.vae_tiling {
        a.push("--vae-tiling".into());
    }
    if let Some(mode) = &p.conv_direct {
        a.extend(["--conv-direct".into(), mode.clone()]);
    }
    a.extend(p.extra.iter().cloned());
    a.extend(["-o".into(), out.into()]);
    a
}

fn upscale_args(model: &str, input: &str, output: &str) -> Vec<String> {
    [
        "-M",
        "upscale",
        "--upscale-model",
        &expand_home(model),
        "-i",
        input,
        "-o",
        output,
    ]
    .map(String::from)
    .to_vec()
}

pub fn expand_home(p: &str) -> String {
    match (p.strip_prefix("~/"), directories::BaseDirs::new()) {
        (Some(rest), Some(dirs)) => dirs.home_dir().join(rest).to_string_lossy().into_owned(),
        _ => p.to_string(),
    }
}

fn random_seed() -> i64 {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let mixed = (nanos as u64) ^ (u64::from(std::process::id()) << 32);
    // splitmix64 step, then keep it positive and below 2^31 like the seeds people share
    let mut z = mixed.wrapping_add(0x9e37_79b9_7f4a_7c15);
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    ((z ^ (z >> 31)) & 0x7fff_ffff) as i64
}

/// `sd-YYYYmmdd-HHMMSS-sSEED.png` in `dir`, with a counter if it already exists.
fn output_path(dir: &Path, seed: i64) -> PathBuf {
    let stamp = chrono::Local::now().format("%Y%m%d-%H%M%S");
    let base = format!("sd-{stamp}-s{seed}");
    let mut path = dir.join(format!("{base}.png"));
    let mut n = 2;
    while path.exists() {
        path = dir.join(format!("{base}-{n}.png"));
        n += 1;
    }
    path
}

struct Runner {
    child: Arc<Mutex<Option<Child>>>,
    cancelled: Arc<AtomicBool>,
    tx: Sender<Event>,
}

impl Runner {
    fn local(&self, p: &Profile, prompt: &str, seed: i64, out_dir: &Path) -> Result<PathBuf> {
        let final_path = output_path(out_dir, seed);
        let final_str = final_path.to_string_lossy().into_owned();
        let sd_cli = expand_home(&p.sd_cli);
        match &p.upscale {
            None => self.run(Command::new(&sd_cli).args(gen_args(p, prompt, seed, &final_str)))?,
            Some(model) => {
                let draft =
                    std::env::temp_dir().join(format!("sd-tui-{}-{seed}.png", std::process::id()));
                let draft_str = draft.to_string_lossy().into_owned();
                let result = self
                    .run(Command::new(&sd_cli).args(gen_args(p, prompt, seed, &draft_str)))
                    .and_then(|_| {
                        let _ = self.tx.send(Event::Stage("upscaling x4".into()));
                        self.run(
                            Command::new(&sd_cli).args(upscale_args(model, &draft_str, &final_str)),
                        )
                    });
                let _ = std::fs::remove_file(&draft);
                result?;
            }
        }
        if !final_path.exists() {
            bail!("sd-cli finished but wrote no image");
        }
        Ok(final_path)
    }

    /// Run a command to completion, streaming its output as events. Errors on a non-zero exit
    /// (with sd-cli's error lines) or on cancellation.
    fn run(&self, cmd: &mut Command) -> Result<()> {
        if self.cancelled.load(Ordering::SeqCst) {
            bail!("cancelled");
        }
        let program = cmd.get_program().to_string_lossy().into_owned();
        // No stdin: the TUI owns the terminal and children must not read keystrokes.
        let mut child = cmd
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .with_context(|| format!("could not start {program}"))?;
        let errors = Arc::new(Mutex::new(Vec::<String>::new()));
        let readers: Vec<_> = [
            child
                .stdout
                .take()
                .map(|s| Box::new(s) as Box<dyn Read + Send>),
            child
                .stderr
                .take()
                .map(|s| Box::new(s) as Box<dyn Read + Send>),
        ]
        .into_iter()
        .flatten()
        .map(|stream| {
            let tx = self.tx.clone();
            let errors = errors.clone();
            let program = program.clone();
            thread::spawn(move || pump(stream, &program, &tx, &errors))
        })
        .collect();
        *self.child.lock().unwrap() = Some(child);
        let status = loop {
            if let Some(status) = self.child.lock().unwrap().as_mut().unwrap().try_wait()? {
                break status;
            }
            thread::sleep(Duration::from_millis(50));
        };
        for r in readers {
            let _ = r.join();
        }
        *self.child.lock().unwrap() = None;
        if self.cancelled.load(Ordering::SeqCst) {
            bail!("cancelled");
        }
        if !status.success() {
            let errors = errors.lock().unwrap();
            if errors.is_empty() {
                bail!("{program} exited with {status}");
            }
            bail!("{}", errors.join("; "));
        }
        Ok(())
    }

    // ---- sd-server ------------------------------------------------------------------------

    fn server(&self, p: &Profile, prompt: &str, seed: i64, out_dir: &Path) -> Result<PathBuf> {
        let base = p
            .url
            .as_deref()
            .context("profile has target = \"server\" but no url")?
            .trim_end_matches('/');
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .timeout_global(Some(Duration::from_secs(60)))
            .build()
            .into();
        let body = server_request(p, prompt, seed)?;

        let _ = self.tx.send(Event::Stage(format!("submitting to {base}")));
        let mut resp = agent
            .post(&format!("{base}/sdcpp/v1/img_gen"))
            .send_json(&body)
            .map_err(|e| anyhow!("can't reach sd-server at {base}: {e} (is it running?)"))?;
        let status = resp.status().as_u16();
        let submitted: Value = resp.body_mut().read_json().unwrap_or(Value::Null);
        if status != 202 && status != 200 {
            bail!(
                "sd-server refused the job (HTTP {status}): {}",
                api_error(&submitted)
            );
        }
        let poll = match submitted["poll_url"].as_str() {
            Some(u) => format!("{base}{u}"),
            None => format!(
                "{base}/sdcpp/v1/jobs/{}",
                submitted["id"].as_str().context("no job id in reply")?
            ),
        };
        let job_id = submitted["id"].as_str().unwrap_or_default().to_string();

        let mut last = (String::new(), None);
        loop {
            if self.cancelled.load(Ordering::SeqCst) {
                let _ = agent
                    .post(&format!("{base}/sdcpp/v1/jobs/{job_id}/cancel"))
                    .send_empty();
                bail!("cancelled");
            }
            let mut resp = agent
                .get(&poll)
                .call()
                .map_err(|e| anyhow!("lost sd-server at {base}: {e}"))?;
            let job: Value = resp
                .body_mut()
                .read_json()
                .context("unreadable job status from sd-server")?;
            match job["status"].as_str().unwrap_or("") {
                "completed" => {
                    let b64 = job["result"]["images"][0]["b64_json"]
                        .as_str()
                        .context("completed job has no image")?;
                    let png = base64::engine::general_purpose::STANDARD
                        .decode(b64)
                        .context("sd-server returned an image that isn't valid base64")?;
                    let path = output_path(out_dir, seed);
                    std::fs::write(&path, png)
                        .with_context(|| format!("writing {}", path.display()))?;
                    return Ok(path);
                }
                "failed" => bail!("{}", api_error(&job)),
                "cancelled" => bail!("cancelled on the server"),
                "queued" => {
                    let pos = job["queue_position"].as_u64().unwrap_or(0);
                    self.update(
                        &mut last,
                        format!("queued on server (position {pos})"),
                        None,
                    );
                }
                _ => {
                    let pr = &job["progress"];
                    let stage = pr["stage"].as_str().unwrap_or("working");
                    let detail = pr["detail"].as_str().unwrap_or("");
                    let label = if detail.is_empty() {
                        stage.replace('_', " ")
                    } else {
                        format!("{} ({})", stage.replace('_', " "), detail.replace('_', " "))
                    };
                    let counts = match (pr["completed"].as_u64(), pr["total"].as_u64()) {
                        (Some(d), Some(t)) => Some((d as u32, t as u32)),
                        _ => None,
                    };
                    self.update(&mut last, label, counts);
                }
            }
            thread::sleep(Duration::from_millis(400));
        }
    }

    /// Send stage/progress events only when they change.
    fn update(
        &self,
        last: &mut (String, Option<(u32, u32)>),
        stage: String,
        counts: Option<(u32, u32)>,
    ) {
        if last.0 != stage {
            let _ = self.tx.send(Event::Stage(stage.clone()));
            last.0 = stage;
            last.1 = None;
        }
        if counts != last.1 {
            if let Some((done, total)) = counts {
                let _ = self.tx.send(Event::Progress { done, total });
            }
            last.1 = counts;
        }
    }
}

/// The /sdcpp/v1/img_gen request body for a profile.
fn server_request(p: &Profile, prompt: &str, seed: i64) -> Result<Value> {
    let mut body = json!({
        "prompt": prompt,
        "negative_prompt": p.negative,
        "width": p.width,
        "height": p.height,
        "seed": seed,
        "batch_count": 1,
        "sample_params": {
            "sample_method": p.sampler,
            "sample_steps": p.steps,
            "guidance": { "txt_cfg": p.cfg },
        },
        "vae_tiling_params": { "enabled": p.vae_tiling },
        "preview": "none",
        "output_format": "png",
    });
    let slots = p.model_slots();
    if !slots.is_empty() || p.recipe.is_some() {
        let recipe = p.recipe.as_deref().context(
            "server profiles that pick model files also need `recipe` (e.g. recipe = \"anima\")",
        )?;
        let models: serde_json::Map<String, Value> = slots
            .into_iter()
            .map(|(slot, id)| (slot.to_string(), Value::from(id)))
            .collect();
        body["model"] = json!({ "recipe": recipe, "models": models });
    }
    if let Some(upscaler) = &p.upscale {
        body["hires"] = json!({
            "enabled": true,
            "upscaler": upscaler,
            "scale": p.hires_scale,
            "steps": 0,
            "denoising_strength": p.hires_denoise,
        });
    }
    Ok(body)
}

/// Add a hint to sd.cpp errors that are cryptic on their own.
fn explain(err: String) -> String {
    if err.contains("capacity exceeded") {
        // sd.cpp refuses to load weights when MemAvailable drops under 1 GiB; on unified-memory
        // machines the GPU copy of the model counts against system RAM too.
        format!(
            "{err} -- hint: not enough free RAM to load the model. Close other memory users \
             (another sd-server/sd-cli, browsers), or use a smaller quantized (GGUF) model."
        )
    } else {
        err
    }
}

/// Best human-readable message from an sd-server error reply.
fn api_error(v: &Value) -> String {
    if let Some(msg) = v["error"]["message"].as_str() {
        return msg.to_string();
    }
    match (v["error"].as_str(), v["code"].as_str()) {
        (Some(e), Some(c)) => format!("{e} ({c})"),
        (Some(e), None) => e.to_string(),
        _ if v.is_null() => "no details".to_string(),
        _ => v.to_string(),
    }
}

// ---- sd-cli output parsing --------------------------------------------------------------------

/// Read a child's output, splitting on \r as well as \n (sd-cli redraws progress bars with \r).
fn pump(
    mut stream: Box<dyn Read + Send>,
    program: &str,
    tx: &Sender<Event>,
    errors: &Mutex<Vec<String>>,
) {
    let mut buf = [0u8; 4096];
    let mut line = Vec::new();
    loop {
        let n = match stream.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => n,
        };
        for &b in &buf[..n] {
            if b == b'\n' || b == b'\r' {
                handle_line(&String::from_utf8_lossy(&line), program, tx, errors);
                line.clear();
            } else {
                line.push(b);
            }
        }
    }
    if !line.is_empty() {
        handle_line(&String::from_utf8_lossy(&line), program, tx, errors);
    }
}

/// Progress and info lines drive the status line. Warnings and errors go to the system log;
/// errors are also kept for the failure message if the command fails, which is when they
/// matter. (sd.cpp warnings are things like "conv2d direct auto: Vulkan0 -> off (probe output
/// check failed)": a faster kernel failed its self-test and the standard one is used.)
fn handle_line(raw: &str, program: &str, tx: &Sender<Event>, errors: &Mutex<Vec<String>>) {
    let line = strip_ansi(raw);
    let line = line.trim();
    if line.is_empty() {
        return;
    }
    let name = Path::new(program)
        .file_name()
        .map_or(program.into(), |n| n.to_string_lossy());
    if let Some((done, total)) = parse_progress(line) {
        let _ = tx.send(Event::Progress { done, total });
    } else if let Some(msg) = line.strip_prefix("[E]") {
        let msg = shorten(msg);
        crate::syslog::err(&format!("{name}: {msg}"));
        errors.lock().unwrap().push(msg);
    } else if let Some(msg) = line.strip_prefix("[W]") {
        crate::syslog::warning(&format!("{name}: {}", shorten(msg)));
    } else if let Some(msg) = line.strip_prefix("[I]") {
        let _ = tx.send(Event::Stage(shorten(msg)));
    } else if line.contains("rror") {
        crate::syslog::err(&format!("{name}: {line}"));
        errors.lock().unwrap().push(line.to_string());
    }
}

/// "loading tensors completed, taking 2.00s (...) --- model_loader.cpp:1451" -> without the
/// source location.
fn shorten(msg: &str) -> String {
    let msg = msg.trim();
    let msg = msg.rsplit_once(" --- ").map_or(msg, |(text, _)| text);
    msg.trim().to_string()
}

/// sd-cli progress bars look like `  |=====>     | 3/8 - 25.31s/it`.
fn parse_progress(line: &str) -> Option<(u32, u32)> {
    let after_bar = &line[line.rfind('|')? + 1..];
    let token = after_bar.split_whitespace().next()?;
    let (done, total) = token.split_once('/')?;
    Some((done.parse().ok()?, total.parse().ok()?))
}

fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            if chars.peek() == Some(&'[') {
                chars.next();
                for c in chars.by_ref() {
                    if c.is_ascii_alphabetic() {
                        break;
                    }
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, DEFAULT_CONFIG};

    fn profile(name: &str) -> Profile {
        toml::from_str::<Config>(DEFAULT_CONFIG)
            .unwrap()
            .profile(name)
            .unwrap()
    }

    #[test]
    fn progress_lines_parse() {
        assert_eq!(
            parse_progress("  |======>        | 3/8 -  25.31s/it - ETA 02:36"),
            Some((3, 8))
        );
        assert_eq!(
            parse_progress("|##########| 104/104 - 682.11MB/s"),
            Some((104, 104))
        );
        assert_eq!(parse_progress("[I] sampling completed"), None);
    }

    #[test]
    fn info_lines_lose_source_location() {
        assert_eq!(
            shorten(" sampling completed, taking 44.27s --- sampling.cpp:99"),
            "sampling completed, taking 44.27s"
        );
    }

    #[test]
    fn ansi_is_stripped() {
        assert_eq!(strip_ansi("\x1b[0mhello\x1b[K"), "hello");
    }

    #[test]
    fn local_args_expand_home_and_keep_prompt_whole() {
        let args = gen_args(&profile("quick"), "it's a cat; rm -rf /", 7, "/tmp/x.png");
        assert!(args.iter().any(|a| a == "it's a cat; rm -rf /"));
        assert!(!args.iter().any(|a| a.starts_with("~/")));
        let i = args.iter().position(|a| a == "--seed").unwrap();
        assert_eq!(args[i + 1], "7");
    }

    #[test]
    fn server_request_selects_recipe_and_models() {
        let body = server_request(&profile("remote"), "a cat", 5).unwrap();
        assert_eq!(body["model"]["recipe"], "anima");
        assert_eq!(
            body["model"]["models"]["llm"],
            "text_encoders/anima/qwen_3_06b_base.safetensors"
        );
        assert_eq!(body["sample_params"]["sample_steps"], 8);
        assert_eq!(body["seed"], 5);
        assert!(body.get("hires").is_none());
    }

    #[test]
    fn server_request_needs_recipe_with_models() {
        let mut p = profile("remote");
        p.recipe = None;
        assert!(server_request(&p, "x", 1).is_err());
        // no models and no recipe: use the server's current model
        p.diffusion_model = None;
        p.llm = None;
        p.vae = None;
        assert!(server_request(&p, "x", 1).unwrap().get("model").is_none());
    }

    #[test]
    fn server_upscale_becomes_hires() {
        let mut p = profile("remote");
        p.upscale = Some("RealESRGAN_x4plus_anime_6B".into());
        let body = server_request(&p, "x", 1).unwrap();
        assert_eq!(body["hires"]["enabled"], true);
        assert_eq!(body["hires"]["upscaler"], "RealESRGAN_x4plus_anime_6B");
    }

    #[test]
    fn api_errors_read_both_shapes() {
        assert_eq!(
            api_error(&json!({"error": {"code": "x", "message": "boom"}})),
            "boom"
        );
        assert_eq!(
            api_error(
                &json!({"error": "No current model is configured", "code": "model_not_loaded"})
            ),
            "No current model is configured (model_not_loaded)"
        );
    }

    #[test]
    fn capacity_errors_get_a_hint() {
        assert!(explain("RAM weight capacity exceeded. Requested 0 bytes".into()).contains("hint"));
        assert_eq!(explain("boom".into()), "boom");
    }

    #[test]
    fn seeds_are_positive_31_bit() {
        for _ in 0..100 {
            assert!((0..=0x7fff_ffff).contains(&random_seed()));
        }
    }
}
