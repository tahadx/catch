# Browser support

Catch uses two browser-side pieces:

- A **native messaging host file**, which lets the browser start `catch-host`.
- An **extension**, which sends links to Catch.

The installer writes host files for every supported browser it knows about,
even browsers that are not installed yet. Loading the extension is different
for each browser family.

Both extensions ship inside the plugin:

- `extensions/chromium-catch`
- `extensions/firefox-catch`

## Chromium family

Host files are written for:

- Chromium
- Google Chrome stable, beta, and unstable
- Brave stable, beta, and nightly
- Brave Origin stable, beta, and nightly
- Microsoft Edge stable and dev

Chromium, Brave, and Edge add Catch to an existing flags file, so the
extension loads on the next browser start:

| Browser | Flags file |
| --- | --- |
| Chromium | `chromium-flags.conf` |
| Brave stable | `brave-flags.conf` |
| Brave Origin | `brave-origin-flags.conf` |
| Edge stable | `microsoft-edge-flags.conf` |
| Edge dev | `microsoft-edge-dev-flags.conf` |

Google Chrome ignores `--load-extension` since Chrome 137, so Chrome stable,
beta, and unstable never autoload. Brave beta and nightly have no flags-file
entry; Chromium, Brave, or Edge without an existing flags file stay manual too.
Load the extension once:

1. Open `chrome://extensions`, `brave://extensions`, or `edge://extensions`.
2. Turn on Developer mode.
3. Choose **Load unpacked** and select
   `~/.config/omarchy/plugins/io.github.tahadx.catch/extensions/chromium-catch`.

That copy stays installed in the browser profile until you remove it.

## Firefox family

Host files are written for Firefox, Zen, Floorp, LibreWolf, and Waterfox.
Flatpak builds of Firefox, Zen, and LibreWolf get one too, under
`~/.var/app/…`.

Firefox will not keep an unsigned extension. Load it after each launch:

1. Open `about:debugging#/runtime/this-firefox`.
2. Click **Load Temporary Add-on**.
3. Select `manifest.json` in
   `~/.config/omarchy/plugins/io.github.tahadx.catch/extensions/firefox-catch`.

Quit the browser and the add-on is gone. Load it again next time. Do this once
per browser: Firefox and Zen are separate.

A Flatpak browser can still refuse to run `catch-host`, because that program
sits outside the sandbox. Install the distro package of that browser when the
menu does nothing.

## A browser you install later

The installer does not rescan for browsers added after setup. For one installed
later:

```bash
catch install-hosts
catch install-extension
```

`install-hosts` rewrites every host file in the lists above.
`install-extension` updates the Chromium, Brave, and Edge flags files that
already exist. Restart that browser. Google Chrome ignores the flag, so load it
unpacked; Firefox-family still needs the temporary add-on after every launch.

## Other browsers

Vivaldi, Opera, and other forks are not given a host file. Their downloads stay
in that browser.

## Troubleshooting

**After removing Catch, Brave or Chromium reports `Failed to load extension
from: . Manifest file is missing or unreadable`.** Catch was removed without
running its cleanup. `Setup → Remove Plugin` deletes a git checkout without
running plugin code, so the old path stays in `brave-flags.conf` or
`chromium-flags.conf`. Chromium hides the missing path, which is why the error
shows only `.`. Run:

```bash
catch-uninstall
```

That helper is installed next to `catch` and survives plugin removal. If it is
missing, recover it from a fresh clone:

```bash
git clone --depth 1 https://github.com/tahadx/catch
./catch/bin/uninstall
```

Restart the browser afterward. See [Remove Catch](../README.md#remove-catch)
for the normal removal order. If Catch was loaded manually rather than through
the flags file, remove it once from `brave://extensions` or
`chrome://extensions`.

**The Catch menu is missing.** The extension is not loaded, or the browser
started before its flags file was written. Restart the browser, check the
extensions page, and load the unpacked or temporary extension if needed.

**The menu does nothing, or the file still downloads in the browser.** Catch
leaves the browser download running when it cannot reach the native host, so
nothing is swallowed silently. Make sure `~/.local/bin/catch-host` exists and
is executable, then run `catch install-hosts` and reload the extension. A
Flatpak browser may need its distro package instead.

**Chrome ignores the flag.** Chrome 137 and newer no longer accept
`--load-extension`. Load the unpacked extension manually.

**The Firefox add-on disappeared after a restart.** Temporary add-ons do not
survive a browser restart. Load it again from `about:debugging`.

**`catch` or `catch-host` is not found.** The installer links both into
`~/.local/bin`; make sure that directory is on your `PATH`. If the links are
missing, run the installer again:

```bash
~/.config/omarchy/plugins/io.github.tahadx.catch/install.sh
```
