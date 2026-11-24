//! Profiles: named generation setups (models, size, sampling, optional upscale, and where to
//! run: local sd-cli or a remote sd-server over its web API). Stored as TOML in
//! ~/.config/sd-tui/config.toml; a default file is written on first run.

use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    #[serde(default = "default_profile_name")]
    pub default_profile: String,
    #[serde(default)]
    pub display: Display,
    #[serde(default)]
    pub profiles: BTreeMap<String, Profile>,
}

fn default_profile_name() -> String {
    "quick".into()
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Display {
    /// auto | kitty | sixel | iterm2 | halfblocks
    #[serde(default = "auto")]
    pub protocol: String,
}

fn auto() -> String {
    "auto".into()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Target {
    /// Run sd-cli on this machine.
    Local,
    /// Send the request to an sd-server's /sdcpp/v1 API.
    Server,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Profile {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub target: Target,
    /// sd-server base URL when target = "server", e.g. "http://localhost:7860".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// sd-server recipe id (e.g. "anima"); needed when a server profile picks model files.
    /// Without it (and without model fields) the server's current model is used.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recipe: Option<String>,
    /// sd-cli binary for local profiles; "~/" is expanded.
    #[serde(default = "default_sd_cli")]
    pub sd_cli: String,

    // Model components. Local: file paths. Server: file ids from the server's model roots
    // (paths relative to the first --model-dir, e.g. "diffusion_models/anima/x.safetensors").
    /// Single-file checkpoint (-m / slot "model"), for SD1.x/SDXL.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diffusion_model: Option<String>,
    /// Text encoder (--llm / slot "llm"; Anima, Qwen-Image, MOSS).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub llm: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vae: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tokenizer: Option<String>,
    /// Local only: LoRA folder for <lora:NAME:W> prompt tags.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lora_dir: Option<String>,

    pub width: u32,
    pub height: u32,
    pub steps: u32,
    pub cfg: f32,
    #[serde(default = "default_sampler")]
    pub sampler: String,
    /// -1 = random (a fresh seed is picked per image and put in the file name).
    #[serde(default = "random_seed")]
    pub seed: i64,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub negative: String,
    #[serde(default)]
    pub vae_tiling: bool,
    /// Local: ESRGAN model file, run after generation (plain x4 upscale).
    /// Server: hires upscaler name from the server's --hires-upscalers-dir (file name without
    /// extension); runs a hires pass (upscale, then a light redraw at the larger size).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upscale: Option<String>,
    /// Server hires pass: scale factor and denoising strength.
    #[serde(default = "default_hires_scale")]
    pub hires_scale: f32,
    #[serde(default = "default_hires_denoise")]
    pub hires_denoise: f32,
    /// Local only: sd.cpp's direct-convolution kernels, `auto` (probe the GPU on every run,
    /// the default), `on` or `off`. Set `off` where the probe always fails, to skip it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub conv_direct: Option<String>,
    /// Local only: extra sd-cli arguments appended to the generation command.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub extra: Vec<String>,
}

fn default_sd_cli() -> String {
    "sd-cli".into()
}
fn default_sampler() -> String {
    "euler".into()
}
fn random_seed() -> i64 {
    -1
}
fn default_hires_scale() -> f32 {
    4.0
}
fn default_hires_denoise() -> f32 {
    0.35
}

impl Profile {
    /// One-line summary for the status bar.
    pub fn summary(&self) -> String {
        let place = match self.target {
            Target::Local => "local".to_string(),
            Target::Server => {
                let url = self.url.as_deref().unwrap_or("?");
                let host = url.split("://").nth(1).unwrap_or(url).trim_end_matches('/');
                format!("server {host}")
            }
        };
        let seed = if self.seed < 0 {
            "random".to_string()
        } else {
            self.seed.to_string()
        };
        let up = match (&self.upscale, self.target) {
            (None, _) => String::new(),
            (Some(_), Target::Local) => " +x4".to_string(),
            (Some(_), Target::Server) => format!(" +hires x{}", self.hires_scale),
        };
        format!(
            "{}x{}{up} · {} steps · cfg {} · {} · seed {seed} · {place}",
            self.width, self.height, self.steps, self.cfg, self.sampler
        )
    }

    /// Apply `key=value` (used by /set). Returns a short description of the change.
    pub fn set(&mut self, key: &str, value: &str) -> Result<String> {
        let v = value.trim();
        let none_if_off = |v: &str| -> Option<String> {
            match v {
                "" | "off" | "none" | "no" => None,
                other => Some(other.to_string()),
            }
        };
        match key {
            "width" | "w" => self.width = parse_dim(v)?,
            "height" | "h" => self.height = parse_dim(v)?,
            "size" => {
                let (w, h) = v
                    .split_once(['x', 'X'])
                    .context("size is WIDTHxHEIGHT, e.g. 384x384")?;
                self.width = parse_dim(w)?;
                self.height = parse_dim(h)?;
            }
            "aspect" | "ar" => {
                let (w, h) = aspect_size(self.width, self.height, v)?;
                self.width = w;
                self.height = h;
                return Ok(format!("aspect {v} -> {w}x{h}"));
            }
            "steps" => self.steps = v.parse().context("steps is a whole number")?,
            "cfg" => self.cfg = v.parse().context("cfg is a number")?,
            "sampler" => self.sampler = v.to_string(),
            "seed" => {
                self.seed = if v == "random" || v == "-1" {
                    -1
                } else {
                    v.parse().context("seed is a number or 'random'")?
                }
            }
            "negative" | "neg" => self.negative = v.to_string(),
            "upscale" => self.upscale = none_if_off(v),
            "hires_scale" => self.hires_scale = v.parse().context("hires_scale is a number")?,
            "hires_denoise" => {
                self.hires_denoise = v.parse().context("hires_denoise is a number")?
            }
            "vae_tiling" | "tiling" => self.vae_tiling = parse_bool(v)?,
            "model" => self.model = none_if_off(v),
            "diffusion_model" => self.diffusion_model = none_if_off(v),
            "llm" => self.llm = none_if_off(v),
            "vae" => self.vae = none_if_off(v),
            "tokenizer" => self.tokenizer = none_if_off(v),
            "lora_dir" => self.lora_dir = none_if_off(v),
            "sd_cli" => self.sd_cli = v.to_string(),
            "recipe" => self.recipe = none_if_off(v),
            "url" => {
                self.url = none_if_off(v);
                if self.url.is_some() {
                    self.target = Target::Server;
                }
            }
            "target" => {
                self.target = match v {
                    "local" => Target::Local,
                    "server" => Target::Server,
                    _ => bail!("target is 'local' or 'server'"),
                }
            }
            "conv_direct" => {
                self.conv_direct = match v {
                    "" | "auto" | "default" => None,
                    "on" | "off" => Some(v.to_string()),
                    // sd-cli also takes assignments like "auto,vae=off"
                    other if other.contains('=') => Some(other.to_string()),
                    _ => bail!("conv_direct is auto, on or off"),
                }
            }
            "extra" => self.extra = shlex::split(v).context("could not parse extra arguments")?,
            _ => bail!("unknown setting '{key}' (see /help)"),
        }
        Ok(format!("{key} = {v}"))
    }

    /// Server model selection: slot -> file id, for the components that are set.
    pub fn model_slots(&self) -> Vec<(&'static str, &str)> {
        [
            ("model", &self.model),
            ("diffusion_model", &self.diffusion_model),
            ("llm", &self.llm),
            ("vae", &self.vae),
            ("tokenizer", &self.tokenizer),
        ]
        .into_iter()
        .filter_map(|(slot, v)| v.as_deref().map(|v| (slot, v)))
        .collect()
    }
}

impl Profile {
    /// Current value of a setting, as /set would accept it (for `/cfg` with no value).
    pub fn get(&self, key: &str) -> Option<String> {
        let opt = |v: &Option<String>| v.clone().unwrap_or_else(|| "off".into());
        Some(match key {
            "width" | "w" => self.width.to_string(),
            "height" | "h" => self.height.to_string(),
            "size" => format!("{}x{}", self.width, self.height),
            "aspect" | "ar" => {
                let g = gcd(self.width, self.height);
                format!(
                    "{}:{} ({}x{})",
                    self.width / g,
                    self.height / g,
                    self.width,
                    self.height
                )
            }
            "steps" => self.steps.to_string(),
            "cfg" => self.cfg.to_string(),
            "sampler" => self.sampler.clone(),
            "seed" => {
                if self.seed < 0 {
                    "random".into()
                } else {
                    self.seed.to_string()
                }
            }
            "negative" | "neg" => {
                if self.negative.is_empty() {
                    "(none)".into()
                } else {
                    self.negative.clone()
                }
            }
            "upscale" => opt(&self.upscale),
            "hires_scale" => self.hires_scale.to_string(),
            "hires_denoise" => self.hires_denoise.to_string(),
            "vae_tiling" | "tiling" => {
                if self.vae_tiling {
                    "on".into()
                } else {
                    "off".into()
                }
            }
            "model" => opt(&self.model),
            "diffusion_model" => opt(&self.diffusion_model),
            "llm" => opt(&self.llm),
            "vae" => opt(&self.vae),
            "tokenizer" => opt(&self.tokenizer),
            "lora_dir" => opt(&self.lora_dir),
            "sd_cli" => self.sd_cli.clone(),
            "recipe" => opt(&self.recipe),
            "url" => opt(&self.url),
            "target" => format!("{:?}", self.target).to_lowercase(),
            "conv_direct" => self.conv_direct.clone().unwrap_or_else(|| "auto".into()),
            "extra" => self.extra.join(" "),
            _ => return None,
        })
    }
}

/// New size with aspect ratio `spec` ("16:9", "2:3", or a name) and about the same pixel
/// count as `width`x`height`, each side a multiple of 16.
fn aspect_size(width: u32, height: u32, spec: &str) -> Result<(u32, u32)> {
    let (a, b) = match spec.to_lowercase().as_str() {
        "square" => (1.0, 1.0),
        "portrait" => (2.0, 3.0),
        "landscape" => (3.0, 2.0),
        "wide" => (16.0, 9.0),
        "tall" => (9.0, 16.0),
        other => {
            let (a, b) = other
                .split_once([':', '/', 'x'])
                .context("aspect is W:H (e.g. 16:9) or square/portrait/landscape/wide/tall")?;
            let (a, b): (f64, f64) = (a.trim().parse()?, b.trim().parse()?);
            anyhow::ensure!(a > 0.0 && b > 0.0, "aspect sides must be positive");
            (a, b)
        }
    };
    let area = width as f64 * height as f64;
    let round16 = |x: f64| ((x / 16.0).round() as u32).max(1) * 16;
    Ok((
        round16((area * a / b).sqrt()),
        round16((area * b / a).sqrt()),
    ))
}

fn gcd(a: u32, b: u32) -> u32 {
    if b == 0 { a.max(1) } else { gcd(b, a % b) }
}

fn parse_dim(v: &str) -> Result<u32> {
    let n: u32 = v
        .trim()
        .parse()
        .context("size must be a whole number of pixels")?;
    if n == 0 || !n.is_multiple_of(16) {
        bail!("width/height must be a positive multiple of 16 (got {n})");
    }
    Ok(n)
}

fn parse_bool(v: &str) -> Result<bool> {
    match v {
        "on" | "true" | "yes" | "1" => Ok(true),
        "off" | "false" | "no" | "0" => Ok(false),
        _ => bail!("expected on/off"),
    }
}

pub fn default_path() -> Result<PathBuf> {
    let dirs = directories::ProjectDirs::from("", "", "sd-tui").context("no home directory")?;
    Ok(dirs.config_dir().join("config.toml"))
}

impl Config {
    /// Load `path`, writing the built-in default there first if it doesn't exist.
    pub fn load_or_init(path: &Path) -> Result<Config> {
        if !path.exists() {
            if let Some(dir) = path.parent() {
                fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
            }
            fs::write(path, DEFAULT_CONFIG)
                .with_context(|| format!("writing {}", path.display()))?;
        }
        Config::load(path)
    }

    pub fn load(path: &Path) -> Result<Config> {
        let text =
            fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        let config: Config =
            toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
        if config.profiles.is_empty() {
            bail!("{} has no [profiles.*]", path.display());
        }
        Ok(config)
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        let text = toml::to_string_pretty(self).context("serializing config")?;
        let tmp = path.with_extension("toml.tmp");
        fs::write(&tmp, text).with_context(|| format!("writing {}", tmp.display()))?;
        fs::rename(&tmp, path).with_context(|| format!("replacing {}", path.display()))?;
        Ok(())
    }

    pub fn profile(&self, name: &str) -> Result<Profile> {
        self.profiles.get(name).cloned().with_context(|| {
            let names: Vec<_> = self.profiles.keys().cloned().collect();
            format!("no profile '{name}' (have: {})", names.join(", "))
        })
    }
}

/// Written to ~/.config/sd-tui/config.toml on first run. `quick` is the fast route measured on
/// the M2 Air: Anima Turbo at 384², 8 steps, then RealESRGAN anime x4 -> 1536² in ~40 s.
pub const DEFAULT_CONFIG: &str = r#"# sd-tui config. Profiles are generation setups; switch with /profile NAME, tweak with /set,
# keep tweaks with /save. Local paths may start with "~/".
default_profile = "quick"

[display]
# auto | kitty | sixel | iterm2 | halfblocks
protocol = "auto"

[profiles.quick]
description = "Anima Turbo 384x384, 8 steps, then RealESRGAN anime x4 -> 1536x1536 (~40 s on an M2 Air)"
target = "local"
sd_cli = "sd-cli"
diffusion_model = "~/models/sd-models/diffusion_models/anima/anima-turbo-v1.1.safetensors"
llm = "~/models/sd-models/text_encoders/anima/qwen_3_06b_base.safetensors"
vae = "~/models/sd-models/vae/anima/qwen_image_vae.safetensors"
width = 384
height = 384
steps = 8
cfg = 1.0
sampler = "euler"
seed = -1
upscale = "~/models/sd-models/upscale_models/RealESRGAN_x4plus_anime_6B.pth"

[profiles.full]
description = "Anima Turbo 1024x1024, 8 steps, no upscale (~4.5 min on an M2 Air)"
target = "local"
sd_cli = "sd-cli"
diffusion_model = "~/models/sd-models/diffusion_models/anima/anima-turbo-v1.1.safetensors"
llm = "~/models/sd-models/text_encoders/anima/qwen_3_06b_base.safetensors"
vae = "~/models/sd-models/vae/anima/qwen_image_vae.safetensors"
width = 1024
height = 1024
steps = 8
cfg = 1.0
sampler = "euler"
seed = -1
vae_tiling = true

# Remote profiles talk to sd-server's web API (set url to your server).
# Model fields are file ids relative to the server's first model root.

[profiles.remote]
description = "Anima Turbo 1024x1024 on an sd-server"
target = "server"
url = "http://localhost:7860"
recipe = "anima"
diffusion_model = "diffusion_models/anima/anima-turbo-v1.1.safetensors"
llm = "text_encoders/anima/qwen_3_06b_base.safetensors"
vae = "vae/anima/qwen_image_vae.safetensors"
width = 1024
height = 1024
steps = 8
cfg = 1.0
sampler = "euler"
seed = -1

[profiles.remote-aesthetic]
description = "Anima Aesthetic 1024x1024, 30 steps, cfg 4.5 on an sd-server (best quality)"
target = "server"
url = "http://localhost:7860"
recipe = "anima"
diffusion_model = "diffusion_models/anima/anima-aesthetic-v1.1.safetensors"
llm = "text_encoders/anima/qwen_3_06b_base.safetensors"
vae = "vae/anima/qwen_image_vae.safetensors"
width = 1024
height = 1024
steps = 30
cfg = 4.5
sampler = "euler"
seed = -1
"#;

#[cfg(test)]
mod tests {
    use super::*;

    fn default_config() -> Config {
        toml::from_str(DEFAULT_CONFIG).unwrap()
    }

    #[test]
    fn default_config_parses_and_has_quick() {
        let c = default_config();
        assert_eq!(c.default_profile, "quick");
        let q = c.profile("quick").unwrap();
        assert_eq!((q.width, q.height, q.steps), (384, 384, 8));
        assert!(q.upscale.is_some());
        let k = c.profile("remote").unwrap();
        assert_eq!(k.target, Target::Server);
        assert_eq!(k.recipe.as_deref(), Some("anima"));
        assert_eq!(k.model_slots().len(), 3);
    }

    #[test]
    fn set_parses_values() {
        let mut p = default_config().profile("quick").unwrap();
        p.set("size", "512x768").unwrap();
        assert_eq!((p.width, p.height), (512, 768));
        p.set("seed", "42").unwrap();
        assert_eq!(p.seed, 42);
        p.set("seed", "random").unwrap();
        assert_eq!(p.seed, -1);
        p.set("upscale", "off").unwrap();
        assert!(p.upscale.is_none());
        p.set("url", "http://localhost:7860").unwrap();
        assert_eq!(p.target, Target::Server);
        assert!(p.set("width", "500").is_err()); // not a multiple of 16
        p.set("size", "384x384").unwrap();
        p.set("aspect", "16:9").unwrap();
        assert_eq!((p.width, p.height), (512, 288));
        assert_eq!(p.get("aspect").unwrap(), "16:9 (512x288)");
        p.set("aspect", "portrait").unwrap();
        assert_eq!((p.width % 16, p.height % 16), (0, 0));
        assert!(p.height > p.width);
        p.set("aspect", "square").unwrap();
        assert_eq!((p.width, p.height), (384, 384));
        assert!(p.set("aspect", "banana").is_err());
        assert!(p.set("target", "ssh").is_err());
        assert!(p.set("bogus", "1").is_err());
    }

    #[test]
    fn config_round_trips() {
        let c = default_config();
        let again: Config = toml::from_str(&toml::to_string_pretty(&c).unwrap()).unwrap();
        assert_eq!(again.profiles.len(), c.profiles.len());
    }
}
