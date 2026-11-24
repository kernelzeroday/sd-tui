# sd

A streamlined terminal UI for [stable-diffusion.cpp](https://github.com/leejet/stable-diffusion.cpp).
Type a prompt, press Enter, get an image: shown in the terminal, saved to the current folder, and
logged to a resumable session. Profiles run `sd-cli` locally or send requests to an `sd-server`
over its web API.

```sh
sd                         # open the interface (new session)
sd --resume                # continue the most recent session
sd --resume 20261123-2025  # continue a specific session
sd "a fox in the snow"     # one image, no interface; prints the file path
sd -f prompt.txt           # one image from a prompt file (its lines are joined)
sd -b prompts.txt          # one image per line of a file (prints each path)
sd -p remote "a fox"       # with a specific profile
sd --list                  # profiles
sd --sessions              # recent sessions
```

## The interface

- Each line you type is an image request. Requests typed while one is rendering are queued.
- The prompt box wraps long prompts and grows taller as you type (up to a third of the screen);
  ↑/↓ move between its lines.
- The image pane shows the session's images; the file name is on its top edge, and the prompt
  that made it, its seed, profile and generation time on the bottom edge. ←/→ browse (when the prompt is empty), Home/End jump to
  the first/last.
- Images are shown with the kitty graphics protocol (Ghostty, kitty), sixel, iTerm2, or block
  characters, whichever the terminal supports.
- Images are saved as `sd-YYYYmmdd-HHMMSS-sSEED.png` in the current folder (or `-o DIR`).
- Pasting multi-line text joins the lines with spaces; a paste never starts a generation.
- The status line shows the profile, progress and the queue. Short messages appear there for a
  few seconds (errors stay until Esc or the next prompt); `/help`, `/show` and `/config` open a
  scrollable panel in place of the image (Esc closes it, typing goes back to the prompt).
- sd-cli's warnings and errors, and the interface's own errors, go to the system log rather than
  the screen: `journalctl -t sd` (add `-f` to follow). A failed image's error is also shown.

| Key | Action |
| --- | --- |
| Enter | generate (or run a /command) |
| ←/→, Home/End | browse images (empty prompt); otherwise move the cursor |
| Esc | cancel the running image and clear the queue; otherwise clear the input |
| ↑/↓ | move between the prompt's lines, then prompt history (kept across sessions) |
| Tab | complete a /command, or a file path after `/file`, `/batch`, `/load`, `/upscale`, ... |
| Ctrl+U | clear the input |
| Ctrl+C | clear the input; press twice in quick succession to quit (Ctrl+D twice also quits, on an empty prompt) |

## Commands

A bare command opens a menu of sensible values with the current one marked: type to filter, ↑/↓
and Enter to choose, Esc to close. If nothing in the list matches what you typed, Enter uses the
typed text. `/` on its own opens a menu of all commands.

| Command | |
| --- | --- |
| `/size WxH` | image size (`/size 512x768`) |
| `/aspect W:H` | reshape keeping the pixel count: `16:9`, `2:3`, `square`, `portrait`, `landscape`, `wide`, `tall` |
| `/steps N`, `/cfg N` | sampling steps, guidance |
| `/seed N`, `/seed random` | fixed seed or a new one per image (the menu offers the shown image's seed) |
| `/sampler NAME` | `euler`, `euler_a`, `er_sde`, `dpm++2m`, ... |
| `/neg TEXT`, `/neg off` | negative prompt |
| `/file PATH` | load a prompt from a text file into the input, to edit or press Enter |
| `/batch PATH` | queue one image per line of a text file |
| `/repeat [N]` | queue the last prompt again N times (default 1) with the current settings |
| `/upscale PATH\|NAME\|off` | upscale after generating (see Profiles) |
| `/profile NAME`, `/profiles` | switch / list profiles |
| `/show` | all current settings, as TOML |
| `/set KEY=VALUE ...` | change several settings at once |
| `/save [NAME]` | save the current settings as a profile (rewrites the config file) |
| `/load PATH`, `/reload` | load another config file / re-read this one |
| `/reuse` | put the shown image's prompt in the input to edit |
| `/open` | open the shown image with `xdg-open` |
| `/resume [ID]`, `/new` | switch to (a menu of recent sessions) or start a session |
| `/config`, `/help`, `/quit` | file paths and log location; commands and keys |

Every profile setting also works as `/KEY VALUE` (bare `/KEY` shows it or opens its menu):
`width height vae_tiling conv_direct hires_scale hires_denoise model diffusion_model llm vae tokenizer lora_dir
sd_cli target url recipe extra`.

## Prompt files

Plain text. `/file` and `sd -f` treat the whole file as one prompt, joining its lines with
spaces, so a long prompt can be wrapped however you like. `/batch` and `sd -b` make one image
per line. In both, blank lines and lines starting with `#` are skipped. A bare `/file` or
`/batch` offers the `.txt` files in the current folder; `-` reads standard input
(`sd -b - < prompts.txt`). A one-shot batch keeps going past a failed image and exits non-zero
at the end if any failed.

## Sessions

Every request is appended to `~/.local/share/sd-tui/sessions/<id>.jsonl`: time, prompt, profile,
the full settings, seed, image path, generation time, and whether it succeeded, failed or was
cancelled. Resuming a session restores its image gallery and prompt history and keeps appending
to the same file. One-shot `sd "prompt"` runs are logged to `sessions/cli.jsonl`. On quitting,
sd prints the `sd --resume <id>` command for the session it was in.

Everything typed at the prompt (prompts and commands) is also saved to
`~/.local/share/sd-tui/history`, shared by all sessions like a shell's history, so ↑ reaches
prompts from earlier sessions too. The last 1000 entries are kept.

## Profiles

`~/.config/sd-tui/config.toml` is created on first run with:

| Profile | What | Speed |
| --- | --- | --- |
| `quick` (default) | local: Anima Turbo 384², 8 steps, then RealESRGAN anime x4 -> 1536² | ~40 s on an M2 Air |
| `full` | local: Anima Turbo 1024², 8 steps | ~4.5 min on an M2 Air |
| `remote` | server: Anima Turbo 1024² on an sd-server | seconds on a fast GPU |
| `remote-aesthetic` | server: Anima Aesthetic 1024², 30 steps, CFG 4.5 | best quality |

Generating small and upscaling is the fast path: generation time scales with pixel count while the
ESRGAN upscale takes seconds. Below ~256 px the model stops composing properly.

### Local profiles (`target = "local"`)

```toml
[profiles.quick]
target = "local"
sd_cli = "sd-cli"               # "~/" is expanded
diffusion_model = "~/models/sd-models/diffusion_models/anima/anima-turbo-v1.1.safetensors"
llm = "~/models/sd-models/text_encoders/anima/qwen_3_06b_base.safetensors"
vae = "~/models/sd-models/vae/anima/qwen_image_vae.safetensors"
width = 384
height = 384
steps = 8
cfg = 1.0
sampler = "euler"
seed = -1                       # -1 = random per image (the seed goes in the file name)
upscale = "~/models/sd-models/upscale_models/RealESRGAN_x4plus_anime_6B.pth"   # plain ESRGAN x4
# also: model (single-file checkpoint, -m), tokenizer, lora_dir (for <lora:NAME:W> in prompts),
# negative, vae_tiling, extra = ["--flag", "value"],
# conv_direct = "auto" | "on" | "off"   (sd-cli --conv-direct; see Troubleshooting)
```

### Server profiles (`target = "server"`)

Requests go to sd-server's native job API (`POST /sdcpp/v1/img_gen`, polled for progress, Esc
sends a cancel). The server keeps models loaded between images.

```toml
[profiles.remote]
target = "server"
url = "http://localhost:7860"
recipe = "anima"                # sd-server recipe id (GET /sdcpp/v1/recipes)
# File ids relative to the server's first --model-dir (GET /sdcpp/v1/model-files):
diffusion_model = "diffusion_models/anima/anima-turbo-v1.1.safetensors"
llm = "text_encoders/anima/qwen_3_06b_base.safetensors"
vae = "vae/anima/qwen_image_vae.safetensors"
width = 1024
height = 1024
steps = 8
cfg = 1.0
# upscale = "RealESRGAN_x4plus_anime_6B"  # a hires pass: upscale, then a light redraw
# hires_scale = 4.0
# hires_denoise = 0.35
```

Without `recipe` and model fields, the server's currently loaded model is used. For server
profiles `upscale` names a hires upscaler from the server's `--hires-upscalers-dir` (file name
without extension, or `Latent`/`Lanczos`); sd-server has no plain upscale endpoint. LoRAs aren't
supported on server profiles yet (the API wants a structured list, not prompt tags).

