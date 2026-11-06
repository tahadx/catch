# Development

Catch is a Rust backend plus an Omarchy QML plugin and two browser extensions.
The plugin ships prebuilt backend binaries, so a normal install does not need
Rust or Node.

## Layout

- `Cargo.toml` and `crates/catch/` — the backend, `catch` and `catch-host`.
- `Panel.qml`, `CatchPanel.qml`, `Service.qml`, and `Model.js` — the bar widget,
  layer-shell host, background service, and pure JavaScript helpers.
- `extensions/chromium-catch/` and `extensions/firefox-catch/` — browser
  extensions. Their `background-3.js` files are byte-identical; CI checks them
  with `cmp`.
- `bin/dist/catch` and `bin/dist/catch-host` — the committed release binaries.
- `bin/configure`, `bin/setup`, `bin/refresh-dist`, `bin/stamp-version`,
  `bin/version`, `bin/check-release`, `bin/lint-qml`, `bin/uninstall`, and
  `bin/render-icons` — install and release tooling.
- `tests/model.test.js`, `tests/install.test.sh`, and
  `crates/catch/tests/reliability.rs` — model, installer, and end-to-end tests.
- `docs/` — design, reference, development, browser, segmented-download, release,
  and binding guides.

## Build and test

The backend uses stable Rust. Node is needed for the model and extension checks;
`qmllint` is needed for the QML syntax check; `shellcheck` is needed when shell
scripts change.

```sh
cargo fmt --check -p catch
cargo clippy -p catch --all-targets -- -D warnings
cargo test -p catch --quiet

node tests/model.test.js
node --check extensions/chromium-catch/background-3.js
node --check extensions/firefox-catch/background-3.js
cmp extensions/chromium-catch/background-3.js extensions/firefox-catch/background-3.js

bin/lint-qml
bash tests/install.test.sh
bin/check-release
```

`bin/lint-qml` is a parse-level check. CI has no Omarchy `qs.*` modules, so
unresolved-import warnings are expected there. Set `LINT_QML_VERBOSE=1` to print
every diagnostic on success too.

The reliability suite starts its own local HTTP server and runs real `catch`
processes against a throwaway `HOME`. `tests/install.test.sh` does the same for
the installer and uninstaller. Neither touches the real queue, real downloads,
or a real browser profile. Keep that isolation in any new test.

`tests/bench.py` is a manual benchmark, not part of CI.

## Local hooks

Enable the pre-push gate once per clone:

```sh
git config core.hooksPath .githooks
```

`pre-push` mirrors the CI checks and then proves `bin/dist` is current: it
records the binary hashes, runs `bin/refresh-dist`, and fails if the hashes
changed. If it fails, commit the rebuilt `bin/dist/catch` and
`bin/dist/catch-host` and push again.

## Shipped backend

`bin/dist` holds the binaries users run. Rebuild them from this tree with:

```sh
bin/refresh-dist
```

That runs `cargo build --release -p catch --bins`, copies both binaries into
`bin/dist/`, strips them, and prints the build stamp. Run it before committing
any change under `crates/`, `Cargo.toml`, or `Cargo.lock`. The shell service
calls `catch drain` on queue changes, and a stale binary can rewrite
`state.json` in an old schema.

`bin/check-release` verifies:

- `manifest.json` is valid, has an id, `schemaVersion` 1, and existing entry
  points;
- the manifest, crate, and both extension manifests carry the same version;
- both extension versions are valid Chrome extension versions and no
  `version_name` overrides them;
- both shipped binaries are executable ELF files for this machine's CPU;
- `bin/dist/catch --version` reports the manifest version.

Never hand-edit a shipped binary. If a binary is wrong, fix the source and run
`bin/refresh-dist`.

## Extensions

The Chromium and Firefox extensions share one background script. Edit
`extensions/chromium-catch/background-3.js`, copy it to
`extensions/firefox-catch/background-3.js`, and verify:

```sh
cmp extensions/chromium-catch/background-3.js \
  extensions/firefox-catch/background-3.js
```

The two extension manifests are currently identical: both carry the same pinned
`key`, permissions, commands, and background entry. Keep them in sync when a
shared field changes. The Firefox native-host template uses `GECKO_ID` from
`crates/catch/src/install.rs`; keep that value aligned with the extension's
Firefox id if one is declared in the manifest.

`bin/configure` writes native-messaging host manifests for every browser family
it knows. `catch install-hosts` rewrites them; `catch install-extension` updates
the Chromium/Brave/Edge flags files that already exist. See
[browser support](browsers.md) for loading and troubleshooting.

## Development install

Two paths exist:

```sh
# Build a debug backend, link it into ~/.local/bin, write host files,
# render icons, and print the manual extension steps.
bin/setup
```

```sh
# Install the checkout the way a user gets it: configure the shipped
# binaries, browser hosts, cleanup helper, and optional Super+D bind.
./install.sh
```

`./install.sh` is the path to use when testing manifest, installer, or shipped
binary changes. It is idempotent and safe to re-run. After QML changes, restart
the shell so the loaded panel is replaced:

```sh
omarchy-restart-shell
```

## Release

The tag is the only version source. The full flow is in [release](release.md).

```sh
bin/stamp-version 0.2.0
bin/refresh-dist
bin/check-release
git commit -am "Release 0.2.0"
git tag -a v0.2.0 -m "catch 0.2.0"
git push origin v0.2.0
```

The release workflow re-stamps from the tag, refuses to publish if the tag did
not already carry the version, builds both binaries, verifies their reported
version, stages `catch-<version>-linux-x86_64`,
`catch-host-<version>-linux-x86_64`, and `checksums.txt`, and publishes the
release notes.

## CI

`.github/workflows/build.yml` runs formatting, `cargo check`, clippy, tests, the
model test, extension checks, shellcheck, the installer test, QML syntax,
release consistency, the plugin contract, and a dependency audit. Keep local
checks aligned with it; if CI changes, update this file and
`CLAUDE.md` in the same commit.
