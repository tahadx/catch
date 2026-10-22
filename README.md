# Catch

Catch is a native download manager for Omarchy: a bar widget shows the queue,
a Rust backend owns the transfers, and browser extensions hand downloads over
from the Chromium and Firefox families.

## Features

- **Browser hand-off** — left-click a download in a connected browser, or
  right-click for links, page media, selected links, video, or audio.
- **One or many links** — paste a link or list; each unique http(s) link becomes a row.
- **Video and audio** — YouTube/Vimeo links use `yt-dlp`; audio-only is in the browser menu.
- **Resumable queue** — 1, 3, 5, or 10 transfers run at once; pause or stop
  keeps partial data. Re-sending a saved link asks before a second copy.
- **Segmented downloads** — large files use parallel ranges when supported,
  with a plain fallback. See [segmented downloads](docs/segmented-downloads.md).

## Requirements

Catch ships a prebuilt Linux x86_64 backend in `bin/dist`; nothing is compiled.
It uses programs already on an Omarchy machine, except `yt-dlp`:

| Program | Needed for | Source |
| --- | --- | --- |
| `yt-dlp` | Video and audio downloads | **install this one** |
| `wl-paste`, `wl-copy` | Clipboard access | `wl-clipboard` |
| `xdg-open` | Opening files and folders | `xdg-utils` |
| `systemctl`, `omarchy-notification-send` | Schedule, alerts | systemd, Omarchy |

Only `yt-dlp` is not part of Omarchy: `omarchy pkg add yt-dlp`.

## Install

```bash
omarchy plugin add https://github.com/tahadx/catch --enable
~/.config/omarchy/plugins/io.github.tahadx.catch/install.sh
```

The first command puts Catch on the bar. The installer links `catch` and
`catch-host` into `~/.local/bin`, writes host files, registers the extension,
and asks before adding `Super+D` (`n` skips; `--yes` adds it). Re-running is safe.

Restart the browser once, then follow [browser support](docs/browsers.md).

## Browser support

| Browser family | Extension |
| --- | --- |
| Chromium, Brave stable/Origin, Edge | Loads next start if a flags file exists |
| Google Chrome, Brave beta/nightly | One-time **Load unpacked** |
| Firefox, Zen, Floorp, LibreWolf, Waterfox | **Load Temporary Add-on** each launch |

Flatpak browsers may block the host; use the distro package. Vivaldi, Opera,
and other unlisted browsers are unsupported. Details: [browser support](docs/browsers.md).

## Usage and keyboard shortcuts

Left-click the bar icon or press `Super+D`; right-click opens the download folder.

- **Add links.** Paste one link or a list and press `enter`; each unique http(s)
  link becomes its own row. `ctrl+v` pastes from anywhere in the panel.
- **From the browser.** Left-clicking a file link sends it to Catch. Right-click
  → **Catch** for links, page media, selected links, video, or audio;
  **Download with browser** bypasses Catch, **Copy page URL** copies only the URL.
- **Videos.** YouTube and Vimeo links use `yt-dlp`; **Catch as video** forces
  it for other supported sites.
- **Control the queue.** Each row has a mark, a stop, and a bin. Stop keeps
  partial data; the bin removes it and the row but keeps a finished file. `r`
  gives a failed or stopped row a new address, reusing bytes only for the same file.
- **Already saved links.** Sending a link whose file is in the download folder
  asks whether to open, save another copy, or replace it, even after its row is gone.

| Key | Action |
| --- | --- |
| `↑` `↓` / `j` `k` | Move between rows |
| `space` / `a` | Mark a row / all rows |
| `p` / `x` | Pause or resume (marked rows too) / stop, keeping data |
| `r` | Give a failed or stopped row a new address |
| `c` / `delete` | Remove marked rows / cursor row; finished file stays |
| `d` | Delete marked files from disk |
| `enter` | Open a finished file |
| `tab` / `ctrl+v` | Switch field/rows / paste links |
| `o` / `s` | Open folder / settings |
| `esc` / `q` | Back one screen / close |

## Configuration

The gear icon or `s` opens settings, stored in `~/.config/omarchy/shell.json`
(also editable from Omarchy's settings).

| Setting | Default | Notes |
| --- | --- | --- |
| Download folder | `~/Downloads/catch` | Empty uses `$XDG_DOWNLOAD_DIR/catch` when set |
| Parallel transfers | 3 | 1, 3, 5, or 10 |
| Connections per file | 4 | 1, 2, 4, or 8; 1 disables segmented downloads |
| Bandwidth | unlimited / 0 KB/s | Mode (`balanced`, `low`) plus optional global cap |
| Schedule | off | e.g. `02:00`–`06:00`; enabling installs a systemd timer |
| Panel position | bar | `bar`, `top`, `center`, `left`, `right`, `bottom` |

Headless overrides: `CATCH_MAX_PARALLEL`, `CATCH_SEGMENTS`, `CATCH_BW_MODE`,
`CATCH_BW_CAP_BPS`; connection controls are in
[segmented downloads](docs/segmented-downloads.md#environment-overrides).
`CATCH_CATEGORIES=on` sorts files into type folders.

## Update and uninstall

```bash
omarchy plugin update io.github.tahadx.catch
```

Restart the browser to pick up extension changes. To remove Catch, clean up
first, then remove the plugin:

```bash
~/.config/omarchy/plugins/io.github.tahadx.catch/bin/uninstall
omarchy plugin remove io.github.tahadx.catch
```

`bin/uninstall` removes the timer, `catch`/`catch-host` links, the `Super+D`
binding, host files, and flags entries. Downloads and queue records stay. If
the plugin is gone, run it from a fresh clone and remove the extension in the browser.

## Development

The panel and service are `Panel.qml`, `Service.qml`, and `Model.js`; the
backend, extensions, and tooling live in `crates/catch`, `extensions/`, and `bin/`.

```bash
bin/refresh-dist          # rebuild bin/dist from source
cargo test -p catch       # backend tests
node tests/model.test.js  # panel model tests
bin/check-release         # version and shipped-binary consistency
```

Release steps: [release](docs/release.md).

## License

MIT © 2026 Taha Sadough. See [LICENSE](LICENSE).
