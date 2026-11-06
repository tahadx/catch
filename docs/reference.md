# Catch reference

Detailed reference for the Catch CLI, settings, files, transfer behavior,
troubleshooting, and uninstall. See the [README](../README.md) for installation
and panel usage, and [design](design.md) for the interface.

## CLI

The panel is a front end for the `catch` command. The command is also usable on
its own:

| Command | Action |
| --- | --- |
| `catch add <url> [--dir <path>] [--video] [--audio] [--sha256 <hex>]` | Queue a URL and download it now (blocking) |
| `catch enqueue <urls...> [--video] [--audio] [--again] [--sha256 <hex>]` | Queue one or more URLs and return at once |
| `catch download <id> [--dir <path>]` | Download one queued entry (blocking) |
| `catch video <id> [--dir <path>] [--audio]` | Fetch one entry with `yt-dlp` (blocking) |
| `catch list [--json]` | List the queue; `--json` prints the raw state |
| `catch drain [--max <n>]` | Start queued entries while slots are free |
| `catch tick` | One scheduler pass, run every minute by the timer |
| `catch schedule [--install] [--remove]` | Show, install, or remove the scheduler timer |
| `catch pause <id>` / `catch pause --all` | Pause, keeping partial data |
| `catch resume <id>` / `catch resume --all` | Re-queue a paused, failed, or stopped entry |
| `catch cancel <id>` / `catch cancel --all` | Stop, keeping the row and partial data |
| `catch remove <id...> [--delete-file]` | Remove rows; `--delete-file` also deletes files |
| `catch refresh <id> <url> [--force]` | Point a stopped/failed entry at a fresh address |
| `catch priority <id> <level>` | Set the transfer priority; higher runs first |
| `catch checksum <id> <hex|none>` | Set or clear an expected SHA-256 |
| `catch why <id>` | Explain one entry: partial data, segments, retries |
| `catch rename <url> <title...>` | Rename what Catch saved for a link |
| `catch copy <url> [title...]` | Save an already-caught link again, as a second copy |
| `catch replace <url>` | Fetch a link again, replacing the saved file |
| `catch dismiss <url>` | Drop an "already saved" question without downloading |
| `catch clear` | Drop finished/error/cancelled rows; files stay |
| `catch status` | Print paths, queue count, binary, version, build |
| `catch paste` | Queue every http(s) link on the Wayland clipboard |
| `catch install-hosts [--host <path>]` | Write native host manifests for every browser |
| `catch install-extension [--dir <path>]` | Update Chromium/Brave/Edge flags files |
| `catch native-host` | Speak Chrome/Firefox Native Messaging on stdio |

### URLs and stdin

A URL argument of `-` means "read it from stdin". The panel uses this form
because `/proc/<pid>/cmdline` is readable by every local user and a signed URL
keeps its token in the query string. `enqueue -` reads one line of
whitespace-separated URLs, so a whole batch stays out of argv. The browser
hand-off does not go through argv at all: the native host receives links as
JSON.

`catch add` blocks for the whole transfer. The panel uses `enqueue`, which
writes the rows and returns; the browser host queues directly and returns the
same way. The drainer starts as many as the parallel setting allows and keeps
the rest queued.

### Add and enqueue

- Only `http(s)` URLs are accepted. A batch reports a bad link and queues the
  rest; a single bad link is an error.
- `--video` forces `yt-dlp`. Watch-style YouTube and Vimeo links route to
  `yt-dlp` automatically, because a plain fetch would save the page HTML.
- `--audio` extracts the best audio stream and is only meaningful with
  `--video`.
- `--sha256` pins the expected digest (64 hex characters). A mismatch is an
  integrity error and the file is not published. Only one URL per `--sha256`.
- `--again` forces a fresh, independent download and clears a pending duplicate
  question. Without it, a link whose file is already in the folder becomes a
  question for the panel instead of a quiet second copy.

### Saved-link questions

