# CLAUDE.md

Catch is an Omarchy bar plugin for downloads (`io.github.tahadx.catch`): a QML
bar widget and panel, a Rust backend in `crates/catch`, and browser extensions
for the Chromium and Firefox families. The panel queues, pauses, resumes, stops,
and removes downloads; the backend owns every transfer and the state file; the
extensions hand links over through a native-messaging host.

This repo is the plugin root. `omarchy plugin add` clones it directly into
`~/.config/omarchy/plugins/io.github.tahadx.catch/`, so `manifest.json`,
`Panel.qml`, `Service.qml`, `CatchPanel.qml`, `Model.js`, `install.sh`,
`LICENSE`, `README.md`, `preview.jpg`, and `bin/` must stay at the repo root.

## Read these first

| File | What it is | When to read it |
| --- | --- | --- |
| `docs/design.md` | **The design system.** Tokens, layout, panel anatomy, motion, copy, interaction rules. | Before any panel or visual change |
| `docs/reference.md` | CLI, settings, state files, paths, troubleshooting, uninstall. | Before changing backend behavior or settings |
| `docs/development.md` | Build, test, shipped binaries, hooks, release tooling. | Before changing `crates/`, `bin/`, or CI |
| `docs/browsers.md` | Native hosts, extension loading, browser-family quirks. | Before changing extensions or install behavior |
| `docs/segmented-downloads.md` | Range probing, manifests, resume, work stealing. | Before changing `segment.rs` or download recovery |
| `README.md` | User-facing features, shortcuts, install/update, marketplace copy. | Before changing a user-visible feature |
| `manifest.json` | Plugin id, kinds, settings schema, defaults. | Before adding or changing a setting |

## Omarchy skills

Load only the skill matching your current task before touching code (do not load
all of them). If your agent has no skill loader, read
`~/.local/share/octomarchy/skills/<name>/SKILL.md` if present locally.

| Area | Skill |
| --- | --- |
| Plugin manifest, kinds, entry points, marketplace | `omarchy-plugins` |
| Omarchy CLI, themes, bar layout, user config | `omarchy-system` |
| QML, Quickshell, bar widgets, services | `qml-quickshell` |
| Panel and bar UI, spacing, states, motion | `ui-ux` |
| Hyprland binds, window rules, monitors | `hyprland` |
| JavaScript modules next to QML | `javascript-qml` |
| Marketplace security review | `plugin-security` |
| GitHub, PRs, marketplace issue workflow | `github` |
| Branches, commits, plugin updates | `git-workflow` |
| Small diffs, validation, definition of done | `software-engineering` |

## Architecture: an Omarchy bar plugin

Catch runs **inside `omarchy-shell`**, Omarchy's long-lived Quickshell process,
as the plugin `io.github.tahadx.catch`. It has two entry points: `Panel.qml`
(kind `bar-widget`) and `Service.qml` (kind `service`). `Panel.qml` extends
`qs.Ui`'s `Panel`; `CatchPanel.qml` is the layer-shell host adapted from
Omarchy's `qs.Ui` `KeyboardPanel`. The shell owns the window, summon/hide, and
key handling; the panel is not an app of its own.

QML and the backend talk over **process boundaries**. The panel runs `catch`
commands through Quickshell `Process`; `Service.qml` watches the state file and
runs `catch drain` when the *shape* of the queue changes, not on every progress
write. The CLI owns the queue; QML only reads state and asks the CLI to act.

The browser side is separate: the extension sends a native message, the browser
spawns `catch-host`, and the host calls the same backend functions directly. It
never shells out with a URL.

```text
bar widget / panel    Panel.qml + CatchPanel.qml + qs.Ui Panel
        │  Quickshell Process: catch <command>   (URLs over stdin, never argv)
        ▼
~/.local/bin/catch -> plugin bin/dist/catch   (Rust backend, crates/catch)
        │  state file + worker processes
        ▼
reqwest / yt-dlp / ffmpeg   (plain transfers, video extraction)
        │
        ├── config   ~/.config/omarchy/shell.json (widget settings)
        ├── runtime  ~/.local/state/omarchy/catch/state.json (queue + saved index)
        └── files    ~/Downloads/catch (or the configured folder)

browser extension -> native messaging -> ~/.local/bin/catch-host
```

**Keep dependencies near zero:** QML, plain JS, and the shipped Rust backend,
using only what Omarchy already ships (Quickshell, Qt, bash, `yt-dlp`,
`wl-clipboard`, `xdg-utils`, systemd) plus the crates already in
`crates/catch/Cargo.toml`. No new QML import, Node runtime, npm package, native
module, or workspace crate without asking first.