`[display] protocol = "auto"` picks the image protocol; set `kitty`, `sixel`, `iterm2` or
`halfblocks` to force one.

## Troubleshooting

- `RAM weight capacity exceeded. Requested 0 bytes, 0 bytes available`: stable-diffusion.cpp
  refuses to load weights when the system's available memory drops under 1 GiB. On
  unified-memory machines (Apple silicon) the GPU copy counts against RAM too, so another
  sd-server/sd-cli or a big browser can tip it over. `sd` adds this hint to the error.
- `conv2d/conv3d direct auto: Vulkan0 -> off (probe output check failed)` in the system log:
  harmless. On every run sd.cpp self-tests a faster direct-convolution Vulkan kernel and, if
  its output is wrong on this GPU (as on Asahi), uses the standard one. Set
  `conv_direct = "off"` in local profiles to skip the probe and the warning.
- Keys typed in the first moment after start can be swallowed while the image library asks the
  terminal which graphics protocol it supports (only in terminals that never answer).

## Build

```sh
cargo build --release          # binary: target/release/sd
cargo test
packaging/rpm.sh               # RPM -> target/rpmbuild/RPMS/<arch>/
packaging/rpm.sh --install     # build, then install it with pkexec dnf (-> /usr/bin/sd)
cargo install --path .         # or without packaging: -> ~/.cargo/bin/sd
```

Needs `sd-cli` (local profiles) or a reachable `sd-server` (server profiles).