Catch keeps a `saved` index that outlives queue rows. Sending a link whose file
is already in the download folder records a question in `state.asks`; the panel
asks whether to open, save another copy, or replace it. The question survives a
panel reload and is announced once on the desktop.

The explicit answers are:

- `catch copy <url> [title...]` — save another copy; the caught file is never
  touched.
- `catch replace <url>` — fetch again and replace the saved file. This is
  destructive and asks in the panel.
- `catch dismiss <url>` — drop the question without downloading.

`catch rename <url> <title...>` changes the row title and the saved file name.
It works after the row is gone; the file is still on disk. It refuses while a
transfer is active.

### Queue control

- **Pause** keeps the row and partial data. A paused entry stays in the queue.
- **Stop** (`cancel`) ends the worker, keeps the row as `cancelled`, and keeps
  partial data for a later resume.
- **Remove** drops the row and deletes its partial `.part` file and segment
  manifest. Without `--delete-file`, a finished file stays. With `--delete-file`,
  Catch also deletes the finished file, but only when it is inside the download
  folder.
- **Resume** re-queues a paused, errored, or stopped entry and starts it again.
  It continues from the partial data and resets the retry budget.
- **Priority** orders queued entries; higher runs first, FIFO inside a level.

### Refresh an expired link

Signed links expire. `catch refresh <id> <new-url>` probes the new address before
reusing partial data:

- When the length and validators prove it is the same file, the partial data is
  kept.
- When it cannot be proven, the command refuses. `--force` resets the partial
  data and fetches from zero.

The panel exposes this as `r` on a failed or stopped row. The new URL is pasted
into the link field, not passed in argv.

### Diagnostics

`catch status` prints the state path, download folder, queue count, binary path,
version, and build stamp.

`catch why <id>` prints the URL, bytes, speed/ETA, attempts and next retry,
partial-file path and size, segment manifest summary, validators, expected
checksum, and an assessment. **It prints the full URL, including any signed query
token. Do not paste its output into an issue or screenshot without redacting
it.**

`catch list --json` prints the raw state file. It is useful for debugging and for
the panel's shape, but it also contains full URLs. Treat it as private.

### Scheduler

The schedule is stored in the widget settings (`schedEnabled`, `schedStart`,
`schedStop`). `drain` refuses to start queued entries while the window is shut
and marks them `sched_hold`; the panel shows that as **Scheduled**. `tick` parks
live transfers when the window closes and releases them when it reopens.

The timer is a user systemd timer:

```sh
catch schedule --install
catch schedule --remove
catch schedule
```

The panel installs or removes it when the schedule is toggled. Check it with:

```sh
systemctl --user status catch-schedule.timer
journalctl --user -u catch-schedule.service
```

The CLI help for `tick` and `schedule` still says "NOT READY"; the enforcement
is implemented in `drain`/`tick` and the panel. Treat the help text as stale
until it is cleaned up.

## Settings

Settings live in the Catch widget entry inside
`~/.config/omarchy/shell.json`. The panel writes them through Omarchy's settings
API, so a change made in the panel is what the backend reads on the next
transfer. The gear icon or `s` opens the panel settings.

| Key | Default | Values / notes |
| --- | --- | --- |
| `downloadDir` | empty | Empty uses `$XDG_DOWNLOAD_DIR/catch`, else `~/Downloads/catch`. `~` expands. |
| `maxParallel` | `3` | `1`, `3`, `5`, `10` |
| `segments` | `4` | `1`, `2`, `4`, `8`; `1` disables segmented downloads |
| `bwMode` | `unlimited` | `unlimited`, `balanced`, `low`; shapes the connection budget |
| `bwCapKb` | `0` | Global cap in KB/s; `0` means no cap |
| `schedEnabled` | `off` | `off` or `on` |
| `schedStart` | `02:00` | `HH:MM`, overnight wrap allowed |
| `schedStop` | `06:00` | `HH:MM` |
| `uiPlacement` | `bar` | `bar`, `top`, `center`, `left`, `right`, `bottom` |
| `catchCommand` | `catch` | Declared in the manifest; the panel and service currently invoke `catch` from `PATH` |

