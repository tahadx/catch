# Catch design system

Catch is a download manager that happens to live in a bar. The surface is
monochrome, dense, and quiet, with the theme accent used as a signal: live
progress, the current row, the primary action, or a finished download. Nothing
is decorated for its own sake. Hierarchy comes from spacing, type, hairlines,
and short motion.

This is the shared reference for Catch's interface. It describes the system
that ships today; `Panel.qml` and `CatchPanel.qml` are the reference
implementation. When the code and this guide drift, update both in the same
change.

The system is built on Omarchy's tokens in `qs.Commons.Style` and
`qs.Commons.Color`. Catch owns no private palette, so one `omarchy theme set`
should restyle the entire plugin.

---

## 1. How to use this document

### Source of truth

| Concern | File |
| :--- | :--- |
| Bar widget, queue panel, settings, confirmations | `Panel.qml` |
| Layer-shell host, placement, motion, dismissal | `CatchPanel.qml` |
| Queue shaping and pure helpers | `Model.js` |
| Background drain service | `Service.qml` |
| Plugin registration and settings schema | `manifest.json` |
| Deployment and the optional bind | `install.sh` |

### The one rule

If a visual change needs a new color, a new radius, a new font size, or a new
gap that is not already a token, stop and check whether the hierarchy can be
fixed with an existing token instead. The answer is usually yes.

---

## 2. Principles

1. **Theme is the brand.** Color comes from the active Omarchy theme. Catch
   contributes layout, rhythm, and restraint, never hues.
2. **Accent is a signal.** Accent marks live transfer, the current row, the
   primary action, and completion. It may appear in several roles at once, but
   never twice for the same role, and never as decoration.
3. **One scroll area, stable order.** The panel body is one `Flickable` holding
   the header, link input, section head, rows, foot, and settings. Do not nest a
   scroll area or a second list. A dynamic value must not move the scroll origin
   or reorder the frame.
4. **Density is respect.** A download manager is a tool. Show more, scroll less,
   and use `Style.space()` / `Style.spacing.*` for every gap.
5. **Motion explains.** Transitions are short, one-shot, and quiet. Motion never
   delays an action. Progress glides; the panel does not bounce.
6. **Keyboard first, one cursor.** Every surface is reachable and operable
   without a pointer. Hover and keyboard share one visual cursor.
7. **Flat, not boxy.** Fill and hairlines create depth. Shadows belong only to
   floating layers, never to rows or buttons.
8. **No layout shift.** Dynamic values get reserved space. A changing byte
   count, badge, or error never moves the title, the actions, or the scroll
   origin.
9. **Destruction is explicit.** A destructive action stays neutral until it
   arms, then relabels itself and warns. Urgent color is the warning, not the
   default.
10. **Remove before adding.** If a new feature needs a new surface, delete an
    existing one first or prove it belongs in the current hierarchy.

---

## 3. Tokens & Color

Use these. Do not introduce numeric literals for color, radius, spacing, or type
in layout code.

### Core tokens

| Purpose | Token |
| :--- | :--- |
| Spacing, padding, gaps, sizes | `Style.space(n)` |
| Fractional spacing | `Style.spaceReal(n)` |
| Named spacing roles | `Style.spacing.xs`, `sm`, `md`, `lg`, `xl`, `xxl` |
| Corner radius | `Style.cornerRadius` |
| Type scale | `Style.font.caption` … `Style.font.display` |
| Icon size | `Style.font.icon`, `Style.font.iconLarge` |
| Bar geometry | `Style.bar.iconSlot`, `Style.bar.iconCanvas`, `Style.bar.iconFont` |
| Interactive fills | `Style.hoverFillFor(...)`, `Style.selectedFillFor(...)`, `Style.normalFillFor(...)` |
| Control fills | `Style.controlFill(...)`, `Style.focusFillFor(...)` |
| Borders | `Border.controlSpec(...)`, `Border.surfaceSpec(...)`, `Border.localOrSurfaceSpec(...)` |
| Alpha helper | `Util.alpha(color, alpha)` |
| Motion duration | `Style.duration(ms)` |

Panel surfaces read text color from the bar when hosted there:

```qml
readonly property color foreground: bar ? bar.foreground : Commons.Color.foreground
```

