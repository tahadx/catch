# Hyprland bindings

Catch does not need a Hyprland binding. The bar widget opens the panel with a
left click, and the download folder opens with a right click. The installer can
add one optional shortcut for opening the panel.

## Add it with the installer

`./install.sh` asks before writing the binding. Answer `y` to add it, `n` to
skip. `--yes` (or `-y`) answers yes for a non-interactive install:

```sh
./install.sh --yes
```

The installer appends this exact line to
`~/.config/hypr/bindings.lua`:

```lua
o.bind("SUPER + D", "Catch downloads", "omarchy-shell shell toggle io.github.tahadx.catch '{}'")
```

It backs the file up first when the file exists, preserves every other line, and
does not glue the new line onto a last line without a newline. It reloads
Hyprland only when a live session is present. Re-running the installer does not
add the line twice.

## Add it by hand

Append the line above to `~/.config/hypr/bindings.lua`, then run:

```sh
hyprctl reload
```

The binding uses key names, so it follows the active keyboard layout.

## Remove it

`bin/uninstall` removes exactly that line and leaves the rest of the file byte
for byte. To remove it by hand, delete the line and run `hyprctl reload` again.

The shortcut is only a convenience. Closing the panel with `q` or `Esc` does not
stop a download; use the row's stop action or `x`.