To learn the host, read the shell's own code in `$OMARCHY_PATH/shell/`
(`/usr/share/omarchy/shell`):

- `shell.qml` — plugin loading, `summon`/`hide`/`toggle`, and the scoped `shell`
  API
- `Ui/Panel.qml`, `Ui/PanelHero.qml`, `Ui/PanelActionButton.qml`,
  `Ui/PanelController.qml` — the base panel Catch extends; read these before
  changing layout or focus behavior
- `Ui/KeyboardPanel.qml` — the window mechanics `CatchPanel.qml` is adapted
  from; keep the two in sync when Omarchy changes the host
- `plugins/panels/`, `plugins/bar/` — small plugins built the same way, one with
  a bar widget and a panel
- `Commons/{Style,Color,Util}.qml` — tokens; use these instead of a private
  palette
- `default/agents/skills/omarchy/plugins.md` — plugin safety rules; use
  `textFormat: Text.PlainText` for any dynamic text

Rules that follow from this:

- **State & paths:** widget settings live in `~/.config/omarchy/shell.json`;
  the queue and saved index live in
  `$XDG_STATE_HOME/omarchy/catch/state.json` (or
  `~/.local/state/omarchy/catch/state.json`). Partial downloads live beside
  their destination.
- **One owner:** only the backend writes `state.json`, always atomically and
  owner-only (`0700` directory, `0600` file). QML must never write it.
- **Never block the shell:** QML must survive a backend error. Parse
  defensively, show the row error or `actionError`, and never wait
  synchronously on a command. Use `enqueue` from the panel, not blocking `add`.
- **Static artifact, real processes:** `bin/dist/catch` and
  `bin/dist/catch-host` are committed release binaries built by
  `bin/refresh-dist`. Never hand-edit them. `bin/check-release` verifies their
  version and CPU architecture.
- **The panel is the interface:** no separate window, no browser UI, no
  notification-only workflow. Desktop notifications are best-effort feedback;
  the panel and `catch why` are the source of truth.

## Repo layout

```text
manifest.json            plugin registration, kinds, settings schema
Panel.qml                bar widget, queue panel, settings, confirmations
CatchPanel.qml           layer-shell host (placement, motion, dismissal)
Service.qml, Model.js    shell service and pure queue helpers
install.sh               backend setup, browser hosts, optional Super+D bind
crates/catch/src/        Rust backend (main.rs, state.rs, download.rs, segment.rs, video.rs, native_host.rs, install.rs, schedule.rs, conns.rs, drain.rs, throttle.rs)
crates/catch/tests/      end-to-end reliability suite and local HTTP server
extensions/              chromium-catch and firefox-catch (identical background-3.js)
bin/                     configure, setup, refresh-dist, check-release, stamp-version, version, lint-qml, uninstall, render-icons
docs/                    design, reference, development, browsers, segmented downloads, release, bindings
tests/                   model.test.js, install.test.sh, bench.py
assets/, preview.jpg     marketplace assets (preview.jpg shows the panel only)
```

## Ground rules & hard constraints

1. **Theme is the brand.** Use `Commons.Color.*`, `Style.*`, `Border.*`, and
   `Util.alpha()`. Never add a private palette or hardcoded UI colors.
2. **One scroll area, stable order.** The panel body is a single `Flickable`
   holding the header, link input, section head, rows, foot, and settings. Do
   not add a nested scroll area or a second row for metadata. A dynamic value
   must not move the scroll origin or reorder the frame.
3. **Panel, manifest, backend, and extensions stay in sync.** Every new setting
   needs a `manifest.json` schema entry, a `Panel.qml` row, and backend support
   in the same change. Never render a UI row the backend cannot honor. A change
   to a native message field needs `native_host.rs` and both extension copies.
4. **The state file is the contract.** QML reads it; the backend writes it.
   Keep `version`, `downloads`, `saved`, and `asks` backward-compatible. Unknown
   fields are preserved where the backend rewrites records.
5. **URLs are private.** A signed URL carries its token in the query string.
   Never put a user URL in argv, a shell command, a log, or a commit. The panel
   sends links over stdin; the native host receives them as JSON. `/proc` is
   readable by other local users.
6. **No shell interpolation.** Never interpolate user text into `sh -c` or a
   shell string. Pass paths, titles, and prompts through discrete `argv` or
   stdin. Reject empty, `NUL`, and newline values whenever a path is expected.
7. **Browser hand-off is never silent.** If Catch cannot reach the host, the
   browser download must continue. Do not swallow a download the backend cannot
   take. Leave account-bound Google/Gemini/Drive/Gmail downloads to the browser:
   Catch has no cookies and cannot complete a sign-in wall.