Raw `1` is allowed for hairlines and optical geometry. Custom marks such as the
bar glyph may use unit math (`width / 24`) because they are drawings, not
layout.

### Color roles

| Role | Alias | Value | Use |
| :--- | :--- | :--- | :--- |
| Foreground | `foreground` | bar foreground or `Commons.Color.foreground` | Text, icons, active controls |
| Quiet | `dim` | `Util.alpha(foreground, 0.7)` | Meta, hints, inactive labels, section counts |
| Accent | `accent` | `Commons.Color.accent` | Live progress, cursor, primary action, done |
| Urgent | `urgent` | bar urgent or `Commons.Color.urgent` | Errors, stop, destructive confirmation |
| Background | `Commons.Color.background` | theme popup surface | Floating cards, scrims |

`dim` is deliberately a foreground blend, not the palette's `muted` role. On
many themes `muted` sits below a readable contrast and made captions and badges
vanish. Keep the blend.

### Status colors

The row badge and progress fill are decided by one function, `badgeColor()`:

| Status | Color | Why |
| :--- | :--- | :--- |
| `error` | `urgent` | A failure must be visible |
| `done` | `accent` | Completion is the positive signal |
| `downloading` | `foreground` | Active but not finished; accent already means progress |
| `queued`, `paused`, `cancelled` | `dim` | Waiting is quiet |

The badge background is the same color at `0.18` alpha. The badge text uses the
full color. Do not give "downloading" the accent: active and finished rows then
read alike.

### Ink on fills

`inkOn(fill)` chooses theme background or theme foreground by contrast ratio.
Use it for any text, glyph, or thumb placed on an accent or urgent fill. Never
assume white. Light and dark themes must both pass; test by switching theme, not
by inverting values.

### Alpha ladder

| Surface | Alpha |
| :--- | :--- |
| Rest fill | `Style.normalFillFor(...)` |
| Hover / keyboard cursor | `Style.hoverFillFor(...)` |
| Selected / marked | `Style.selectedFillFor(...)` or accent at `0.10` |
| Hairline | `Style.normalBorderFor(...)` |
| Strong border | `Style.hoverBorderFor(...)` or selected border |
| Quiet text | foreground at `0.70` |

Rules:

- No pure black or pure white for content. Black is allowed only for depth:
  keycap shadow, floating-layer shadow, or overlay scrim, never for text, fills,
  or borders on content.
- Never use accent for static headings, ordinary dividers, or decorative icons.
- Never use urgent unless the surface stops, fails, deletes, or warns.
- Do not tint the panel background. It is the theme's popup surface.
- Floating layers use `Commons.Color.background` with a normal control border.
- Marked rows use an accent tint (`0.10`) and the cursor keeps the border. A
  mark must stay visible after the keyboard cursor moves away.

---

## 4. Typography & Copy

### Typography

The theme font is usually monospace, which suits filenames, sizes, and speeds.
The type scale is:

| Role | Token | Use |
| :--- | :--- | :--- |
| Display | `Style.font.display` | `Catch`, `Catch settings` |
| Title | `Style.font.title` | `Downloads`, conflict and confirm titles |
| Subtitle | `Style.font.subtitle` | Row title, input text, primary button |
| Body | `Style.font.body` | Meta line, settings labels, counts |
| Body small | `Style.font.bodySmall` | Shortcut `?` mark |
| Caption | `Style.font.caption` | Header meta, folder line, badge, hints |
| Icon | `Style.font.icon`, `Style.font.iconLarge` | Row glyph, header actions |

The header subtitle is uppercase, bold, caption-sized, and letterspaced `1.2`.
Settings and conflict section labels (`DOWNLOAD FOLDER`, `TRANSFERS`,
`APPEARANCE`, `SCHEDULE`, `ALREADY IN YOUR FOLDER`) use the same small-caps
treatment. The queue section head (`Downloads`) is title case. Body copy stays
sentence case.

### Copy rules

- Name the action, not the mechanism: "Download", "Pause", "Resume", "Stop",
  "Remove", "Replace".
- A destructive action keeps its neutral label until it arms, then says exactly
  what will be lost: "Replace the saved file?", "Delete 2 files from disk?".
- Errors say what happened and what to try next. Use the backend's error text
  when it is specific; do not replace it with "Something went wrong".