The manifest's schema lists only `bar` for `uiPlacement`; the Catch panel writes
the other placements itself. Keep the manifest and panel in sync when changing
this list.

`CATCH_CATEGORIES=on` sorts finished files into type folders (`video`, `music`,
`documents`, `compressed`, `programs`). It is off by default so a file is where
the panel says it is.

## Environment overrides

All variables are optional. The panel settings are the normal way to change
these values.

| Variable | Default | Meaning |
| --- | --- | --- |
| `CATCH_MAX_PARALLEL` | `3` | Parallel transfers for headless use (1–10) |
| `CATCH_SEGMENTS` | `4` | Segments per eligible file (1, 2, 4, 8; 1 disables) |
| `CATCH_BW_MODE` | `unlimited` | `unlimited`, `balanced`, or `low` |
| `CATCH_BW_CAP_BPS` | none | Global download cap in bytes/second |
| `CATCH_MAX_CONNECTIONS` | parallel × segments, or the traffic mode | Global connection budget (1–64) |
| `CATCH_MAX_PER_HOST` | global budget | Per-host connection ceiling (1–64) |
| `CATCH_SEGMENT_MIN_BYTES` | 33554432 (32 MiB) | Smallest file eligible for segmentation |
| `CATCH_SEGMENT_MIN_PIECE` | 8388608 (8 MiB) | Target segment size |
| `CATCH_CATEGORIES` | off | `on` sorts files into type folders |
| `CATCH_STALL_SECS` | `60` | Body stall timeout before reconnecting |
| `CATCH_RESPONSE_SECS` | `30` | Connect/response-header timeout |
| `CATCH_MAX_ATTEMPTS` | `4` | Connection attempts per transfer |
| `CATCH_VIDEO_IDLE_SECS` | `600` | `yt-dlp` silence before the run is treated as hung |

Internal/test-only variables: `CATCH_NO_SELF_CONFIGURE` (prevents recursive
configuration), `CATCH_HOST_SINGLE_SHOT` (native host exits after one message),
and the `CATCH_PROG`/`CATCH_META`/`CATCH_FILE` markers used between `yt-dlp` and
the backend. Do not document or rely on them as user settings.

## Files and paths

| What | Where |
| --- | --- |
| Widget settings | `~/.config/omarchy/shell.json` |
| Queue, saved index, questions | `$XDG_STATE_HOME/omarchy/catch/state.json` (default `~/.local/state/omarchy/catch/state.json`) |
| Download folder | `downloadDir`, else `$XDG_DOWNLOAD_DIR/catch`, else `~/Downloads/catch` |
| Partial file | `<download folder>/<name>.<id>.part` (or its category folder) |
| Segment manifest | `<download folder>/<name>.<id>.part.meta` |
| Quarantined bad state | `$XDG_STATE_HOME/omarchy/catch/state.json.bad-<seconds>` |
| Native host manifests | Per-browser `NativeMessagingHosts` / `native-messaging-hosts` |
| Scheduler units | `~/.config/systemd/user/catch-schedule.{service,timer}` |
| Backend links | `~/.local/bin/catch`, `~/.local/bin/catch-host` |
| Cleanup helper | `~/.local/bin/catch-uninstall` |

The state directory is owner-only (`0700`) and `state.json` is `0600`. The file
holds full URLs, including signed query strings. Every writer creates it private
at creation, then renames atomically.

`state.json` has `version`, `downloads`, `saved`, and `asks`. Statuses are
`queued`, `downloading`, `paused`, `done`, `error`, and `cancelled`. The saved
index is keyed by URL and records the path, kind, size, and time. Questions in
`asks` are either a saved file or a live transfer.

## Transfer behavior

### Sequential

A plain transfer writes a `.part` file, follows redirects, validates the final
URL scheme, and renames into place only after the bytes are complete. A short or
stalled body is retried; partial data survives a stop, pause, crash, or restart.

### Segmented