8. **Zero new heavy dependencies.** Keep the backend self-contained. Do not add
   a workspace crate, an async runtime change, or an external runtime
   dependency without asking first.
9. **No symlinks anywhere in the plugin tree.** `omarchy plugin validate .`
   rejects symlinks inside the plugin (`AGENTS.md` must remain a plain file).
   The installer may create links in `~/.local/bin`; the repo must not.
10. **Never edit installed files.** Do not touch `/usr/share/omarchy` or
    `~/.config/omarchy/plugins/io.github.tahadx.catch/` directly. Edit the
    checkout and run `./install.sh`.
11. **Git & push discipline.** Keep diffs small. Never push, force-push, or
    rewrite git history unless the user explicitly asks.
12. **Opening Catch shows the queue.** A pending duplicate question is a
    non-modal banner, never the first screen. Only a question that arrives while
    the panel is already open may open the choice view. Do not make `open()`
    replay `state.asks`.
13. **Sync docs in the same commit.** Update `docs/design.md` for panel/UI
    changes, `docs/reference.md` + `README.md` for CLI, config, or user-facing
    behavior changes, `docs/browsers.md` for extension/host changes, and
    `docs/segmented-downloads.md` for range/resume changes. If docs and code
    disagree, fix both.

## Security & privacy

- **State permissions:** preserve `0700` on the state directory and `0600` on
  `state.json`. Every writer goes through `ensure_state_dir` / `write_private`.
  `state.json` contains full URLs, including signed query strings.
- **Path validation:** keep downloads inside the configured folder. Use
  `resolve_inside` / the `safeRelativePath` rules for saved paths; reject empty,
  `NUL`, newline, and traversal values.
- **Safe QML text:** always use `Text.PlainText` for dynamic or user-controlled
  strings; never `Text.RichText`.
- **Native messaging:** the host must stay framed and bounded. A bad body is
  answered and the loop continues; a bad length stops the loop rather than
  misparsing garbage. Keep `MAX_FRAME` and line limits.
- **Network:** the only requests are the downloads the user asked for and the
  `yt-dlp` run for a video link. No telemetry, analytics, update check, or
  credential handling.
- **Log & repo hygiene:** log actions, never payloads, URLs with tokens, or
  environment dumps. The explicit `catch why` diagnostic prints the full URL, so
  never paste its output into an issue or screenshot without redacting it. Treat
  `state.json`, partial files, downloads, and local notes as private; never
  commit them.
- **Extensions:** keep the two `background-3.js` files byte-identical. CI
  checks them with `cmp`. Do not add remote code, analytics, or broad host
  permissions.

## Known pitfalls

- **Stale `bin/dist`:** the shell service runs `catch drain` on every queue
  change. A stale binary can rewrite `state.json` in an old schema, which has
  caused title loss and duplicate rows. Run `bin/refresh-dist` before committing
  any `crates/` change; the pre-push hook also rebuilds and fails if the bytes
  changed.
- **Two extension copies:** `extensions/chromium-catch/background-3.js` and
  `extensions/firefox-catch/background-3.js` are one source of truth. Edit one,
  copy it to the other, and keep them identical.
- **Version is tag-derived:** `bin/version` resolves the version; `bin/stamp-version`
  writes it into `manifest.json`, the crate, and both extension manifests. The
  release workflow re-stamps from the tag and fails if the tag did not already
  carry it.
- **Chrome 137+:** Chrome ignores `--load-extension`; Chromium, Brave, and Edge
  still autoload from their flags files. Do not promise Chrome autoload.
- **Flatpak browsers:** a Flatpak browser may not be able to spawn
  `~/.local/bin/catch-host`. The distro package is the supported path.
- **QML caching:** opening the panel alone may serve cached QML. Run
  `omarchy-restart-shell` (or `./install.sh` when manifest/installer changes)
  before manual testing.
- **PATH shadowing:** `catch` on `PATH` may point at the installed plugin
  binary rather than your local build. Verify with `command -v catch` and
  `catch status`.
- **Single live desktop:** `./install.sh`, `omarchy theme set`, and
  `omarchy-restart-shell` mutate the live desktop session. Never run them from
  two checkouts simultaneously.
- **Blocking `add`:** `catch add` blocks for the whole transfer. The panel and
  browser hand-off must use `enqueue`; a caller that serializes commands would
  stall behind the download.
- **Scheduler help text:** `catch tick` and `catch schedule` still describe
  themselves as "NOT READY" in the CLI help. The window is enforced by
  `drain`/`tick` today; fix the help text the next time `schedule.rs` is
  touched.
- **`catchCommand` setting:** the manifest declares a `catchCommand` key, but
  the panel and service currently invoke `catch` from `PATH`. Do not claim the
  key changes the command until it is wired through.