- Keep one line per fact. Do not repeat the extension when the filename already
  shows it.
- No exclamation marks, no jokes, no filler.
- Use `Text.PlainText` for every dynamic string: filenames, titles, URLs, errors,
  paths, clipboard text, and theme labels.

---

## 5. Panel anatomy

`Panel.qml` extends `qs.Ui`'s `Panel`; `CatchPanel.qml` supplies the layer-shell
window, placement, focus prime, outside-click dismissal, and motion. The card is
one scrolling column: the header, link input, section head, rows, foot, and
settings share one `Flickable`. Do not nest another scroll area.

```text
┌──────────────────────────────────────────────┐
│ Catch                          [folder] [gear]│
│ DOWNLOAD MANAGER · 2 DOWNLOADING              │
│                                              │
│ [ Paste one or more links…      ] [Download ↵]│
│                                              │
│ Downloads                              3 items│
│ ┌──────────────────────────────────────────┐ │
│ │ ⬇  ubuntu.iso                  Downloading│ │
│ │    ISO · 1.2 / 4.8 GB · 18 MB/s · 32%     │ │
│ │    [mark] [stop] [remove]                 │ │
│ │    ████████████░░░░░░░░░░░░░░░            │ │
│ │    ██████████░░░░  seg                   │ │
│ │    Saving to Downloads/catch              │ │
│ └──────────────────────────────────────────┘ │
│ …                                            │
│ ──────────────────────────────────────────── │
│ ? Keyboard shortcuts                       › │
└──────────────────────────────────────────────┘
```

### Header

- Title: `Catch` on the queue, `Catch settings` in settings.
- Meta line: uppercase status. Queue mode shows
  `DOWNLOAD MANAGER · <statusText>`; settings shows
  `SETTINGS · SUPER+D TOGGLES · ESC GOES BACK`; a conflict shows
  `ALREADY IN YOUR FOLDER · ENTER CHOOSES · ESC CANCELS`.
- Actions: folder (`󰉋`) and gear (`󰒓`). The gear turns accent in settings.
  Both are `PanelActionButton`s with tooltips that include the key.

### Link input

One bordered `BorderSurface`, `Style.space(46)` tall, holding a transparent
`TextField` and a filled `SolidButton` ("Download ↵"). The surface owns the
chrome; the field has an empty background. Focus uses `focusFillFor` and a focus
border. The field takes one link or a pasted block; `Model.clipboardLinks()`
extracts every http(s) link, de-duplicates, and the backend validates again.

The same input switches to refresh mode: placeholder "New address for this
download (paste, Enter)…", and Enter runs `catch refresh <id> -`.

### Section head and empty state

`Downloads` on the left, `N items` on the right. Both are plain text; the count
uses `dim`. Empty state is one quiet line:

> Nothing yet — send a download from the browser (Alt+Shift+C).

### Row card

A `BorderSurface` with `Style.cornerRadius`, `Style.spacing.xl` padding, and
`Style.spacing.md` between body blocks. The row is a stable proxy over the model
roles, so progress and speed glide without recreating the delegate.

Anatomy, top to bottom:

1. **Head**: kind glyph from `kindGlyph()` (video, image, audio, archive,
   document, package, or generic file), title (subtitle, bold, `ElideMiddle`),
   status badge, mark, stop/resume, remove.
2. **Meta line**: a live head in accent followed by a static tail in `dim`.
   The live head carries speed and ETA; the static tail carries kind, size,
   segment count, and percent. The static tail takes the elide; size has display
   priority, so a live head must not consume the whole line.
3. **Progress bar**: 5 px, radius 2.5, accent fill (urgent on error), only while
   busy. Width animates over 250 ms with `OutCubic`.
4. **Segment bar**: 3 px blocks, one per range, only when `segments.total > 1`.
   Done blocks are accent, the active block is accent at `0.55`, pending blocks
   are `dim` at `0.18`.
5. **Folder line or error**: `Saved to …` / `Saving to …` in caption `dim`, or
   the backend error in `urgent`, wrapped.
6. **Expanded actions** (only when the row is open): pause/resume, replace
   address, open file. Mark, stop, and remove stay on the row itself so a row
   never looks unstoppable.

