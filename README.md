# Catch — native download manager for Omarchy

> **Status: MVP.** Catch works and is worth using, but the panel layout,
> settings, and CLI surface still change between versions. Files already
> downloaded stay where they were put.

© 2026 Taha Sadough — MIT, see [LICENSE](LICENSE).

Bar widget + Rust backend. Catch links, images, or whole pages from the
browser, and it can download YouTube videos. The `catch` CLI owns the
queue and the bytes on disk. YouTube downloads need `yt-dlp` on your
`PATH`.

## What Catch does

- **YouTube videos** — paste a link into Catch or send it from the browser,
  and Catch downloads the video through `yt-dlp`. The browser menu also
  offers **audio only** for a video's sound. Needs `yt-dlp` on your `PATH`.
- **Links, images, and whole pages** — send a page and Catch takes every
  link, image, video, media file, or file on it, or just the links you
  selected.
- **Plain files** — a plain left-click on a file in a connected browser goes
  to Catch instead of the browser's own downloader. Right-click and choose
  **Catch this** to send one deliberately.
- **A parallel queue** — one to ten transfers at once, on the bar, with the
  rest waiting their turn.
- **A duplicate guard** — send a link you already have and Catch asks
  before saving a second copy.

## Install

Catch has two parts. The bar plugin is the panel. The installer connects
your browsers.

```bash
omarchy plugin add https://github.com/tahadx/catch --enable
~/.config/omarchy/plugins/io.github.tahadx.catch/install.sh
```

`omarchy plugin add` puts Catch on the bar and stops there. The first
time the bar loads Catch, it links `catch` and `catch-host` into
`~/.local/bin` and writes the host files below. `install.sh` runs that
setup when it has not happened yet, prints which supported browsers are
on your `PATH`, and asks before adding Super+D. Press Enter to add it, or `n` to skip. Skipping
prints the one line for `~/.config/hypr/bindings.lua`. `install.sh --yes`
adds Super+D without asking.

