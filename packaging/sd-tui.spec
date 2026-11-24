# Built in place from the source tree by packaging/rpm.sh (rpmbuild --build-in-place):
# no tarball, cargo fetches dependencies as for a normal build.

%global debug_package %{nil}

Name:           sd-tui
Version:        %{?pkgver}%{!?pkgver:0.0.0}
Release:        1%{?dist}
Summary:        Terminal UI for stable-diffusion.cpp: type a prompt, get an image
License:        Apache-2.0
URL:            https://github.com/kernelzeroday/sd-tui

BuildRequires:  cargo
BuildRequires:  rust
Recommends:     xdg-utils

%description
sd is a streamlined terminal interface for stable-diffusion.cpp. Type a prompt, press
Enter, get an image: shown in the terminal (kitty, sixel, iTerm2 or block characters),
saved to the current folder, and logged to a resumable session. Profiles run sd-cli
locally or send requests to an sd-server over its web API. Warnings and errors go to
the system log (journalctl -t sd).

%build
# A separate target dir keeps rpm's build flags from invalidating the normal cargo build.
CARGO_TARGET_DIR=target/rpm cargo build --release --locked

%install
install -Dpm 0755 target/rpm/release/sd %{buildroot}%{_bindir}/sd

%files
%license LICENSE
%doc README.md
%{_bindir}/sd

%changelog
* Tue Nov 24 2026 kernelzeroday - 0.2.6-1
- /repeat [N] queues the last prompt again N times.

* Tue Nov 24 2026 kernelzeroday - 0.2.2-1
- Ctrl+C / Ctrl+D must be pressed twice in quick succession to quit.

* Tue Nov 24 2026 kernelzeroday - 0.2.1-1
- Tab completes file paths for /file, /batch, /load and path settings.

* Tue Nov 24 2026 kernelzeroday - 0.2.0-1
- Prompt box grows with long prompts; notices replace the log pane; /file and /batch;
  sd -f / sd -b; diagnostics to the system log; conv_direct setting; RPM packaging.