The cursor is `Style.selectedFillFor` plus a selected border. Hover is
`Style.hoverFillFor`. A mark is an accent tint, independent of the cursor. The
card color animates over 120 ms.

### Foot

A `PanelSeparator` at strength `0.1`, then the "Keyboard shortcuts" toggle with
a `?` mark and a chevron. The chevron rotates 90° over 120 ms. The sheet is a
two-column list: key cap and action, caption-sized, `dim`. It is collapsed by
default.

### Conflict view

When a link is already saved or already downloading, the panel shows a card
instead of the queue:

- Section header: `ALREADY IN YOUR FOLDER` or `STILL DOWNLOADING`.
- Title, saved path, and one sentence explaining the choice.
- Actions: **Open** (solid, only when a file exists), **Download again**
  (focused), **Replace** (only when a file exists, destructive confirmation),
  **Cancel**.

Focus lands on **Download again**, never on Open. The panel may open by itself
for a question, and Enter is the first thing pressed. The question is also
mirrored in `state.asks`, so it survives a panel reload.

### Settings

A form, not a list. The cursor stays put, and Tab is left to the bar's panel
switching. Sections:

- **DOWNLOAD FOLDER** — current path, a text field, Save. Empty resets.
- **TRANSFERS** — parallel downloads (1, 3, 5, 10), download segments
  (1, 2, 4, 8), bandwidth mode (unlimited, balanced, low), global cap (KB/s).
- **APPEARANCE** — panel position (under the bar icon, top center, center of
  screen, left pane, right pane, bottom center).
- **SCHEDULE** — anytime or scheduled window, from/until `HH:MM`, Save. Turning
  it on installs the systemd timer; turning it off removes it.

Choice settings use a label and a right-aligned control; text values use a
full-width field with a Save button. Dropdowns are custom (`PanelDropdown`)
because they must escape the scroll view and stay keyboard operable. Every field
releases the keyboard when it commits; otherwise the panel becomes a keyboard
dead-end.

### Confirm overlays

Replace and full delete are sibling overlays over the content, not new pages.
They use a `Commons.Color.background` scrim at `0.7`, a centered card capped at
`Style.space(370)`, and a title that states the loss. The destructive choice is
preselected. Left/Right/Tab switch, Enter approves, Esc cancels, and the buttons
do the same. Clicks outside cancel.

---

## 6. Motion

Motion is short and one-shot. The animated values are:

| Element | Duration | Easing |
| :--- | :--- | :--- |
| Row hover/cursor color | 120 ms | linear `ColorAnimation` |
| Progress width | 250 ms | `OutCubic` |
| Shortcut chevron | 120 ms | `OutCubic` |
| Bar placement open / close | 140 / 120 ms | `OutCubic` / `InCubic` |
| Edge placement open / close | 200 / 160 ms | `OutQuint` / `InOutCubic` |
| Center placement | none | appears in place |

The panel animates one value, `reveal`: 1 open, 0 parked off its edge. The card
stays opaque and the screen edge clips it, so it reads as a sheet sliding in,
not a faded popup. Edge placements enter and leave through their own edge; the
bar placement keeps its original fade. Focus is primed for 75 ms with
`Exclusive`, then settles on `OnDemand`; keyboard focus follows `open`, not
`visible`, so a fade-out cannot lock the user out.

Do not add motion to text, badges, or layout. Do not animate a state change that
the user is waiting on.

---

## 7. Interaction

The panel is keyboard-first. `PanelKeyCatcher` dispatches; the panel must never
declare a second `Keys.onPressed` on the same object.

| Key | Action |
| :--- | :--- |
| `↑` `↓` / `j` `k` | Move the cursor between rows |
| `space` | Mark or unmark the cursor row |
| `a` | Mark or unmark all rows |
| `p` | Pause a live row, or resume a stopped row; with marks, act on all |
| `x` | Stop the cursor row, keeping the row and partial data |
| `r` | Replace the address of a failed or stopped row |
| `c` | Remove marked rows (files stay) |
| `d` | Delete marked files from disk (confirmation) |
| `delete` | Remove the cursor row (file stays) |
| `enter` | Open a finished file |
| `tab` | Move between the link field and the row list |
| `ctrl+v` / `super+v` | Paste link(s) into the input from anywhere |
| `o` | Open the download folder |
| `s` | Open or close settings |
| `esc` | Back one level (settings → queue), then close |
| `q` | Close |