Running `install.sh` again is safe. An existing Super+D line is left as it
is. Once Catch is configured, the script also leaves existing host files
alone, so a browser you install later needs the two commands in
[A browser you install later](#a-browser-you-install-later).

The program that gets linked is the one already in the plugin at
`bin/dist`. Install does not compile it.

## Browsers

A host file lets the browser start Catch. The extension is what sends
downloads to it. Host files are written for every browser in the two lists
below, even ones you have not installed. The extension step is different
for each family.

### Chromium, Chrome, Brave, and Edge

Host files are written for:

- Chromium — `~/.config/chromium`
- Chrome stable, beta, and unstable
- Brave stable, beta, nightly, and Origin (stable, beta, nightly)
- Microsoft Edge stable and dev

The extension loads itself on the next start when that browser already has
a flags file in `~/.config`:

| Browser | Flags file |
| --- | --- |
| Chromium | `chromium-flags.conf` |
| Chrome stable | `chrome-flags.conf` |
| Brave stable | `brave-flags.conf` |
| Brave Origin | `brave-origin-flags.conf` |
| Edge stable | `microsoft-edge-flags.conf` |
| Edge dev | `microsoft-edge-dev-flags.conf` |

Restart the browser once after installing. Open the extensions page and
confirm Catch is listed.

Chrome beta, Chrome unstable, Brave beta, and Brave nightly get a host
file and are not in the flags table, so they do not autoload. Load the
extension by hand for those. Use the same steps when a flags file was
missing and the Catch menu never appears:

1. Open `chrome://extensions`, `brave://extensions`, or `edge://extensions`.
2. Turn on Developer mode.
3. Choose Load unpacked, and pick `~/.config/omarchy/plugins/io.github.tahadx.catch/extensions/chromium-catch`.

That copy stays installed until you remove it.

### Firefox, Zen, Floorp, LibreWolf, and Waterfox

Host files are written for Firefox, Zen, Floorp, LibreWolf, and Waterfox.
Flatpak builds of Firefox, Zen, and LibreWolf get one too, under
`~/.var/app/…`.

Firefox will not keep an unsigned extension. Load it after each launch:

1. Open `about:debugging#/runtime/this-firefox`.
2. Click Load Temporary Add-on.
3. Pick any file in `~/.config/omarchy/plugins/io.github.tahadx.catch/extensions/firefox-catch`.

Quit the browser and the add-on is gone. Load it again next time. Do this
once per browser (Firefox and Zen are separate).

A Flatpak browser can still refuse to run `catch-host`, because that
program sits outside the sandbox. Install the distro package of that
browser when the menu does nothing.

### A browser you install later

`install.sh` does not rewrite host files after the first setup. For a
browser added afterwards:

```bash
catch install-hosts
catch install-extension
```

`install-hosts` rewrites every host file in the lists above.
`install-extension` updates the Chromium flags files that already exist.
Restart that Chromium-family browser. Firefox-family still needs the
temporary add-on after every launch.

### Other browsers

Vivaldi, Opera, and other forks are not given a host file. Their downloads
stay in that browser.

## Use it

- Left-click the bar icon or Super+D: open Catch
- A YouTube link downloads the video. Paste it into Catch, or send it
  from the browser. `yt-dlp` has to be installed
- Every row carries its own controls: a tick to mark it, a stop, and a
  bin. Stop ends a live transfer and keeps the row so you can re-queue
  it; the bin removes the row and keeps the file. On a downloading row
  the bin stops the transfer too
- A marked row is tinted, so a mark stays visible after you move on
- Click a row's buttons, or use keys: `space` marks · `a` all ·
  `c` deletes marked · `x` stops · `o` opens the download folder ·
  `s` settings · `esc` closes
- Enter does nothing on its own. Opening a finished file is the row's own
  button, and the download folder is `o` or the header button — Enter
  never puts a file browser on screen
- Send a link that's already in the download folder — typed into Catch or
  handed over by the browser — and Catch asks instead of saving a second
  copy. Open the file you have, download another copy, or replace it
  (`ctrl+r` replaces, which discards the old bytes)
- Catch remembers what it saved for a link, so the same question comes up
  even after the queue rows are cleared or removed — the files are still on
  disk. Delete one of those files outside Catch and the next send treats the
  link as new again
- Right-click the icon: open the download folder
- Browser, once that browser's extension is loaded: right-click → Catch
  menu (this link, as video, audio only, all links/images/videos/media/files
  on the page, selected links, browser download, open folder, copy page
  URL). Plain left-clicks on files go to Catch; **Catch this** sends one
  deliberately.

Downloads land in `~/Downloads/catch`. In the panel, the gear icon or `s`
opens settings: the download folder, how many transfers run at once (1–10),
and how many byte-range segments each large download uses (1 disables
segmented downloading).

## Segmented downloads

Catch can download a single large file through several parallel HTTP
requests, each fetching a distinct byte range, when the server supports it.
This is automatic and never changes the final file: the bytes are written
into their assigned positions of one `.part` file and verified before the
file is renamed into place.

Eligibility (all must hold):

- The file is at least 32 MiB (override with `CATCH_SEGMENT_MIN_BYTES`).
- The server answers a real `Range: bytes=0-1023` probe with `206 Partial
  Content`, a valid `Content-Range` carrying the total length, and the
  expected byte count.
- The response uses identity content encoding (no transparent compression
  that would invalidate byte offsets).
- The server exposes an `ETag` or `Last-Modified` validator, so a changed
  representation can be detected instead of being spliced into the file.
- There is enough connection budget left: Catch divides a global budget
  (`CATCH_MAX_CONNECTIONS`, default 8) across the active transfers, so
  three transfers never open 24 connections. Each transfer is capped by
  the segments setting (1–8, default 4) and by a minimum useful segment
  size of 8 MiB (`CATCH_SEGMENT_MIN_PIECE`).

When any of these fails, Catch uses its ordinary sequential downloader for
that file. A server that ignores `Range` and answers `200 OK`, returns a
mismatched `Content-Range`, changes validators between requests, or
rejects a range (HTTP 416) makes the segmented attempt fall back to a
fresh sequential download from byte zero — bytes from two different
representations are never mixed.

Resumption is segment-aware. A sidecar file named `<file>.<id>.part.meta`
next to the `.part` file records which ranges are durably committed (a
range is committed only after its bytes are written and synchronized).
After an interruption or a restart, completed ranges are reused and only
the missing ones are fetched again. An existing `.part` file from an older
Catch version, or one with a missing or corrupt manifest, is never
reinterpreted as a set of segments: both files are discarded and the
transfer starts over. Removing a download deletes both files.

Diagnostics: while a segmented download is running, the row shows `done/total
seg`. The manifest is ordinary JSON and can be inspected with
`cat ~/Downloads/catch/<file>.<id>.part.meta`. A failed segment is named in
the worker's error message and in the row's error field; the partial data
stays on disk for a manual retry.

Environment overrides (all optional):

| Variable | Default | Meaning |
| --- | --- | --- |
| `CATCH_SEGMENTS` | 4 (or the panel setting) | Segments per eligible file (1 disables) |
| `CATCH_SEGMENT_MIN_BYTES` | 33554432 | Smallest file eligible for segmentation |
| `CATCH_SEGMENT_MIN_PIECE` | 8388608 | Smallest useful segment size |
| `CATCH_MAX_CONNECTIONS` | 8 | Global connection budget across transfers |

## Requirements

Catch ships its own backend as a Linux x86_64 binary in `bin/dist`, so
there is nothing to compile and no package to install. It calls a few
programs already on your machine:

| Program | Needed for | Comes from |
| --- | --- | --- |
| `yt-dlp` | YouTube and other video downloads | you install this one |
| `wl-paste` | pasting a link into Catch | `wl-clipboard` |
| `wl-copy` | copying a page URL from the browser menu | `wl-clipboard` |
| `xdg-open` | opening a file or the download folder | `xdg-utils` |
| `systemctl` | scheduled downloads | systemd |
| `omarchy-notification-send` | desktop notifications | Omarchy |

Only `yt-dlp` is worth installing yourself. Without it a video link
fails with `spawn yt-dlp (is it installed?)` rather than downloading;
everything else on this list is already part of an Omarchy install. The
panel, the service, and the browser extension are all in this repository.

## Remove Catch

Uninstall in two steps, in this order. The first removes what Catch put
on your machine; the second removes the plugin itself.

**1. Clean up the installed pieces.** From the plugin directory:

```bash
~/.config/omarchy/plugins/io.github.tahadx.catch/bin/uninstall
```

That removes the scheduler timer, the `catch` and `catch-host` symlinks in
`~/.local/bin`, Catch's `Super+D` binding (other bindings are left byte for
byte), every browser host manifest, and Catch's entry in the Chromium-family
flags files — leaving Omarchy's own flags alone. It is idempotent, so running
it twice is fine.

Already removed the plugin? Run it from a fresh clone instead:

```bash
git clone --depth 1 https://github.com/tahadx/catch
./catch/bin/uninstall
```

**2. Remove the plugin.**

```bash
omarchy plugin remove io.github.tahadx.catch
```

**What is left alone:** your queue records and everything in your
download folder. Catch never deletes a file you downloaded; remove those
yourself if you want them gone. The Firefox add-on and the Chromium
extension live in each browser's own profile, so remove them there too.
