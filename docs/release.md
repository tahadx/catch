# Release

Push a version tag. GitHub Actions builds the release binaries, writes the
changelog, and publishes the release.

**The tag is the only version source.** Nothing carries a hand-kept version:
`manifest.json` (what Omarchy shows for the plugin), the crate version (what
`catch --version` and `catch status` report), the browser extensions' version
(what `chrome://extensions` and `about:addons` show) and the release assets are
all derived from the tag by `bin/stamp-version`.

## Make a release

1. Make sure `main` holds the commits you want to ship.
2. Stamp the version and rebuild the shipped backend. Pass the version
   explicitly: this commit has to be made *before* the tag exists, so git
   cannot resolve it yet.

   ```sh
   bin/stamp-version 0.1.0    # manifest.json + the crate + both extensions
   bin/refresh-dist           # the binary now reports that version
   git commit -am "Release 0.1.0"
   ```

3. Create an annotated tag on that commit. The tag body is the user-facing
   highlights: it opens `What's Changed`, above the full commit list the
   workflow always keeps.

   ```sh
   git tag -a v0.1.0 -m "catch 0.1.0" -m "First release: ..."

   git push origin v0.1.0
   ```

4. Follow the `release` workflow in the Actions tab.

## What the workflow does

- Stamps the tag's version into `manifest.json`, the crate, and both browser
  extensions, then **fails if the tag did not already carry it**. `omarchy
  plugin add` installs from git and reads `manifest.json`, so a tag whose files
  disagree would report the wrong version to everyone who installs it. To fix a
  tag that trips this, run `bin/stamp-version && bin/refresh-dist`, commit, and
  move the tag.
- Builds `cargo build --release -p catch`. Refuses to publish a missing or
  non-executable binary, or one whose `catch status` reports a version other
  than the tag.
- Stages `catch-<version>-linux-x86_64`, `catch-host-<version>-linux-x86_64`,
  and `checksums.txt`.
- Collects the commits between the previous tag and the new tag.
- Prepends the annotated tag body as the user-facing highlights above the
  commit list. A lightweight tag ships commits only.
- Links each commit to its author on GitHub when the commit email belongs to
  an account. Merge commits are skipped.
- A tag with a hyphen is a prerelease, for example `v0.2.0-beta.1`.
- A stable release compares against the previous stable tag, skipping
  prerelease tags.

## How a version is resolved

`bin/version` decides, in order:

1. `$CATCH_VERSION` — what a release build is told to call itself.
2. The tag at `HEAD` — a release: exactly the tag, without the `v`.
3. The nearest tag plus the commit count — a dev build, for example
   `0.1.0+4`, so a binary still says which release it will become.
4. `0.0.0-dev` — no git, no tag.

The result is always valid semver, because the same string goes into the
crate manifest and cargo refuses anything else. It carries no dirty marker:
it is written into committed files and `Cargo.lock`, where churn on every
uncommitted change would be noise — an uncommitted tree already shows up in
`catch status` as a dirty build.

Cutting a release skips that resolution and names the version outright
(`bin/stamp-version 0.1.0`), because the tag that will confirm it does not
exist yet when the stamping commit is made.

## Rebuild a release

Run the `release` workflow by hand and enter a tag that already exists. The
workflow replaces the assets and refreshes the notes of that release. The
pipeline lives in [`.github/workflows/release.yml`](../.github/workflows/release.yml).