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
echo "  Chromium, Brave, Edge: extension autoloads via browser flags (restart the browser once)."
echo "  Google Chrome (137+): load unpacked once from $HERE/extensions/chromium-catch"
echo "  Firefox family: load the extension once per browser (temporary add-on):"
echo "    about:debugging#/runtime/this-firefox -> Load Temporary Add-on"
echo "    -> pick any file in $HERE/extensions/firefox-catch"

echo "== Panel shortcut =="
CONFIG_HOME="${XDG_CONFIG_HOME:-$HOME/.config}"
BINDINGS="$CONFIG_HOME/hypr/bindings.lua"
BIND_LINE="o.bind(\"SUPER + D\", \"Catch downloads\", \"omarchy-shell shell toggle io.github.tahadx.catch '{}'\")"
if [[ -f "$BINDINGS" ]] && grep -qxF -- "$BIND_LINE" "$BINDINGS"; then
  echo "  Super+D toggle already present."
else
  if ask "Add the Super+D toggle shortcut to Hyprland bindings?"; then
    mkdir -p -- "$(dirname -- "$BINDINGS")"
    if [[ -f "$BINDINGS" ]]; then
      cp -- "$BINDINGS" "$BINDINGS.bak.$(date +%s)"
    fi
    # Do not glue the new line onto an existing last line without a newline.
    if [[ -s "$BINDINGS" && -n "$(tail -c 1 -- "$BINDINGS")" ]]; then
      printf '\n' >>"$BINDINGS"
    fi
    printf '\n%s\n' "$BIND_LINE" >>"$BINDINGS"
    if [[ -n "${HYPRLAND_INSTANCE_SIGNATURE:-}" ]] && command -v hyprctl >/dev/null 2>&1; then
      hyprctl reload >/dev/null 2>&1 || true
    fi
    echo "  Added Super+D (backup beside bindings.lua when it existed). Press it to toggle Catch."
  else
    echo "  Skipped. Add it yourself any time with one line in"
    echo "  $BINDINGS, then \`hyprctl reload\`:"
    echo
    echo "    $BIND_LINE"
  fi
fi

echo "Done. Left-click the bar icon (or Super+D) to open Catch."
echo
echo "To remove Catch later, run this before removing the plugin:"
echo "  $HERE/bin/uninstall"
echo "  omarchy plugin remove io.github.tahadx.catch"
echo "Setup → Remove Plugin deletes a git checkout without running that cleanup."
echo "If you remove it first, run: ~/.local/bin/catch-uninstall"
