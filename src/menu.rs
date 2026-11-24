//! Pop-up menus for bare commands (`/cfg`, `/size`, `/profile`, `/`, ...): sensible values to
//! pick from, the current one marked. Typing filters; if nothing matches, Enter uses the typed
//! text as the value. Drawing lives in `tui`.

use std::path::Path;

use crate::config::{Config, Profile, Target};

/// What choosing a row does.
#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    /// `profile.set(key, value)`
    Set(String),
    Profile,
    Resume,
    /// Run the value as a command line (the `/` menu).
    Command,
}

#[derive(Debug, Clone)]
pub struct Choice {
    pub value: String,
    pub note: String,
}

fn c(value: impl Into<String>, note: impl Into<String>) -> Choice {
    Choice {
        value: value.into(),
        note: note.into(),
    }
}

pub struct Menu {
    pub title: String,
    pub action: Action,
    pub choices: Vec<Choice>,
    /// The current value, marked in the list.
    pub current: Option<String>,
    pub filter: String,
    pub cursor: usize,
    visible: Vec<usize>,
}

impl Menu {
    pub fn new(
        title: impl Into<String>,
        action: Action,
        choices: Vec<Choice>,
        current: Option<String>,
    ) -> Menu {
        let mut m = Menu {
            title: title.into(),
            action,
            choices,
            current,
            filter: String::new(),
            cursor: 0,
            visible: Vec::new(),
        };
        m.update();
        // Open on the current value when it's in the list.
        if let Some(cur) = &m.current
            && let Some(pos) = m.visible.iter().position(|&i| m.choices[i].value == *cur)
        {
            m.cursor = pos;
        }
        m
    }

    /// Indices of the rows that match the filter (every word must appear).
    pub fn visible(&self) -> &[usize] {
        &self.visible
    }

    fn update(&mut self) {
        let words: Vec<String> = self
            .filter
            .to_lowercase()
            .split_whitespace()
            .map(String::from)
            .collect();
        self.visible = (0..self.choices.len())
            .filter(|&i| {
                let hay =
                    format!("{} {}", self.choices[i].value, self.choices[i].note).to_lowercase();
                words.iter().all(|w| hay.contains(w.as_str()))
            })
            .collect();
        self.cursor = self.cursor.min(self.visible.len().saturating_sub(1));
    }

    pub fn move_by(&mut self, delta: isize) {
        let n = self.visible.len();
        if n > 0 {
            self.cursor = (self.cursor as isize + delta).clamp(0, n as isize - 1) as usize;
        }
    }

    pub fn push(&mut self, ch: char) {
        self.filter.push(ch);
        self.cursor = 0;
        self.update();
    }

    pub fn pop(&mut self) {
        self.filter.pop();
        self.update();
    }

    /// The value Enter would choose: the highlighted row, or the typed text when nothing matches.
    pub fn chosen(&self) -> Option<String> {
        match self.visible.get(self.cursor) {
            Some(&i) => Some(self.choices[i].value.clone()),
            None if !self.filter.trim().is_empty() => Some(self.filter.trim().to_string()),
            None => None,
        }
    }
}

/// Menu for a bare setting command (`/cfg`), or None if the setting has no useful presets.
pub fn for_setting(key: &str, profile: &Profile, shown_seed: Option<i64>) -> Option<Menu> {
    let choices = match key {
        "size" => vec![
            c("256x256", "fastest draft"),
            c("384x384", "quick draft (x4 upscale -> 1536)"),
            c("512x512", "SD1.5 native"),
            c("512x768", "SD1.5 portrait"),
            c("768x512", "SD1.5 landscape"),
            c("768x768", ""),
            c("1024x1024", "Anima / SDXL / Qwen-Image native"),
            c("832x1216", "portrait, ~1 MP"),
            c("1216x832", "landscape, ~1 MP"),
            c("1344x768", "wide 16:9, ~1 MP"),
            c("768x1344", "tall 9:16, ~1 MP"),
            c("1536x1536", "Anima maximum"),
        ],
        "aspect" | "ar" => vec![
            c("square", "1:1"),
            c("portrait", "2:3"),
            c("landscape", "3:2"),
            c("wide", "16:9"),
            c("tall", "9:16"),
            c("4:3", ""),
            c("3:4", ""),
            c("21:9", "cinema"),
        ],
        "steps" => vec![
            c("4", "very rough"),
            c("6", ""),
            c("8", "Turbo default"),
            c("12", "Turbo, more detail"),
            c("20", ""),
            c("25", "SD1.5 typical"),
            c("30", "Anima Aesthetic"),
            c("40", ""),
            c("50", "slow, max detail"),
        ],
        "cfg" => vec![
            c("1", "Turbo / distilled models"),
            c("2", ""),
            c("3", "MOSS-Image"),
            c("4", ""),
            c("4.5", "Anima Aesthetic"),
            c("5", "SDXL / Illustrious"),
            c("6", "Qwen-Image"),
            c("7", "SD1.5"),
            c("9", "strong"),
        ],
        "seed" => {
            let mut v = vec![c("random", "new seed every image")];
            if let Some(s) = shown_seed {
                v.push(c(s.to_string(), "seed of the shown image"));
            }
            if profile.seed >= 0 {
                v.push(c(profile.seed.to_string(), "current fixed seed"));
            }
            v.extend([c("42", ""), c("1", ""), c("1234", "")]);
            v
        }
        "sampler" => vec![
            c("euler", "default for flow/DiT models (Anima, Qwen, MOSS)"),
            c("euler_a", "SD1.5 default, more variety"),
            c("er_sde", "Anima card suggestion for non-Turbo"),
            c("dpm++2m", "SD1.5/SDXL classic"),
            c("dpm++2m_sde", ""),
            c("dpm++2s_a", ""),
            c("heun", "slower, accurate"),
            c("res_multistep", ""),
            c("lcm", "LCM models/LoRAs only"),
            c("ddim_trailing", ""),
        ],
        "negative" | "neg" => vec![
            c("off", "no negative prompt"),
            c(
                "lowres, bad anatomy, bad hands, text, error, missing fingers, worst quality, low quality, jpeg artifacts, signature, watermark, blurry",
                "classic anime",
            ),
            c("worst quality, low quality, blurry, watermark", "short"),
            c("text, watermark, signature, logo", "no text"),
            c(
                "deformed, disfigured, extra limbs, bad proportions",
                "anatomy",
            ),
        ],
        "upscale" => match profile.target {
            Target::Local => {
                let mut v = vec![c("off", "no upscale")];
                v.extend(
                    upscalers_near(profile)
                        .into_iter()
                        .map(|p| c(p, "ESRGAN x4 after generating")),
                );
                v
            }
            Target::Server => vec![
                c("off", "no hires pass"),
                c(
                    "RealESRGAN_x4plus_anime_6B",
                    "hires pass with RealESRGAN anime",
                ),
                c("Latent", "hires pass, latent upscale"),
                c("Lanczos", "hires pass, Lanczos"),
            ],
        },
        "vae_tiling" | "tiling" => vec![c("on", "less memory when decoding"), c("off", "")],
        "hires_scale" => vec![c("1.5", ""), c("2", ""), c("3", ""), c("4", "")],
        "hires_denoise" => vec![
            c("0.2", "keep the image, add a little detail"),
            c("0.35", "default"),
            c("0.5", ""),
            c("0.7", "redraw a lot"),
        ],
        "target" => vec![
            c("local", "run sd-cli here"),
            c("server", "send to sd-server (needs url)"),
        ],
        _ => return None,
    };
    let title = format!("/{key}");
    Some(Menu::new(
        title,
        Action::Set(key.to_string()),
        choices,
        profile.get(key),
    ))
}

