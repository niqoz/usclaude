# usclaude

*[Version française](README.fr.md)*

A small Linux applet that shows your [Claude Code](https://claude.com/claude-code)
usage limits in the system tray — the same figures as the `/usage` command: the
5-hour session and the weekly limits, with their reset times.

![usclaude in the XFCE panel, menu open (French locale)](assets/screenshot.png)

The icon shows two gauges, **session** on the left and **week** on the right: green
below 50 %, orange below 80 %, red above. Details appear on hover and in the menu.

Written in Rust with [ksni](https://github.com/iovxw/ksni) (StatusNotifierItem
protocol, no GTK). A single static binary with no dependencies.

> Independent project, not affiliated with Anthropic. The interface is in English,
> or in French when the system language is French.

## Requirements

- **Claude Code signed in with a claude.ai account** (Pro or Max), i.e. via
  `/login`. An API key has no `/usage` limits to show.
- **A panel that supports StatusNotifierItem**, the standard tray icon protocol.
  X11 or Wayland makes no difference.

## Compatibility

| Desktop | Support |
| --- | --- |
| KDE Plasma | ✅ native |
| XFCE 4.16 and later | ✅ *Status Tray* plugin (tested on XFCE 4.20) |
| LXQt, Cinnamon | ✅ native |
| GNOME on Ubuntu | ✅ AppIndicator extension installed by default |
| GNOME elsewhere (Debian, Fedora…) | ⚠️ install the *AppIndicator and KStatusNotifierItem Support* extension |
| MATE, Budgie | ⚠️ depends on the version and the tray applet in use |
| Sway, Hyprland… with waybar | ✅ if the `tray` module is enabled |
| i3bar, polybar, LXDE | ❌ only support the older XEmbed protocol |

Only XFCE 4.20 has been tested; the rest relies on each desktop's advertised
support.

Without a compatible tray, the applet does not quit: it says so in the terminal and
waits. The icon appears as soon as the panel is ready, which also covers session
startup (when the applet starts before the panel) and panel restarts.

## Installation

From the [latest release](https://github.com/niqoz/usclaude/releases/latest).

**Debian, Ubuntu, Linux Mint**:

```sh
sudo apt install ./usclaude_0.1.2_amd64.deb
```

The applet is added to the menu under **Accessories** and can also be started by
typing `usclaude`.

**Other distributions** (x86_64): static binary, no dependencies at all.

```sh
tar xzf usclaude-0.1.2-x86_64-linux.tar.gz
install -m 755 usclaude-0.1.2-x86_64-linux/usclaude ~/.local/bin/
usclaude &
```

**From source** (Rust 1.89 or later):

```sh
cargo install --git https://github.com/niqoz/usclaude
```

To start it with your session, tick **Start at login** in its menu.

## Menu

| Entry | Meaning |
| --- | --- |
| Limits | Percentage used and reset time of each limit. |
| Refresh | Refresh now. |
| Settings | Refresh interval: 90 s, 3 min, 5 min or 10 min. |
| Start at login | Creates or removes the autostart entry. |
| Restart | Restarts the applet, e.g. after updating the binary. |
| Quit | Quits. |

Only one instance runs at a time: a second launch exits immediately.

## How it works

The applet queries the same service as `/usage`
(`https://api.anthropic.com/api/oauth/usage`) with Claude Code's sign-in token,
read from `~/.claude/.credentials.json` (or `$CLAUDE_CONFIG_DIR`).

The token is **read, never modified**: the applet does not refresh it itself, as
that would invalidate Claude Code's session. When it expires, the applet shows
"token expired" until the next use of `claude`, which renews it.
No other data is sent anywhere.

Files used:

| File | Purpose |
| --- | --- |
| `~/.claude/.credentials.json` | Sign-in token, read-only. |
| `~/.config/usclaude/interval` | Chosen interval, in seconds. |
| `~/.cache/usclaude/last.json` | Last valid response, shown again at startup. |
| `~/.config/autostart/usclaude.desktop` | Autostart, if enabled. |
| `$XDG_RUNTIME_DIR/usclaude-$USER.lock` | Single-instance lock. |

## Troubleshooting

```sh
usclaude --print
```

prints the usage once in the terminal, or the error encountered.

## Known limitations

- **Undocumented service**: the address and response format are not public and
  may change without notice. Unknown limits are shown under their raw name as soon
  as they go above 0 %.
- **Frequent refresh**: the service may answer "too many requests" (HTTP 429).
  The applet then keeps the last values and waits for the delay given by the
  service (`Retry-After` header, up to 1 h). Without one, it doubles its wait on
  each refusal, up to 10 min. It returns to the chosen interval as soon as a
  request succeeds; the menu shows the time of the next attempt.
- **At startup**, the applet immediately shows the last known figures with their
  time ("Updated Wed 9 at 15:11"), until the first request succeeds. An error
  that occurred since is shown on its own menu line.
- **Language**: English, or French when the system language is French
  (`LANG=fr_…`). To force English: `LANG=en_US.UTF-8 usclaude`.

## Tests

```sh
cargo test
cargo clippy --all-targets
```

## Building releases

```sh
./packaging/build-release.sh
```

builds the `.tar.gz` archive and the `.deb` package in `target/dist/`, both with a
static (musl) binary. Requires the `rustup target add x86_64-unknown-linux-musl`
target and the `musl-tools`, `dpkg-dev` and `fakeroot` packages.

## License

MIT