A large file is segmented only when the server proves real byte-range support,
identity encoding, and a validator (`ETag` or `Last-Modified`), and the transfer
starts from byte zero. The details are in
[segmented downloads](segmented-downloads.md). A server that does not cooperate
falls back to a fresh sequential transfer; bytes from two representations are
never mixed.

### Video and audio

`yt-dlp` owns video extraction. Catch streams `--newline` progress into the state
file, keeps the process group so Stop reaches FFmpeg, and treats silence longer
than `CATCH_VIDEO_IDLE_SECS` as hung. A pause is not a failure: `yt-dlp` keeps
its partial files and a resume continues. A stopped video keeps its partial data
for a manual retry.

### Retries and errors

A connection failure is retried with backoff. The row records the attempt count
and next retry. A server that answers HTML where a file was expected is named as
a page/login wall rather than a bare HTTP error. Account-bound Google, Gemini,
Drive, and Gmail downloads are left to the browser: Catch has no cookies and
cannot complete a sign-in wall.

## Browser hosts

`catch install-hosts` writes host manifests for every Chromium and Firefox family
it knows, including browsers that are not installed yet. `catch install-extension`
updates existing Chromium/Brave/Edge flags files. Chrome 137+ ignores
`--load-extension`, so Chrome users load the unpacked extension once. Firefox
temporary add-ons must be loaded after each launch. Full details, including
Flatpak limits, are in [browser support](browsers.md).

## Troubleshooting

**`catch` or `catch-host` is not found.** The installer links both into
`~/.local/bin`; make sure that directory is on `PATH`. Re-run `./install.sh` or
`bin/configure`.

**The panel shows stale data.** The shell may be serving cached QML. Run
`omarchy-restart-shell`. If the manifest or installer changed, run `./install.sh`
first.

**A download is stuck at 0 B/s.** Run `catch why <id>`. It shows the partial
data, speed/ETA, attempts, and next retry. A stalled connection is retried; a
server that never sends eventually surfaces an error.

**A link expired.** Copy a fresh address and press `r` on the failed row, or run
`catch refresh <id> <url>`. The backend verifies the new address before reusing
partial data.

**The queue file is corrupt.** Catch moves it aside as
`state.json.bad-<seconds>` and starts clean. The old file is kept for recovery;
do not delete it until you have checked it.

**The scheduler does nothing.** Confirm the window and timer:

```sh
catch schedule
systemctl --user status catch-schedule.timer
journalctl --user -u catch-schedule.service
```

**A browser download still goes to the browser.** Catch leaves the browser
download running when it cannot reach the host, rather than swallowing it.
Check `~/.local/bin/catch-host`, run `catch install-hosts`, and reload the
extension. A Flatpak browser may need its distro package.

**The extension menu does nothing.** Restart the browser after the flags file
changes, check the extensions page, and load the unpacked/temporary extension if
needed. Chrome 137+ never autoloads from the flag.

**After removing Catch, Brave or Chromium reports `Failed to load extension from:
. Manifest file is missing or unreadable`.** The cleanup did not run. Run
`catch-uninstall`, or recover it from a fresh clone. See
[Remove Catch](../README.md#remove-catch).

## Uninstall

Run Catch's cleanup **before** removing the plugin:

```sh
~/.config/omarchy/plugins/io.github.tahadx.catch/bin/uninstall
omarchy plugin remove io.github.tahadx.catch
```

`Setup → Remove Plugin` deletes a git checkout without running plugin code, so
the cleanup script disappears with it. `bin/uninstall` removes the backend
links, native host manifests, Chromium/Brave/Edge flags entries, the `Super+D`
binding, and the scheduler timer. It does not touch the queue or downloaded
files.

The same script is installed as `~/.local/bin/catch-uninstall`, so it survives
Remove Plugin. If the plugin directory is already gone, run:

```sh
catch-uninstall
```

If that helper is missing too, recover it from a fresh clone:

```sh
git clone --depth 1 https://github.com/tahadx/catch
./catch/bin/uninstall
```

Restart the browser afterwards. Your queue and download folder stay.