Rules:

- Horizontal arrows are off. One key used to close the panel while its twin
  opened a file; only vertical movement remains.
- Settings is a form: the cursor stays put, and queue verbs are disabled.
- While a text field owns the keyboard, the catcher is blocked and the field
  handles Tab and paste itself. Every field must call `release()` when it
  commits.
- `Enter` is inert for row activation. It opens a finished file; Space marks.
  This avoids opening a file while the user is submitting a pasted link.
- Hover and keyboard share one cursor. Hovering the panel holds it open.
- Mouse: left-click the bar mark toggles the panel; right-click opens the
  folder; middle-click runs `catch list`. Double-click a finished row to open
  it. Clicking a row sets the cursor.
- The panel closes on outside click. Clicks in the bar strip pass through so
  another bar icon can be selected in one click.

---

## 8. Bar widget

The bar mark is a `BarIconButton` with the download glyph `󰇚`. It is `active`
when the queue has a queued or downloading row, with `activeColor` set to the
accent. Its tooltip is `Model.statusText(queue)`:

- no downloads: `Catch — no downloads yet`
- finished only: `Catch — N finished`
- active: `Catch — <name> · <percent> (+N more)`

The bar mark is a queue signal, not a progress bar. Do not add a second glyph,
a badge count, or a spinner. The panel is where detail lives.

---

## 9. Panel placement

`CatchPanel.qml` supports six placements, stored in `uiPlacement`:

| Value | Where it opens |
| :--- | :--- |
| `bar` | Under the bar icon (default) |
| `top` | Top center |
| `center` | Center of the screen |
| `left` | Left pane, full available height |
| `right` | Right pane, full available height |
| `bottom` | Bottom center |

The bar, top, center, and bottom placements fit their content, capped at
`Style.space(560)` high. The left and right panes use the full available height
so the scroll area has somewhere to go. The panel width is fitted to
`Style.space(480)` and the screen's available rectangle. Unknown values fall back
to `bar`, so a stale setting cannot leave the panel without a position.

`CatchPanel.qml` is adapted from Omarchy's `qs.Ui KeyboardPanel` (MIT). Keep the
window mechanics — focus prime, outside-click dismissal, bar click-through mask,
other-output dismissal, single-popout coordination — in sync with the shell file
when Omarchy changes the host.

---

## 10. Do / don't

**Do**

- Use `Style.space()` for gaps and `Style.font.*` for type.
- Use `Commons.Color`, `Util.alpha()`, `Border.controlSpec()`, and
  `Style.*FillFor()`.
- Keep one scroll area.
- Reserve space for dynamic values and elide the static tail first.
- Use `Text.PlainText` for every dynamic string.
- Release keyboard focus when a field commits.
- Confirm replace and full delete.
- Test both themes and all six placements.
- Test with an empty queue, a live transfer, a paused row, an error, a
  completed row, and a saved-link question.

**Don't**

- Add a private palette or hardcoded color.
- Add a shadow to a row or button.
- Animate layout or text.
- Give "downloading" the accent badge.
- Use urgent for anything that is not stopping, failing, deleting, or warning.
- Add a second window, popup, or notification-only workflow.
- Put a URL in argv or a shell string.
- Hide stop or remove behind a menu.
- Add a new setting without a manifest entry, a panel row, and backend support.
- Let a field keep focus after it commits.

---

## 11. Review checklist

Before handing back a panel change:

- [ ] Tokens only: no new color, radius, spacing, or font literals.
- [ ] One scroll area, no nested scrolling; dynamic values do not move the
      scroll origin.
- [ ] Every dynamic string is `Text.PlainText`.
- [ ] The keyboard path works with the pointer untouched.
- [ ] Fields release focus; the catcher is not stuck blocked.
- [ ] Destructive actions ask and relabel.
- [ ] No layout shift when bytes, speed, ETA, badge, or error changes.
- [ ] Both light and dark themes read.
- [ ] All six placements open, fit, and close.
- [ ] `bin/lint-qml` passes.
- [ ] `docs/design.md` and `README.md` match the change.
