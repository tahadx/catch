Catch is an Omarchy shell plugin plus a Rust download backend. The queue, the bytes on disk, retries, and finalization live in Rust; the panel is a thin QML view over the CLI and JSON state. This repository is the source of truth for both. An installed plugin under `$HOME/.config/omarchy/plugins/io.github.tahadx.catch` is a clone: develop here, not there.

`AGENTS.md` and `CLAUDE.md` are identical copies for agent discovery. Edit this file, copy it over `AGENTS.md`, and keep the pair byte-identical; CI checks it. A symlink is not an option because Omarchy refuses symlinks inside a plugin tree.

## Source map

| Tree | Role |
| --- | --- |
| `crates/catch/src/main.rs` | CLI commands, add/catch handoff, worker entry points |
| `crates/catch/src/download.rs` | HTTP engine: deadlines, retry/resume, Range and validators, finalize |
| `crates/catch/src/segment.rs` | Segmented downloads: probe, range math, manifest, positioned writes, resume |
| `crates/catch/src/video.rs` | yt-dlp worker, bounded output capture, process-group stop |
| `crates/catch/src/state.rs` | JSON state, locks and claims, filenames, saved index, drain marker |
| `crates/catch/src/native_host.rs` | Chrome/Firefox native messaging loop and batching |
| `crates/catch/src/drain.rs` | Slot scheduling and worker spawning |
| `Panel.qml`, `Service.qml`, `Model.js` | Bar widget, service, and pure JS logic tested by Node |
| `extensions/chromium-catch/`, `extensions/firefox-catch/` | Browser extensions; `background-3.js` must stay identical in both |
| `bin/`, `install.sh` | Configure/uninstall, dist refresh, QML lint, version and release checks |
| `bin/dist/` | Shipped x86_64 binaries, generated and committed |
| `crates/catch/tests/`, `tests/` | End-to-end, model, installer, and benchmark suites |
| `docs/release.md` | Tag-driven release process |

Find Omarchy's packaged shell through `$OMARCHY_PATH` (default `/usr/share/omarchy`). It is read-only: never edit it, and never write machine-specific home directories into this file or into a skill.

Runtime tools: `yt-dlp` and `ffmpeg` for video. Omarchy already provides `wl-paste`/`wl-copy`, `xdg-open`, `systemctl`, and `omarchy-notification-send`.

## Skills

There is no repo-local product skill. Load the matching shared set for the area you touch.

**Omarchy / QML** — panel, service, bar widget, settings:

`omarchy`, `omarchy-plugins`, `qml-quickshell`, `javascript-qml`, `ui-ux`, `hyprland` (bindings only)

**Browser handoff** — extensions, native messaging, permissions:

`omarchy-plugins`, `javascript-qml`, `plugin-security`

**Rust backend** — CLI, HTTP transfers, state, video:

`rust-router` first, then the relevant m-series skill (`m06-error-handling`, `m07-concurrency`, `m10-performance`, `m12-lifecycle`, and `m13-domain-error` cover most Catch work). Use `unsafe-checker` for unsafe or FFI code and `domain-web` for HTTP semantics. `coding-guidelines` applies to style.

**Process / release** — installer, CI, versions, tags:

`software-engineering`, `git-workflow`, `github`

## Routing

**Backend work**
Load the Rust set plus `domain-web` when HTTP behavior is involved. Rust owns the queue, state, bytes, and finalization; the panel must not grow a second download engine. Any behavior change needs a regression test.

**Panel / service work**
Load the Omarchy/QML set. Keep `Model.js` pure and covered by `node tests/model.test.js`; do not move transfer logic out of Rust. Use `omarchy-restart-shell` when QML changes do not hot-reload.

**Browser handoff work**
Load the browser set. Both extension copies are one source of truth: edit one, copy it to the other, and keep the manifests browser-specific. Native host stdout carries protocol frames only.

**Installer / release work**
Load the process set. Exercise temp homes, reruns, and the release consistency check. `bin/dist` is part of the shipped artifact, not a build byproduct.

**Cross-area information**
Working in one area but needing behavior from another loads that area's skills too. Examples: panel work that depends on drain ordering loads the Rust set; backend work that changes host messages loads the browser set.

## Non-negotiables

- Rust is the source of truth for queue state and file bytes. QML reads it through the CLI/JSON; it never invents a transfer, duplicate rule, or completion path.
- Preserve documented duplicate/replace semantics, the saved index, and download-directory rules unless a change is deliberate and documented.
- State writes stay atomic, cross-process, and forward-compatible: unknown JSON keys pass through, malformed state is quarantined, and old fields keep loading.
- Download correctness comes before throughput: monotonic deadlines, bounded retries, no retry of permanent errors, strict `Content-Range`/`206`/`416`/`If-Range` handling, and no splicing different representations.
- Browser hints, URL tails, and `Content-Disposition` names are untrusted. Sanitize to one path segment and truncate on a UTF-8 boundary.
- Native host diagnostics never go to stdout, acknowledgements wait for acceptance, and one download must not acquire two workers.
- After any `crates/` change, run `bin/refresh-dist` and commit both shipped binaries. Never hand-edit `bin/dist`.
- Tests and benchmarks use throwaway `HOME`/XDG directories. Never touch the real home, queue, or download folder from a test.
- Do not make system-wide TCP, DNS, or scheduler changes. Measure before claiming a performance win, using the controlled local server, and never claim an ISP speed increase.

## Verify

Minimum before committing:

```sh
cargo fmt --check -p catch
cargo clippy -p catch --all-targets -- -D warnings
cargo test -p catch
node tests/model.test.js
cmp AGENTS.md CLAUDE.md
bash tests/install.test.sh
bin/check-release
```

For QML or extension changes, also run:

```sh
node --check extensions/chromium-catch/background-3.js
node --check extensions/firefox-catch/background-3.js
cmp extensions/chromium-catch/background-3.js extensions/firefox-catch/background-3.js
bin/lint-qml
omarchy plugin validate .
```

For transfer changes, benchmark before and after:

```sh
python3 tests/bench.py --binary bin/dist/catch --levels 1,3,10
```

Enable the local gate once per clone:

```sh
git config core.hooksPath .githooks
```

It runs the same checks as CI and refuses a stale `bin/dist`.

## Commits

Every commit must have a description body, not just a subject line. Subject: `platform: area: summary` (50 characters or less). Platform comes first: `backend`, `panel`, `browser`, `installer`, `release`, or `root` for repo-wide docs/config. Then the area, then the summary. Body: what was wrong, what the fix does, and how it was verified (commands run, tests added, benchmarks measured). Do not claim a check passed if it was skipped.
