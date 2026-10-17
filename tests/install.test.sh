#!/bin/bash
# Installer/uninstaller regression test. Everything happens under a throwaway
# HOME; the real user configuration and downloads are never touched.
set -euo pipefail

REPO="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
TEST_ROOT="$(mktemp -d "${TMPDIR:-/tmp}/catch-install-test.XXXXXX")"
export TEST_ROOT

cleanup() {
  rm -rf -- "$TEST_ROOT"
}
trap cleanup EXIT

fail() {
  echo "install.test: $*" >&2
  exit 1
}

assert_file() {
  [[ -f "$1" ]] || fail "missing file: $1"
}

assert_no_file() {
  [[ ! -e "$1" && ! -L "$1" ]] || fail "unexpected file: $1"
}

export HOME="$TEST_ROOT/home"
export XDG_CONFIG_HOME="$HOME/.config"
export XDG_STATE_HOME="$HOME/.state"
export XDG_DOWNLOAD_DIR="$HOME/Downloads"
mkdir -p -- "$HOME/.config/hypr" "$HOME/.local/bin" \
  "$HOME/Downloads/catch" "$XDG_STATE_HOME/omarchy/catch" "$TEST_ROOT/bin"

# Keep the test machine's real systemd and compositor out of this run, and
# make it observable if the installer reloads Hyprland outside a session.
cat >"$TEST_ROOT/bin/systemctl" <<'EOF'
#!/bin/sh
printf '%s\n' "$*" >>"${TEST_ROOT:?}/systemctl.log"
exit 0
EOF
cat >"$TEST_ROOT/bin/hyprctl" <<'EOF'
#!/bin/sh
printf '%s\n' "$*" >>"${TEST_ROOT:?}/hyprctl.log"
exit 0
EOF
chmod 755 -- "$TEST_ROOT/bin/systemctl" "$TEST_ROOT/bin/hyprctl"
export PATH="$TEST_ROOT/bin:$PATH"
unset HYPRLAND_INSTANCE_SIGNATURE

BINDINGS="$XDG_CONFIG_HOME/hypr/bindings.lua"
USER_BIND='o.bind("SUPER + Return", "Terminal", "foot")'
# No trailing newline: the installer must not glue its line onto this one.
printf '%s' "$USER_BIND" >"$BINDINGS"

STATE="$XDG_STATE_HOME/omarchy/catch/state.json"
DOWNLOAD="$XDG_DOWNLOAD_DIR/catch/user-file.bin"
printf '%s\n' '{"version":1,"downloads":[],"saved":{},"asks":[]}' >"$STATE"
printf 'user data' >"$DOWNLOAD"

"$REPO/install.sh" --yes >"$TEST_ROOT/install-1.log"

assert_file "$HOME/.local/bin/catch"
assert_file "$HOME/.local/bin/catch-host"
[[ -L "$HOME/.local/bin/catch" ]] || fail "catch is not a symlink"
[[ -L "$HOME/.local/bin/catch-host" ]] || fail "catch-host is not a symlink"
assert_file "$HOME/.config/chromium/NativeMessagingHosts/io.github.tahadx.catch.json"
assert_file "$HOME/.mozilla/native-messaging-hosts/io.github.tahadx.catch.json"
grep -qxF -- "$USER_BIND" "$BINDINGS" || fail "installer changed the user binding"
grep -qxF -- \
  'o.bind("SUPER + D", "Catch downloads", "omarchy-shell shell toggle io.github.tahadx.catch '\''{}'\''")' \
  "$BINDINGS" || fail "installer did not add the Super+D binding"
[[ "$(grep -c 'SUPER + D' "$BINDINGS")" -eq 1 ]] || fail "binding duplicated"
[[ "$(tail -c 1 -- "$BINDINGS" | wc -l)" -eq 1 ]] || fail "bindings file lost its final newline"
assert_file "$BINDINGS.bak."*
assert_no_file "$TEST_ROOT/hyprctl.log"

BACKUPS_BEFORE="$(find "$XDG_CONFIG_HOME/hypr" -maxdepth 1 -name 'bindings.lua.bak.*' | wc -l)"

"$REPO/install.sh" --yes >"$TEST_ROOT/install-2.log"

[[ "$(grep -c 'SUPER + D' "$BINDINGS")" -eq 1 ]] || fail "rerun duplicated the binding"
BACKUPS_AFTER="$(find "$XDG_CONFIG_HOME/hypr" -maxdepth 1 -name 'bindings.lua.bak.*' | wc -l)"
[[ "$BACKUPS_BEFORE" -eq "$BACKUPS_AFTER" ]] || fail "rerun made another backup"
grep -q 'already present' "$TEST_ROOT/install-2.log" || fail "rerun is not quiet about the binding"
assert_no_file "$TEST_ROOT/hyprctl.log"

"$REPO/bin/uninstall"

assert_no_file "$HOME/.local/bin/catch"
assert_no_file "$HOME/.local/bin/catch-host"
assert_no_file "$HOME/.config/chromium/NativeMessagingHosts/io.github.tahadx.catch.json"
assert_no_file "$HOME/.mozilla/native-messaging-hosts/io.github.tahadx.catch.json"
grep -qxF -- "$USER_BIND" "$BINDINGS" || fail "uninstaller damaged the user binding"
if grep -qF 'SUPER + D' "$BINDINGS"; then
  fail "uninstaller left the Catch binding"
fi
cmp -s -- "$STATE" <(printf '%s\n' '{"version":1,"downloads":[],"saved":{},"asks":[]}') \
  || fail "uninstaller changed queue state"
cmp -s -- "$DOWNLOAD" <(printf 'user data') \
  || fail "uninstaller changed downloaded files"
grep -q 'disable --now catch-schedule.timer' "$TEST_ROOT/systemctl.log" \
  || fail "scheduler was not disabled"

# A second uninstall is a no-op, not an error.
"$REPO/bin/uninstall" >"$TEST_ROOT/uninstall-2.log"

# Reinstalling into a HOME without a bindings file must create it instead of
# failing on cp of a missing source.
BACKUPS_BEFORE="$(find "$XDG_CONFIG_HOME/hypr" -maxdepth 1 -name 'bindings.lua.bak.*' | wc -l)"
rm -f -- "$BINDINGS"
"$REPO/install.sh" --yes >"$TEST_ROOT/install-3.log"
assert_file "$BINDINGS"
grep -qxF -- \
  'o.bind("SUPER + D", "Catch downloads", "omarchy-shell shell toggle io.github.tahadx.catch '\''{}'\''")' \
  "$BINDINGS" || fail "installer did not create a missing bindings file"
BACKUPS_AFTER="$(find "$XDG_CONFIG_HOME/hypr" -maxdepth 1 -name 'bindings.lua.bak.*' | wc -l)"
[[ "$BACKUPS_BEFORE" -eq "$BACKUPS_AFTER" ]] \
  || fail "installer made a backup when there was no bindings file"

# Only Catch-shaped symlinks are ours; an unrelated binary with "catch" in
# its target must survive cleanup.
"$REPO/bin/uninstall" >/dev/null
ln -s -- "$TEST_ROOT/my-catch-helper" "$HOME/.local/bin/catch"
"$REPO/bin/uninstall" >/dev/null
[[ -L "$HOME/.local/bin/catch" ]] || fail "uninstaller removed an unrelated symlink"
rm -f -- "$HOME/.local/bin/catch"

echo "install.test: ok"