/// Upscaler files in the `upscale_models` folder next to the profile's models (local profiles).
fn upscalers_near(profile: &Profile) -> Vec<String> {
    let mut dirs = Vec::new();
    if let Some(cur) = &profile.upscale
        && let Some(parent) = Path::new(cur).parent()
    {
        dirs.push(parent.to_string_lossy().into_owned());
    }
    dirs.push("~/models/sd-models/upscale_models".into());
    let mut out = Vec::new();
    for d in dirs {
        let expanded = crate::runner::expand_home(&d);
        let Ok(entries) = std::fs::read_dir(&expanded) else {
            continue;
        };
        for e in entries.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            if [".pth", ".safetensors", ".gguf"]
                .iter()
                .any(|ext| name.ends_with(ext))
            {
                let shown = format!("{}/{name}", d.trim_end_matches('/'));
                if !out.contains(&shown) {
                    out.push(shown);
                }
            }
        }
    }
    out.sort();
    out
}

pub fn profiles(config: &Config, current: &str) -> Menu {
    let choices = config
        .profiles
        .iter()
        .map(|(name, p)| {
            c(
                name.clone(),
                p.description.clone().unwrap_or_else(|| p.summary()),
            )
        })
        .collect();
    Menu::new(
        "/profile",
        Action::Profile,
        choices,
        Some(current.to_string()),
    )
}

pub fn sessions(rows: &[crate::session::Summary], current: &str) -> Menu {
    let choices = rows
        .iter()
        .map(|s| {
            c(
                s.id.clone(),
                format!("{} images · {}", s.images, s.last_prompt),
            )
        })
        .collect();
    Menu::new(
        "/resume",
        Action::Resume,
        choices,
        Some(current.to_string()),
    )
}

pub fn commands(list: &[(&str, &str)]) -> Menu {
    let choices = list.iter().map(|(cmd, note)| c(*cmd, *note)).collect();
    Menu::new("commands", Action::Command, choices, None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::DEFAULT_CONFIG;

    fn quick() -> Profile {
        toml::from_str::<Config>(DEFAULT_CONFIG)
            .unwrap()
            .profile("quick")
            .unwrap()
    }

    #[test]
    fn opens_on_the_current_value() {
        let m = for_setting("steps", &quick(), None).unwrap();
        assert_eq!(m.chosen().as_deref(), Some("8"));
    }

    #[test]
    fn filter_needs_every_word_and_falls_back_to_typed_text() {
        let mut m = for_setting("cfg", &quick(), None).unwrap();
        for ch in "anima".chars() {
            m.push(ch);
        }
        assert_eq!(m.chosen().as_deref(), Some("4.5"));
        let mut m = for_setting("cfg", &quick(), None).unwrap();
        for ch in "3.3".chars() {
            m.push(ch);
        }
        assert!(m.visible().is_empty());
        assert_eq!(m.chosen().as_deref(), Some("3.3"));
    }

    #[test]
    fn seed_menu_offers_the_shown_images_seed() {
        let m = for_setting("seed", &quick(), Some(777)).unwrap();
        assert!(m.choices.iter().any(|ch| ch.value == "777"));
    }

    #[test]
    fn cursor_stops_at_both_ends() {
        let mut m = for_setting("aspect", &quick(), None).unwrap();
        m.move_by(-5);
        assert_eq!(m.cursor, 0);
        m.move_by(100);
        assert_eq!(m.cursor, m.visible().len() - 1);
    }

    #[test]
    fn settings_without_presets_have_no_menu() {
        assert!(for_setting("llm", &quick(), None).is_none());
    }
}