## Verification & Definition of Done

A change is done only when all applicable checks below pass (these mirror CI and
the pre-push hook):

```sh
# 1. Rust checks
cargo fmt --check -p catch
cargo clippy -p catch --all-targets -- -D warnings
cargo test -p catch --quiet

# 2. QML, model, extensions, installer
node tests/model.test.js
node --check extensions/chromium-catch/background-3.js
node --check extensions/firefox-catch/background-3.js
cmp extensions/chromium-catch/background-3.js extensions/firefox-catch/background-3.js
bin/lint-qml
bash tests/install.test.sh

# 3. Shell lint (when shell scripts change)
shellcheck -x install.sh \
  bin/configure bin/uninstall bin/refresh-dist bin/version \
  bin/stamp-version bin/setup bin/check-release bin/lint-qml \
  tests/install.test.sh

# 4. Shipped binaries and release consistency
bin/check-release

# 5. Plugin contract (when Omarchy is available)
omarchy plugin validate .
```

If `crates/`, `Cargo.toml`, or `Cargo.lock` changed, rebuild the shipped
binaries with `bin/refresh-dist` and commit `bin/dist/catch` and
`bin/dist/catch-host` in the same commit.

Tests must stay off the live desktop. The reliability suite starts its own local
HTTP server and runs `catch` against a throwaway `HOME`; `tests/install.test.sh`
does the same for the installer and uninstaller. `bin/lint-qml` is a parse check
only. Never let a test touch the real queue, real downloads, or a real browser
profile.

### Live desktop testing (when UI or shell integration changes)

```sh
# Inspect backend and queue state
catch status
catch list
catch why <id>

# Open the panel the way the installer's bind does
omarchy-shell shell toggle io.github.tahadx.catch '{}'
grim /tmp/catch-panel.png
```

- **Surface coverage:** test the queue, empty state, conflict question, settings,
  keyboard shortcuts, and the replace/delete confirmations when modifying shared
  row or key code.
- **Synthetic input:** only send keys with `wtype` after confirming the panel has
  focus; early keystrokes leak into the underlying window.
- **Debugging:** inspect `~/.local/state/omarchy/catch/state.json` and run
  `catch why <id>` when a transfer misbehaves. The scheduler's timer logs go to
  the user journal (`journalctl --user -u catch-schedule.service`).
- **Final report:** always list files changed, commands run, verification
  evidence, and any deferred work.

## Commits

Every commit must have a description body, not just a subject line.

Subject: `type(area): summary` (50 characters or less). `type` is one of `feat`,
`fix`, `docs`, `refactor`, `test`, `chore`, `build`. `area` is the part touched:
`panel`, `backend`, `browser`, `extensions`, `install`, `dist`, `docs`, `ci`,
`release`, `marketplace`. Then the summary. Body: what was wrong, what the fix
does, and how it was verified (build, tests, `omarchy plugin validate .`, live
panel check).

## Release & update flow

- **User update:** `omarchy plugin update io.github.tahadx.catch`, then restart
  the browser to pick up extension changes. There is no separate `catch update`
  command.
- **Versioning:** `bin/version` derives the version from the git tag;
  `bin/stamp-version <semver>` writes it into `manifest.json`,
  `crates/catch/Cargo.toml`, and both extension manifests.
- **Cutting a release** (steps 1–4 always; step 5 only when the user asks to
  publish):
  1. `bin/stamp-version <semver>`
  2. `bin/refresh-dist`
  3. `bin/check-release` — verifies semver, plugin id/schema, entry points, crate
     and extension versions, and ELF validity
  4. Commit and tag `v<semver>`
  5. Push the tag `v<semver>` only when explicitly asked; CI re-stamps from the
     tag and fails on any mismatch
- **Full release details:** `docs/release.md`.

## Marketplace

- Required at the repo root: `LICENSE`, `README.md`, `manifest.json`,
  `preview.jpg`.
- `preview.jpg` shows the panel only, not the full desktop.
- `omarchy plugin validate .` must pass, and the tree may hold no symlinks.
- No secrets, logs, state files, downloads, or local config in the committed
  tree.
- The plugin id is `io.github.tahadx.catch`; keep it stable.

## Status

- **Shipped:** browser hand-off, link lists, video/audio through `yt-dlp`,
  resumable queue, parallel transfers, segmented downloads with resume
  checkpoints, pause/stop/remove, saved-link questions, settings (folder,
  transfers, bandwidth, placement, schedule), panel placements, Super+D bind,
  and the cleanup helper.
- **Open:** scheduler CLI help text, the unused `catchCommand` manifest key, and
  marketplace listing/verification.
