#!/bin/bash
# Catch installer: backend binaries, browser host manifests, and an
# optional Super+D panel shortcut. Idempotent; safe to re-run.
# Usage: ./install.sh [--yes]   (--yes answers prompts with yes)
set -euo pipefail

HERE="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
YES=0
[[ "${1:-}" == "--yes" || "${1:-}" == "-y" ]] && YES=1

ask() {
  # ask "question" — true if --yes, the answer is yes/empty, or stdin is
  # closed/unavailable (non-interactive default).
  local prompt="$1"
  (( YES == 1 )) && return 0
  local answer
  if read -r -t 30 -p "$prompt [Y/n] " answer; then
    [[ "$answer" == "" || "$answer" == "y" || "$answer" == "Y" ]]
  else
    return 0
  fi
}

echo "== Catch: backend + browser hosts =="
"$HERE/bin/configure"

echo "== Browsers detected =="
have() { command -v "$1" >/dev/null 2>&1; }
for spec in \
  "chromium:Chromium" \
  "google-chrome:Chrome" "google-chrome-stable:Chrome" \
  "brave:Brave" "brave-browser:Brave" \
  "microsoft-edge:Edge" "microsoft-edge-stable:Edge" \
  "firefox:Firefox" \
  "zen-browser:Zen" \
  "floorp:Floorp" \
  "librewolf:LibreWolf"; do
  bin="${spec%%:*}"
  name="${spec##*:}"
  if have "$bin"; then
    echo "  $name ($bin): host manifest installed"
  fi
done
echo "  Chromium family: extension autoloads via browser flags (restart the browser once)."
echo "  Firefox family: load the extension once per browser (temporary add-on):"
echo "    about:debugging#/runtime/this-firefox -> Load Temporary Add-on"
echo "    -> pick any file in $HERE/extensions/firefox-catch"

echo "== Panel shortcut =="
BINDINGS="$HOME/.config/hypr/bindings.lua"
BIND_LINE="o.bind(\"SUPER + D\", \"Catch downloads\", \"omarchy-shell shell toggle io.github.tahadx.catch '{}'\")"
if [[ -f $BINDINGS ]] && grep -qxF "$BIND_LINE" "$BINDINGS"; then
  echo "  Super+D toggle already present."
else
  if ask "Add the Super+D toggle shortcut to Hyprland bindings?"; then
    cp "$BINDINGS" "$BINDINGS.bak.$(date +%s)"
    printf '\n%s\n' "$BIND_LINE" >>"$BINDINGS"
    hyprctl reload >/dev/null 2>&1 || true
    echo "  Added Super+D (backup beside bindings.lua). Press it to toggle Catch."
  else
    echo "  Skipped. Add it yourself any time with one line in"
    echo "  ~/.config/hypr/bindings.lua, then \`hyprctl reload\`:"
    echo
    echo "    $BIND_LINE"
  fi
fi

echo "Done. Left-click the bar icon (or Super+D) to open Catch."
