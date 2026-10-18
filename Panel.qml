import QtQuick
import QtQuick.Controls
import QtQuick.Layouts
import Quickshell
import Quickshell.Io
import qs.Commons
import qs.Commons as Commons
import qs.Ui
import "Model.js" as Model

// Catch: bar presence plus the queue panel. A click on the mark opens Catch
// itself — the download queue with per-row pause/resume/cancel — instead of
// the download folder (right-click still opens the folder).
Panel {
  id: root
  moduleName: "io.github.tahadx.catch"
  ipcTarget: "io.github.tahadx.catch"
  manageIpc: false

  readonly property string home: Quickshell.env("HOME") || ""
  readonly property string downloadDir: {
    var raw = root.setting("downloadDir", "")
    var dir = Model.expandHome(raw === undefined || raw === null ? "" : raw, root.home)
    return dir.length > 0 ? dir : Model.defaultDownloadDir(root.home)
  }
  readonly property string statePath: (Quickshell.env("XDG_STATE_HOME") || root.home + "/.local/state") + "/omarchy/catch/state.json"

  property var queue: Model.empty()
  readonly property int activeCount: Model.activeCount(root.queue)
  readonly property bool busy: Model.hasActive(root.queue)
  readonly property real progress: Model.overallFraction(root.queue)

  property string actionError: ""
  property int cursorIndex: -1
  property var marked: ({})
  property bool showingSettings: false
  // The shortcut sheet at the foot of the home page, collapsed by default.
  property bool showShortcuts: false
  // Id of the row whose actions are expanded ("…" on the card).
  property string expandedRow: ""
  // Already in the download folder: non-null while the panel asks whether
  // to open that file, save another copy, or replace it.
  property var conflict: null
  property string conflictTitle: ""
  // The link the conflict dialog is asking about.
  property string conflictUrl: ""
  // True when `conflict` came from state.asks, so a reload can close it
  // once the question is gone.
  property bool conflictFromAsk: false
  // Last ask the panel has already announced. Stops a sitting question from
  // popping the panel again on every progress write.
  // URL whose question the user just answered. Ignored until the state file
  // drops it, so the dialog cannot snap back open mid-command.
  property string ignoringAsk: ""
  // Link waiting on the destructive-replace confirmation.
  property string replacePending: ""
  // True briefly after the conflict dialog closes, so the keys that
  // dismissed it cannot activate the row behind it.
  // Last close timestamp: clicks landing in the fade-out window must not
  // resurrect the panel.
  property double closedAt: 0

  function closePeers() {
    if (root.bar && typeof root.bar.moduleWidgets === "function") {
      var items = root.bar.moduleWidgets(root.moduleName)
      for (var i = 0; i < items.length; i++) {
        if (items[i] && items[i] !== root && typeof items[i].close === "function") items[i].close()
      }
    }
  }
  // Set by the settings TextFields themselves (content scope can write
  // outward). Root-scope code must never reference content-scoped ids
  // directly: they are invisible outside the content Component and every
  // such read throws, which is exactly what killed all keyboard input.
  property bool textEditing: false
  // Which home-page zone owns the keyboard: "list" or "input". Tracked
  // explicitly rather than inferred from textEditing, so Tab cycling does
  // not depend on the field focus bookkeeping staying in sync.
  property string focusZone: "list"

  function typing() {
    return root.textEditing
  }

  function toggleSettings() {
    root.hold()
    root.showingSettings = !root.showingSettings
  }

  // URL arrives as an argument: content-scoped field ids are not visible
  // from root scope (see textEditing above).
  function catchUrl(url) {
    root.hold()
    var link = String(url || "").trim()
    if (!/^https?:\/\//i.test(link)) {
      root.actionError = "Links start with http(s)://"
      errorTimer.restart()
      return
    }
    // The backend decides. A file already in the folder becomes an ask in
    // the state file (typed here, or handed over by the browser) and the
    // watcher below opens the question. A missing file is fetched.
    root.ignoringAsk = ""
    // enqueue, not add: `add` blocks on the transfer, which held the panel's
    // single runner for the whole download and left every later command
    // waiting behind it. enqueue writes the row and returns, like a browser
    // hand-off does.
    root.runCatch(["enqueue", link], "Could not catch that link.")
  }

  function firstAsk() {
    var asks = root.queue && root.queue.asks
    return (asks && asks.length > 0) ? asks[0] : null
  }

  function showAsk(ask) {
    if (!ask) return
    root.conflictFromAsk = true
    root.conflictTitle = ask.title !== "" ? ask.title : (ask.filename || "Download")
    root.conflictUrl = ask.url
    root.conflict = ask
    root.showingSettings = false
  }

  // A question arrived in the state file. The first read of a session only
  // remembers it (noteArrival arms `primed` after this). Later arrivals
  // open the panel.
  function syncAsks() {
    if (!root.primed) return
    var ask = root.firstAsk()
    if (ask && root.ignoringAsk === ask.url) return
    if (!ask) {
      root.ignoringAsk = ""
      if (root.conflictFromAsk && root.replacePending === "") {
        root.conflict = null
        root.conflictUrl = ""
        root.conflictFromAsk = false
      }
      return
    }
    if (root.replacePending !== "") return
    // Open whenever the question is not already on screen. Keying this on the
    // url being "new" meant only the first click on a link Catch already had
    // opened the panel: showAsk stored that url as announced, so every later
    // click had arrived=false and returned here without ever opening. Catch
    // raised the question and the user saw nothing at all.
    var showing = root.conflict !== null && root.conflict.url === ask.url
    root.showAsk(ask)
    if (showing || root.opened) return
    root.open()
  }

  function applyPendingAsk() {
    if (root.replacePending !== "") return
    var ask = root.firstAsk()
    if (!ask || root.ignoringAsk === ask.url) return
    root.showAsk(ask)
  }

  // Where the saved file sits, with the home directory dropped.
  function savedWhere() {
    var full = root.savedFilePath()
    if (full === "") return ""
    var where = Model.shortPath(full, root.home)
    var bytes = root.conflict && Number(root.conflict.bytes) > 0 ? Number(root.conflict.bytes) : 0
    return bytes > 0 ? where + " · " + Model.formatBytes(bytes) : where
  }

  // Absolute path of the file the question is about, or "" when it is not
  // safe to hand to xdg-open.
  function savedFilePath() {
    var path = root.conflict && root.conflict.path ? String(root.conflict.path) : ""
    if (path === "" || path.indexOf("..") >= 0 || path.charAt(0) === "/" || path.indexOf("\\") >= 0) return ""
    return root.downloadDir + "/" + path
  }

  function openSaved() {
    root.hold()
    var full = root.savedFilePath()
    if (full === "" || !root.bar || typeof root.bar.run !== "function") {
      root.actionError = "That file is no longer where Catch left it."
      errorTimer.restart()
      return
    }
    root.bar.run("xdg-open " + root.shellQuote(full))
    root.dismissAsk()
  }

  // Conflict actions.
  // copyConflict: catch the link again. The backend lands it as another
  // file ("clip (1).webm") and leaves the caught one alone.
  function copyConflict() {
    if (root.conflictUrl === "") return
    var url = root.conflictUrl
    root.ignoringAsk = url
    root.closeConflict()
    root.runCatch(["copy", url], "Could not save another copy.")
  }

  // The question is answered without a transfer: open, or cancel.
  function dismissAsk() {
    var url = root.conflictUrl
    if (url !== "") root.ignoringAsk = url
    root.closeConflict()
    if (url !== "") root.runCatch(["dismiss", url], "Could not dismiss that question.")
  }

  // replaceConflict: the only destructive choice. Fetch the link again and
  // throw the saved bytes away, so the folder keeps one copy. Keyed by URL,
  // so it works even when the queue row is gone. The confirm sits on top
  // before any bytes are discarded.
  function replaceConflict() {
    if (root.conflictUrl === "") return
    root.hold()
    var url = root.conflictUrl
    root.conflict = null
    root.dismissRowArm()
    root.replacePending = url
  }

  function closeConflict() {
    if (root.conflict === null) return
    root.conflict = null
    // The Enter that dismissed this dialog must not carry on into the row
    // underneath and xdg-open the file: no file browser after saving.
    root.dismissRowArm()
  }

  // Drop a pending row activation and block the next one briefly: the key
  // that opens or dismisses the conflict dialog must not carry on into
  // the row underneath and xdg-open its file.
  function dismissRowArm() {
  }

  // The folder field accepts "~" and "~/…". Store the home path itself so
  // the backend never sees a literal tilde.
  function saveDownloadDir(text) {
    var raw = String(text || "").trim()
    root.writeSetting("downloadDir", raw === "" ? "" : Model.expandHome(raw, root.home))
  }

  // Persist one widget setting to shell.json (mirrors first-party widgets:
  // applied locally first, shell write comes back as the same value).
  function writeSetting(key, value) {
    var entry = { id: root.moduleName }
    for (var k in root.settings) if (k !== "id") entry[k] = root.settings[k]
    entry[key] = value
    root.settings = entry
    if (root.bar && root.bar.shell && typeof root.bar.shell.updateEntryInline === "function") {
      root.bar.shell.updateEntryInline(root.moduleName, entry)
    } else {
      root.actionError = "Shell writer unavailable; use: omarchy bar set io.github.tahadx.catch " + key + " <value>"
      errorTimer.restart()
    }
  }

  function currentParallel() {
    var n = parseInt(root.setting("maxParallel", 3), 10)
    if (n !== 1 && n !== 3 && n !== 5 && n !== 10) n = 3
    return n
  }

  function currentSegments() {
    var n = parseInt(root.setting("segments", 4), 10)
    if (n !== 1 && n !== 2 && n !== 4 && n !== 8) n = 4
    return n
  }

  // Values arrive as arguments because content-scoped field ids are not
  // visible from root scope (see textEditing above).
  function saveSchedule(a, b) {
    root.hold()
    var ok = function (v) { return /^([01]\d|2[0-3]):[0-5]\d$/.test(v) }
    if (a !== "" && !ok(a)) {
      root.actionError = "Window start must be HH:MM (24h)."
      errorTimer.restart()
      return
    }
    if (b !== "" && !ok(b)) {
      root.actionError = "Window end must be HH:MM (24h)."
      errorTimer.restart()
      return
    }
    if (a !== "") root.writeSetting("schedStart", a)
    if (b !== "") root.writeSetting("schedStop", b)
  }

  // Auto-open: a freshly caught download pops the panel; it closes itself
  // after autoCloseMs unless the user engages (hover, click, keys).
  readonly property int autoCloseMs: 7000
  // Rows Catch has seen, so a first read learns the queue instead of treating
  // everything as newly arrived.
  property var knownIds: ({})
  property bool primed: false
  property bool autoOpened: false
  property bool userHolding: false

  function noteArrival() {
    var fresh = false
    var seen = {}
    for (var i = 0; i < root.queue.downloads.length; i++) {
      var id = root.queue.downloads[i].id
      seen[id] = true
      if (!root.knownIds[id]) fresh = true
    }
    root.knownIds = seen
    if (!root.primed) {
      // First read (shell start): learn the file, never pop. A question
      // already sitting in the file is shown when the user opens Catch.
      root.primed = true
      return
    }
    if (!fresh || root.userHolding) return
    // A caught download is worth showing: pop the panel, but only one the
    // user did not ask for, so it can close itself again. Opening on
    // request (bar icon, Super+D, a row) is the user's business and stays.
    if (!root.opened) {
      root.open()
      root.autoOpened = true
    }
    if (root.autoOpened) autoClose.restart()
  }

  function markedCount() {
    var n = 0
    for (var id in root.marked) if (root.marked[id]) n++
    return n
  }

  function isMarked(id) {
    return !!root.marked[id]
  }

  function toggleMark(id) {
    root.hold()
    var next = {}
    for (var key in root.marked) next[key] = root.marked[key]
    if (next[id]) delete next[id]
    else next[id] = true
    root.marked = next
  }

  function pruneMarks() {
    var alive = {}
    for (var i = 0; i < root.queue.downloads.length; i++) alive[root.queue.downloads[i].id] = true
    var next = {}
    for (var id in root.marked) if (alive[id]) next[id] = true
    root.marked = next
  }

  function deleteMarked() {
    root.hold()
    var ids = []
    for (var id in root.marked) ids.push(id)
    root.marked = ({})
    for (var i = 0; i < ids.length; i++) root.removeEntry(ids[i])
  }

  function toggleAllMarks() {
    root.hold()
    var all = true
    for (var i = 0; i < root.queue.downloads.length; i++) {
      if (!root.marked[root.queue.downloads[i].id]) { all = false; break }
    }
    var next = {}
    if (!all) {
      for (var j = 0; j < root.queue.downloads.length; j++) next[root.queue.downloads[j].id] = true
    }
    root.marked = next
  }

  function reload(text) {
    root.queue = Model.parse(text)
    root.syncModel(root.queue.downloads)
    root.pruneMarks()
    if (root.cursorIndex >= root.queue.downloads.length) root.cursorIndex = root.queue.downloads.length - 1
    root.refreshSpeeds()
    root.syncAsks()
    root.noteArrival()
  }

  function refresh() {
    stateFile.reload()
  }

  function open() {
    root.refresh()
    root.cursorIndex = root.queue.downloads.length > 0 ? 0 : -1
    root.autoOpened = false
    root.userHolding = false
    root.textEditing = false
    root.focusZone = "list"
    autoClose.stop()
    root.applyPendingAsk()
    root.controller.show()
    // Belt and suspenders next to the shell's own focus prime: land
    // keyboard focus in our catcher even if the prime fired before the
    // lazily built content existed.
    Qt.callLater(function() {
      Qt.callLater(function() {
        // loadedContent (not the keyCatcher id: content-scoped ids are
        // invisible out here) — belt and suspenders next to the shell's
        // own focus prime.
        var content = panel.loadedContent
        // A question parks focus on its Open button. Stealing it back here
        // would leave Enter on the queue catcher.
        if (root.conflict === null && root.opened && content) content.forceActiveFocus()
      })
    })
  }

  function close() {
    root.actionError = ""
    root.cursorIndex = -1
    root.marked = ({})
    root.showingSettings = false
    root.showShortcuts = false
    root.expandedRow = ""
    root.conflict = null
    root.conflictFromAsk = false
    root.replacePending = ""
    root.closeDropdowns()
    root.autoOpened = false
    root.userHolding = false
    root.textEditing = false
    root.focusZone = "list"
    autoClose.stop()
    root.closedAt = Date.now()
    root.controller.hide()
  }

  Timer {
    id: autoClose
    interval: root.autoCloseMs
    onTriggered: {
      if (root.opened && root.autoOpened && !root.userHolding) root.close()
    }
  }

  function switchPanel(direction) {
    if (root.bar && typeof root.bar.switchPanelFrom === "function")
      return root.bar.switchPanelFrom(root, direction)
    return false
  }

  // Single toggle path for the bar button, Super+D, and IPC: never
  // resurrects a panel that is still fading out, and never leaves two
  // instances open on multi-monitor bars. An open-but-unfocused panel
  // (e.g. auto-opened while you type elsewhere) gets focused instead of
  // closed, so Super+D is always "take me to Catch".
  function togglePanel() {
    var content = panel.loadedContent
    var focused = content ? content.activeFocus : false
    if (root.opened && focused) root.close()
    else if (root.opened) {
      root.hold()
      if (content) content.forceActiveFocus()
    } else if ((Date.now() - root.closedAt) < 350) {
      root.close()
    } else {
      root.closePeers()
      root.open()
    }
  }

  function openFolder() {
    root.hold()
    if (!root.bar || typeof root.bar.run !== "function") return
    root.bar.run("xdg-open " + shellQuote(root.downloadDir))
    root.close()
  }

  function openFile(id) {
    root.hold()
    if (!root.bar || typeof root.bar.run !== "function") return
    var d = null
    for (var i = 0; i < root.queue.downloads.length; i++) {
      if (root.queue.downloads[i].id === id) d = root.queue.downloads[i]
    }
    if (!d) return
    // Just the file. This used to open the download folder as well, so one
    // Enter put a file browser on screen too — the folder is `o`, and the
    // header button.
    root.bar.run("xdg-open " + shellQuote(root.downloadDir + "/" + d.filename))
  }

  function shellQuote(s) {
    return "'" + String(s).replace(/'/g, "'\\''") + "'"
  }

  // One queue for the verbs that answer with nothing but their status.
  property var cmdQueue: []
  property string cmdComplaint: ""

  // Row actions get their own lane. A queued transfer used to hold the single
  // runner for its whole length, so stop and remove sat in cmdQueue for
  // minutes — and only worked when the download had come from the browser,
  // which does not block. A second queue keeps them independent of whatever
  // long command is in flight, and still serializes them with each other so
  // two quick clicks cannot interleave.
  property var rowQueue: []
  property string rowComplaint: ""

  function runCatch(args, complaint) {
    root.cmdQueue = root.cmdQueue.concat([{ argv: ["catch"].concat(args), complaint: complaint }])
    root.nextCmd()
  }

  function nextCmd() {
    if (cmdRunner.running || root.cmdQueue.length === 0) return
    var next = root.cmdQueue[0]
    root.cmdQueue = root.cmdQueue.slice(1)
    root.cmdComplaint = next.complaint
    cmdRunner.command = next.argv
    cmdRunner.running = true
  }

  function runRow(args, complaint) {
    root.rowQueue = root.rowQueue.concat([{ argv: ["catch"].concat(args), complaint: complaint }])
    root.nextRow()
  }

  function nextRow() {
    if (rowRunner.running || root.rowQueue.length === 0) return
    var next = root.rowQueue[0]
    root.rowQueue = root.rowQueue.slice(1)
    root.rowComplaint = next.complaint
    rowRunner.command = next.argv
    rowRunner.running = true
  }

  function hold() { root.userHolding = true }

  function pauseEntry(id) { root.hold(); root.runRow(["pause", id], "Could not pause that download.") }
  function resumeEntry(id) { root.hold(); root.runRow(["resume", id], "Could not resume that download.") }
  function removeEntry(id) { root.hold(); root.runRow(["remove", id], "Could not remove that download.") }
  // Stop is not remove: it ends the transfer and leaves the row behind as
  // cancelled, so the entry can be re-queued. Remove drops the row instead.
  function cancelEntry(id) { root.hold(); root.runRow(["cancel", id], "Could not stop that download.") }

  function resumable(d) {
    return d.status === "paused" || d.status === "error" || d.status === "cancelled"
  }

  function pausable(d) {
    // Queued entries wait; downloading ones stand down within ~250 ms.
    return d.status === "queued" || d.status === "downloading"
  }

  function stoppable(d) {
    // Only a live transfer has anything to stop.
    return d.status === "queued" || d.status === "downloading"
  }

  // Filled action painted here, not with qs.Ui.Button. The kit replaces a
  // filled button's color with a faint wash on focus, which made the accent
  // look like the panel. The fill stays the theme accent. The label is
  // whichever of the theme's background and foreground contrasts with it.
  component SolidButton: BorderSurface {
    id: solid

    property string text: ""
    // Keyboard choice in a confirm. Draws the theme edge without changing the fill.
    property bool marked: false
    signal clicked()

    radius: root.fieldButtonRadius
    color: root.accent
    borderSpec: (activeFocus || marked)
      ? Border.flat(root.foreground, 1)
      : Border.controlSpec("normal", root.foreground, root.accent)
    implicitWidth: solidLabel.implicitWidth + Style.spacing.xl * 4
    implicitHeight: Math.max(Style.space(34), solidLabel.implicitHeight + Style.spacing.md * 2)
    activeFocusOnTab: true

    Text {
      id: solidLabel
      anchors.centerIn: parent
      textFormat: Text.PlainText
      text: solid.text
      color: root.inkOn(root.accent)
      font.family: root.fontFamily
      font.pixelSize: Style.font.subtitle
      font.bold: true
    }

    MouseArea {
      anchors.fill: parent
      cursorShape: Qt.PointingHandCursor
      onClicked: solid.clicked()
    }

    Keys.onReturnPressed: solid.clicked()
    Keys.onEnterPressed: solid.clicked()
    Keys.onSpacePressed: solid.clicked()
  }

  // A dropdown for the settings rows, written here rather than taken from
  // qs.Ui: that component renders nothing at all inside this panel (checked
  // with a labelled trigger), and it carries Qt's CloseOnPressOutside, which
  // closes its popup on the very press that should close it while the
  // trigger's own click re-opens it — so once open, it never closed.
  //
  // This one toggles the way a click should: closed -> open, open -> closed.
  component PanelDropdown: Item {
    id: dd

    property var options: []
    property string value: ""
    property color foreground: root.foreground
    property color accent: root.accent
    // The open list sits on the popup surface, which is not the bar. Pair
    // its ink with that surface or the rows vanish on themes where the bar
    // text and the popup background are close.
    readonly property color popupText: Commons.Color.popups.text
    // The visible scroll area, supplied by the content scope: the popup is
    // measured against it so it flips up rather than being clipped by the
    // panel's bottom edge.
    property var spaceItem: null
    property real spaceHeight: 0
    readonly property bool open: drop.opened
    signal picked(string value)
    // The content scope watches this to let the key catcher stand down while
    // the popup owns the keys.
    signal popupToggled(bool isOpen)

    implicitHeight: trigger.implicitHeight
    implicitWidth: Style.spacing.dropdownWidth

    function optionValue(o) {
      return (o && typeof o === "object") ? String(o.value) : String(o)
    }
    function optionLabel(o) {
      return (o && typeof o === "object") ? String(o.label) : String(o)
    }
    function currentLabel() {
      for (var i = 0; i < options.length; i++) {
        if (optionValue(options[i]) === value) return optionLabel(options[i])
      }
      return value
    }
    function indexOfValue(v) {
      for (var i = 0; i < options.length; i++) {
        if (optionValue(options[i]) === v) return i
      }
      return -1
    }
    function toggle() {
      drop.opened ? drop.close() : drop.open()
    }
    function close() {
      drop.close()
    }
    function selectCurrent() {
      var i = list.currentIndex
      if (i < 0 || i >= options.length) return
      var v = optionValue(options[i])
      dd.value = v
      dd.picked(v)
      drop.close()
    }

    BorderSurface {
      id: trigger
      width: parent.width
      height: Style.space(30)
      radius: root.fieldRadius
      readonly property bool _hot: triggerHover.hovered || trigger.activeFocus
      color: Style.controlFill(trigger.activeFocus, triggerHover.hovered, dd.foreground, dd.accent)
      borderSpec: Border.controlSpec(trigger.activeFocus ? "focus" : (triggerHover.hovered ? "hover-cursor" : "normal"), dd.foreground, dd.accent)

      HoverHandler { id: triggerHover }

      Text {
        anchors.left: parent.left
        anchors.right: chevron.left
        anchors.verticalCenter: parent.verticalCenter
        anchors.leftMargin: Style.spacing.controlPaddingX + trigger.borderLeft
        anchors.rightMargin: Style.spacing.md
        textFormat: Text.PlainText
        text: dd.currentLabel()
        // BorderSurface has no foreground of its own. Reading it painted
        // the closed label black, which disappears on a dark control fill.
        color: dd.foreground
        font.family: root.fontFamily
        font.pixelSize: Style.font.body
        elide: Text.ElideRight
      }

      Text {
        id: chevron
        anchors.right: parent.right
        anchors.verticalCenter: parent.verticalCenter
        anchors.rightMargin: Style.spacing.controlGap + trigger.borderRight
        textFormat: Text.PlainText
        text: "›"
        rotation: dd.open ? 90 : 0
        Behavior on rotation {
          NumberAnimation { duration: Style.duration(120); easing.type: Easing.OutCubic }
        }
        color: dd.foreground
        font.family: root.fontFamily
        font.pixelSize: Style.font.body
      }

      MouseArea {
        id: hit
        anchors.fill: parent
        hoverEnabled: true
        cursorShape: Qt.PointingHandCursor
        // What the popup looked like when the press landed. A press outside
        // closes it, so if the state no longer matches, this press was the
        // one that dismissed it — do nothing, or it would spring back open.
        property bool wasOpen: false
        onPressed: wasOpen = drop.opened
        onClicked: {
          if (!root.dropdownClickToggles(wasOpen, drop.opened)) return
          dd.toggle()
        }
      }

      Keys.onPressed: function (event) {
        if (event.key === Qt.Key_Return || event.key === Qt.Key_Enter
            || event.key === Qt.Key_Space || event.key === Qt.Key_Down) {
          dd.toggle()
          event.accepted = true
        } else if (event.key === Qt.Key_Escape && drop.opened) {
          drop.close()
          event.accepted = true
        }
      }

      Popup {
        id: drop
        x: 0
        y: trigger.height + Style.spacing.xxs
        width: trigger.width
        implicitHeight: Math.min(options.length * Style.spacing.popupRowHeight + Style.spacing.xxs,
                                 Style.spacing.popupRowHeight * 6)
        padding: Style.spacing.hairline
        focus: true
        // Escape dismisses; a press elsewhere is handled by `hit` above so
        // the trigger can tell a dismissing press from an opening one.
        closePolicy: Popup.CloseOnEscape

        background: BorderSurface {
          color: Commons.Color.popups.background
          borderSpec: Border.localOrSurfaceSpec("popups", "border", Commons.Color.popups.border, Commons.Color.popups.border, Style.normalBorderWidth)
          radius: root.fieldRadius
        }

        onOpened: {
          // Flip above the trigger when the list would run past the bottom of
          // the visible area: the settings rows sit low, and a clipped option
          // is no option at all.
          var below = dd.spaceHeight
          if (dd.spaceItem) below -= trigger.mapToItem(dd.spaceItem, 0, trigger.height).y
          drop.y = below >= drop.implicitHeight + Style.spacing.xxs
            ? trigger.height + Style.spacing.xxs
            : -drop.implicitHeight - Style.spacing.xxs
          list.currentIndex = Math.max(0, dd.indexOfValue(dd.value))
          list.forceActiveFocus()
          dd.popupToggled(true)
        }
        onClosed: dd.popupToggled(false)

        contentItem: ListView {
          id: list
          spacing: Style.spacing.labelGap
          clip: true
          implicitHeight: contentHeight
          currentIndex: -1
          model: dd.options

          Keys.priority: Keys.BeforeItem
          Keys.onPressed: function (event) {
            if (event.key === Qt.Key_Escape) {
              drop.close()
              event.accepted = true
            } else if (event.key === Qt.Key_Down || event.text === "j") {
              list.currentIndex = Math.min(dd.options.length - 1, list.currentIndex + 1)
              event.accepted = true
            } else if (event.key === Qt.Key_Up || event.text === "k") {
              list.currentIndex = Math.max(0, list.currentIndex - 1)
              event.accepted = true
            } else if (event.key === Qt.Key_Return || event.key === Qt.Key_Enter) {
              dd.selectCurrent()
              event.accepted = true
            }
          }

          delegate: Item {
            id: optionRow
            // Both are required properties on purpose: a delegate that
            // declares any required property gets *only* the ones it
            // declares, so leaving modelData out makes it an unresolved
            // identifier and every label renders empty.
            required property int index
            required property var modelData
            width: list.width
            height: Style.spacing.popupRowHeight

            Rectangle {
              anchors.fill: parent
              anchors.leftMargin: Style.spacing.xs
              anchors.rightMargin: Style.spacing.xs
              radius: root.fieldButtonRadius
              color: optionRow.index === list.currentIndex
                ? Style.hoverFillFor(dd.popupText, dd.accent)
                : "transparent"

              Text {
                anchors.left: parent.left
                anchors.right: parent.right
                anchors.verticalCenter: parent.verticalCenter
                anchors.leftMargin: Style.spacing.controlPaddingX
                anchors.rightMargin: Style.spacing.controlPaddingX
                textFormat: Text.PlainText
                text: dd.optionLabel(optionRow.modelData)
                color: optionRow.index === list.currentIndex
                  ? Style.hoverStateColor(dd.popupText, dd.accent)
                  : dd.popupText
                font.family: root.fontFamily
                font.pixelSize: Style.font.body
                elide: Text.ElideRight
              }

              MouseArea {
                anchors.fill: parent
                hoverEnabled: true
                cursorShape: Qt.PointingHandCursor
                onPositionChanged: list.currentIndex = optionRow.index
                onClicked: dd.selectCurrent()
              }
            }
          }
        }
      }
    }
  }

  function cancellable(d) {
    // Everything, live or not: removing a live transfer stops it first.
    return true
  }

  // The glyph on a card's left edge: what kind of thing this is. The badge
  // beside it says what state it is in.
  function kindGlyph(d) {
    if (String(d.kind || "file") === "video") return "󰟍"
    var ext = Model.kindLabel(d).toLowerCase()
    if (["png", "jpg", "jpeg", "gif", "webp", "avif", "svg", "bmp", "heic", "tiff"].indexOf(ext) >= 0) return "󰋩"
    if (["mp3", "flac", "wav", "ogg", "m4a", "aac", "opus"].indexOf(ext) >= 0) return "󰎇"
    if (["zip", "tar", "gz", "bz2", "xz", "7z", "rar", "zst"].indexOf(ext) >= 0) return "󰐐"
    if (["pdf", "epub"].indexOf(ext) >= 0) return "󰇁"
    if (["apk", "deb", "rpm", "pkg", "appimage"].indexOf(ext) >= 0) return "󰀳"
    return "󰈢"
  }

  // The card's "…" button: reveals that row's actions inline, and only ever
  // one row at a time.
  function toggleRowActions(id) {
    root.hold()
    root.expandedRow = root.expandedRow === id ? "" : id
  }

  // Does a click on a dropdown's trigger mean "toggle"? A press outside the
  // popup dismisses it, so by the time the trigger's click lands, an open
  // popup is already closed. Toggling then would spring it straight back
  // open and the control would look stuck. When the state no longer matches
  // what it was when the press landed, this press was the dismissing one:
  // leave it alone.
  function dropdownClickToggles(wasOpen, isOpenNow) {
    return wasOpen === isOpenNow
  }

  // Which settings dropdowns have their popup open, and how to shut each one
  // again. The key catcher stands down while any do, so Esc closes the
  // dropdown (not the panel) and the row cursor stops moving under an open
  // popup. A popup whose view is hidden still draws — it is an overlay — so
  // leaving settings closes them.
  property var openDropdowns: ({})

  function noteDropdown(id, open, closeFn) {
    var next = {}
    for (var key in root.openDropdowns) next[key] = root.openDropdowns[key]
    if (open) next[id] = closeFn || true
    else delete next[id]
    root.openDropdowns = next
  }

  function anyDropdownOpen() {
    for (var key in root.openDropdowns) {
      if (root.openDropdowns[key]) return true
    }
    return false
  }

  function closeDropdowns() {
    for (var key in root.openDropdowns) {
      var fn = root.openDropdowns[key]
      if (typeof fn === "function") fn()
    }
    root.openDropdowns = ({})
  }

  // Replace is the one destructive choice: it re-fetches and throws the
  // saved bytes away, and it asks once first.
  function confirmReplace() {
    var url = root.replacePending
    root.ignoringAsk = url
    root.replacePending = ""
    if (url === "") return
    root.runCatch(["replace", url], "Could not replace that download.")
  }

  function cancelReplace() {
    root.replacePending = ""
    root.dismissRowArm()
    root.applyPendingAsk()
  }

  function fraction(d) {
    if (d.status === "done") return 1
    var bytes = root.rowBytes(d)
    if (d.total > 0) return Math.max(0, Math.min(1, bytes / d.total))
    return 0
  }

  function displayName(d) {
    return Model.displayName(d)
  }

  function sizeText(d) {
    if (d.total > 0) return Model.formatBytes(root.rowBytes(d)) + " / " + Model.formatBytes(d.total)
    if (d.bytes > 0) return Model.formatBytes(root.rowBytes(d))
    return d.status === "queued" ? "waiting" : d.status
  }

  // Live speed per row, derived from successive state reads (no backend
  // change needed). Read-only map for bindings; refreshed in reload().
  property var speeds: ({})
  property var speedSeen: ({})
  // id -> the most bytes that row has ever reached, so a reconnect cannot
  // make the progress bar walk backwards.
  property var peakBytes: ({})

  // Row model synced incrementally: flushing state must update roles in
  // place, never rebuild delegates (rebuilds are the visible blink).
  // ListModel lives at root scope so reload() can reach it.
  ListModel {
    id: queueModel
  }

  function rowObject(it) {
    return {
      did: String(it.id || ""),
      url: String(it.url || ""),
      filename: String(it.filename || "download"),
      title: String(it.title || ""),
      kind: String(it.kind || "file"),
      status: String(it.status || "queued"),
      bytes: Number(it.bytes) || 0,
      total: Number(it.total) || 0,
      error: String(it.error || "")
    }
  }

  function setRowChanged(i, obj) {
    var cur = queueModel.get(i)
    if (cur.filename !== obj.filename) queueModel.setProperty(i, "filename", obj.filename)
    if (cur.title !== obj.title) queueModel.setProperty(i, "title", obj.title)
    if (cur.kind !== obj.kind) queueModel.setProperty(i, "kind", obj.kind)
    if (cur.status !== obj.status) queueModel.setProperty(i, "status", obj.status)
    if (cur.bytes !== obj.bytes) queueModel.setProperty(i, "bytes", obj.bytes)
    if (cur.total !== obj.total) queueModel.setProperty(i, "total", obj.total)
    if (cur.error !== obj.error) queueModel.setProperty(i, "error", obj.error)
    if (cur.url !== obj.url) queueModel.setProperty(i, "url", obj.url)
  }

  function syncModel(items) {
    var alive = {}
    for (var j = 0; j < items.length; j++) alive[String(items[j].id)] = true
    for (var r = queueModel.count - 1; r >= 0; r--) {
      if (!alive[queueModel.get(r).did]) queueModel.remove(r)
    }
    for (var k = 0; k < items.length; k++) {
      var obj = rowObject(items[k])
      if (k < queueModel.count && queueModel.get(k).did === obj.did) {
        setRowChanged(k, obj)
      } else {
        var at = -1
        for (var m = 0; m < queueModel.count; m++) {
          if (queueModel.get(m).did === obj.did) { at = m; break }
        }
        if (at >= 0) {
          queueModel.move(at, k, 1)
          setRowChanged(k, obj)
        } else {
          queueModel.insert(k, obj)
        }
      }
    }
    while (queueModel.count > items.length) queueModel.remove(items.length)
  }

  function refreshSpeeds() {
    var now = Date.now()
    var seen = {}
    var out = {}
    var next = {}
    var peaks = {}
    for (var i = 0; i < root.queue.downloads.length; i++) {
      var d = root.queue.downloads[i]
      seen[d.id] = true
      // A reconnect that restarts from zero, or a server that changes its
      // mind about the size, would drag the bar backwards mid-download. The
      // row's high-water mark is what it has actually reached, so the bar
      // only ever moves forward. Rebuilt each pass, so rows that leave the
      // queue take their mark with them.
      peaks[d.id] = Math.max(root.peakBytes[d.id] || 0, Number(d.bytes) || 0)
      if (d.status !== "downloading") continue
      var prev = root.speedSeen[d.id]
      var sample = -1
      if (prev && now > prev.t) {
        sample = Math.max(0, (d.bytes - prev.b) / ((now - prev.t) / 1000))
      }
      var smooth = prev && prev.rate > 0 && sample >= 0 ? prev.rate * 0.65 + sample * 0.35 : sample
      var shown = prev ? prev.shown : ""
      var shownAt = prev ? prev.shownAt : 0
      if (sample >= 0 && (shown === "" || now - shownAt >= 1000)) {
        shown = Model.formatBytes(smooth) + "/s"
        shownAt = now
      }
      if (shown !== "") out[d.id] = shown
      next[d.id] = { b: d.bytes, t: now, rate: smooth < 0 ? 0 : smooth, shown: shown, shownAt: shownAt }
    }
    root.speedSeen = next
    root.speeds = out
    root.peakBytes = peaks
  }

  // Bytes a row has really reached, never less than the file last reported.
  function rowBytes(d) {
    if (!d) return 0
    return Math.max(Number(d.bytes) || 0, root.peakBytes[d.id] || 0)
  }

  function speedFor(id) {
    var s = root.speeds[id]
    return s ? " · " + s : ""
  }

  // Segments diagnostic for a row, looked up from the parsed queue state
  // (not a ListModel role, so it never breaks delegate creation).
  function segmentsFor(id) {
    var q = root.queue
    if (!q || !q.downloads) return null
    for (var i = 0; i < q.downloads.length; i++) {
      if (q.downloads[i].id === id) return q.downloads[i].segments || null
    }
    return null
  }

  // Estimated time remaining for a downloading row, derived from the
  // smoothed speed. Returns "" when unknown.
  function etaFor(id) {
    var d = null
    for (var i = 0; i < root.queue.downloads.length; i++) {
      if (root.queue.downloads[i].id === id) { d = root.queue.downloads[i]; break }
    }
    if (!d || d.status !== "downloading") return ""
    var total = Number(d.total) || 0
    var bytes = root.rowBytes(d)
    if (total <= 0 || bytes >= total) return ""
    var seen = root.speedSeen[id]
    var rate = seen ? Number(seen.rate) : 0
    if (rate <= 0) return ""
    var secs = (total - bytes) / rate
    if (!isFinite(secs) || secs < 0) return ""
    if (secs < 60) return " · " + Math.ceil(secs) + "s left"
    if (secs < 3600) return " · " + Math.floor(secs / 60) + "m " + Math.floor(secs % 60) + "s left"
    return " · " + Math.floor(secs / 3600) + "h " + Math.floor((secs % 3600) / 60) + "m left"
  }

  FileView {
    id: stateFile
    path: root.statePath
    watchChanges: true
    printErrors: false
    onFileChanged: reload()
    onLoaded: root.reload(stateFile.text())
    onLoadFailed: root.reload("")
  }

  Process {
    id: cmdRunner
    running: false
    onExited: function (code) {
      if (code !== 0) {
        root.actionError = root.cmdComplaint
        errorTimer.restart()
      }
      // Force a refresh so renames (and other in-place edits) show up
      // even if the file watcher misses the atomic replace.
      stateFile.reload()
      root.nextCmd()
    }
  }

  Process {
    id: rowRunner
    running: false
    onExited: function (code) {
      if (code !== 0) {
        root.actionError = root.rowComplaint
        errorTimer.restart()
      }
      stateFile.reload()
      root.nextRow()
    }
  }

  Timer {
    id: errorTimer
    interval: 4000
    onTriggered: root.actionError = ""
  }

  // Short leash on row activation right after the conflict dialog closes:
  // long enough to swallow the dismissing Enter, short enough that the
  // next deliberate press works.
  ShellIpc {
    target: root.ipcTarget
    function open(): void { root.open() }
    function close(): void { root.close() }
    function show(): void { root.open() }
    function hide(): void { root.close() }
    function toggle(): void { root.togglePanel() }
  }

  readonly property color foreground: bar ? bar.foreground : Commons.Color.foreground
  readonly property color urgent: bar ? bar.urgent : Commons.Color.urgent
  readonly property color accent: Commons.Color.accent
  // Muted is the palette's own "quiet text" role, so dim labels follow the
  // theme instead of being a hardcoded darkening of the foreground (which
  // went muddy on light themes).
  readonly property color dim: Commons.Color.muted
  readonly property string fontFamily: bar ? bar.fontFamily : Style.font.family

  // sRGB relative luminance, for picking theme ink that reads on a fill.
  function channelLinear(v) {
    return v <= 0.04045 ? v / 12.92 : Math.pow((v + 0.055) / 1.055, 2.4)
  }
  function relLuminance(c) {
    return 0.2126 * root.channelLinear(c.r) + 0.7152 * root.channelLinear(c.g) + 0.0722 * root.channelLinear(c.b)
  }
  function contrastRatio(a, b) {
    var hi = Math.max(a, b)
    var lo = Math.min(a, b)
    return (hi + 0.05) / (lo + 0.05)
  }
  // Theme background or theme foreground, whichever stands further from `fill`.
  function inkOn(fill) {
    var base = root.relLuminance(fill)
    var bg = Commons.Color.background
    var fg = root.foreground
    return root.contrastRatio(base, root.relLuminance(bg)) >= root.contrastRatio(base, root.relLuminance(fg)) ? bg : fg
  }

  // Window rounding is often 0. Fields keep a small radius then, and follow
  // the theme when the theme is already rounder.
  readonly property int fieldRadius: Math.max(Style.cornerRadius, Style.space(4))
  readonly property int fieldButtonInset: Math.max(Style.spacing.md, Math.round(fieldRadius * 0.35))
  readonly property int fieldButtonRadius: Math.max(Style.space(2), fieldRadius - Style.space(2))

  // Badge colour per status: progress and completion read as the accent,
  // failures as urgent, everything else stays quiet.
  function badgeColor(status) {
    if (status === "error") return root.urgent
    if (status === "downloading" || status === "done") return root.accent
    return root.dim
  }

  // Header subtitle: a product label plus whatever the queue is doing, all
  // in the reference's letterspaced small-caps style.
  readonly property string headerMeta: root.conflict !== null
    ? "already in your folder · enter chooses · esc cancels"
    : root.showingSettings
    ? "settings · super+d toggles · esc goes back"
    : "download manager · " + Model.statusText(root.queue).replace(/^Catch — /, "")

  // Where a row's bytes go, with the home directory dropped.
  function folderLine(d) {
    var where = "to " + Model.shortPath(root.downloadDir, root.home)
    return (d.status === "done" || d.status === "error") ? "Saved " + where : "Saving " + where
  }

  implicitWidth: button.implicitWidth
  implicitHeight: button.implicitHeight

  BarIconButton {
    id: button
    anchors.fill: parent
    bar: root.bar
    text: "󰇚"
    active: root.busy
    activeColor: Color.accent
    opacity: 1.0
    tooltipText: Model.statusText(root.queue)
    onPressed: function (b) {
      if (b === Qt.RightButton) root.openFolder()
      else if (b === Qt.MiddleButton) { if (root.bar) root.bar.run("catch list") }
      else root.togglePanel()
    }
  }

  KeyboardPanel {
    id: panel
    anchorItem: button
    owner: root
    bar: root.bar
    open: root.opened
    focusTarget: panel.loadedContent
    contentWidth: panel.fittedContentWidth(Style.space(540))
    contentHeight: panel.fittedContentHeight(panel.loadedContentHeight, Style.space(560))

    content: PanelKeyCatcher {
      id: keyCatcher
      implicitHeight: column.implicitHeight
      anchors.fill: parent
      // Block keys only while a field truly owns them (or the conflict
      // dialog is up): a screen being *visible* must not kill navigation.
      // This is the PanelKeyCatcher contract — blocked hands ALL keys to
      // descendants, so gating it on showingSettings made settings a
      // keyboard dead-end (s/h/esc/tab all swallowed).
      blocked: root.textEditing || root.conflict !== null || root.replacePending !== "" || root.anyDropdownOpen()

      // Hand the keyboard back to the panel. Every field calls this when it
      // commits: setting `field.focus = false` does NOT give up active
      // focus, so the field kept the keyboard, `blocked` stayed true and
      // every shortcut (pause, remove, clear) was dead until the panel was
      // reopened.
      function release() {
        root.textEditing = false
        root.focusZone = "list"
        keyCatcher.forceActiveFocus()
      }

      // Recompute "a field owns the keyboard" from every field, so focus
      // moving between two of them can never leave `blocked` stuck on (which
      // made the panel a keyboard dead-end) or off (which typed row verbs
      // into the field). Lives here because the fields are content-scoped
      // ids, invisible from root scope.
      function syncEditing() {
        root.textEditing = linkField.activeFocus
          || dirField.activeFocus
          || schedStartField.activeFocus
          || schedStopField.activeFocus
      }

      onCloseRequested: {
        // Esc steps back one level: settings → queue, queue → closed.
        if (root.showingSettings) root.toggleSettings()
        else root.close()
      }
      onTabRequested: function (direction) {
        // The home page has two zones: the row list and the link input.
        // Tab walks between them so a link can be typed without the mouse.
        // Settings is a form with no list, so Tab keeps switching panels.
        if (!root.showingSettings && root.conflict === null) {
          if (root.focusZone === "input") keyCatcher.release()
          else keyCatcher.focusInput()
          return
        }
        root.switchPanel(direction)
      }

      // Put the keyboard in the link field. Only reachable from the content
      // scope: linkField is invisible from root scope (see textEditing).
      function focusInput() {
        if (root.showingSettings || root.conflict !== null) return
        root.focusZone = "input"
        linkField.forceActiveFocus()
        if (typeof linkField.selectAll === "function") linkField.selectAll()
      }

      // Paste clipboard text into `field`, at the cursor. Qt binds Ctrl+V
      // natively but nothing binds Super+V, and the two paths disagree, so
      // both combos land here and paste exactly once.
      function pasteIntoField(field) {
        var text = Model.clipboardLink(String(Quickshell.clipboardText || ""))
        if (text === "") {
          root.actionError = "Clipboard holds no link."
          errorTimer.restart()
          return
        }
        // A real paste: drop any selection, insert at the cursor, then leave
        // the cursor after what was inserted.
        var selected = String(field.selectedText || "")
        if (selected.length > 0) field.remove(field.selectionStart, selected.length)
        var at = field.selectionStart
        field.insert(at, text)
        if (typeof field.select === "function") field.select(at, at + text.length)
      }

      // Keys a field handles for itself. The catcher is blocked while a field
      // owns the keyboard, so Tab and paste have to be claimed here.
      function fieldKey(field, event) {
        if (event.key === Qt.Key_Tab || event.key === Qt.Key_Backtab) {
          release()
          event.accepted = true
          return
        }
        if (event.key === Qt.Key_V && (event.modifiers & (Qt.ControlModifier | Qt.MetaModifier))) {
          pasteIntoField(field)
          event.accepted = true
        }
      }

      // Ctrl+V / Super+V from anywhere on the home page: focus the input and
      // paste into it, so a link can be caught without ever touching the
      // field with the mouse.
      function pasteIntoInput() {
        if (root.showingSettings || root.conflict !== null) return
        focusInput()
        pasteIntoField(linkField)
      }

      // Hover counts as attention: NoButton so clicks and wheel pass
      // through to the rows and the scroll view untouched.
      MouseArea {
        anchors.fill: parent
        acceptedButtons: Qt.NoButton
        hoverEnabled: true
        onEntered: root.hold()
      }
      // NOTE: no Keys.onPressed here. PanelKeyCatcher already declares one
      // (Keys.priority BeforeItem) to dispatch every shortcut; a second
      // declaration on the same object replaces that dispatch, which left
      // Tab and the row verbs dead. Keys that arrive while blocked are
      // handled by the fields themselves instead (see linkField).
      onMoveRequested: function (dx, dy) {
        if (root.typing()) return
        root.hold()
        // Horizontal is off for now. One key closed the panel and its twin
        // opened a file, which was easy to hit by accident; the arrow keys
        // came from the same path, so they are off with it. Only the cursor
        // moves now (up/down, j/k).
        if (dx !== 0) return
        // Settings is a form, not the list: the cursor stays put here.
        if (root.showingSettings) return
        if (root.queue.downloads.length === 0) return
        if (root.cursorIndex < 0) root.cursorIndex = 0
        else root.cursorIndex = Math.max(0, Math.min(root.queue.downloads.length - 1, root.cursorIndex + dy))
      }
      onActivateRequested: {
        if (root.typing() || root.showingSettings) return
        root.hold()
        // Space marks. Enter is deliberately inert for now: it used to arm
        // row activation, which opened the finished file — and, until that was
        // fixed, the download folder behind it — while the user was only
        // trying to submit a pasted link. Opening lives on the row's own
        // button, and the folder is `o`.
        var m = root.queue.downloads[root.cursorIndex]
        if (m) root.toggleMark(m.id)
      }
      onReturnRequested: {
        // Enter does nothing. See onActivateRequested above.
      }
      onDeleteRequested: {
        if (root.typing() || root.showingSettings) return
        root.hold()
        var x = root.queue.downloads[root.cursorIndex]
        if (x) root.removeEntry(x.id)
      }
      onTextKey: function (t, modifiers) {
        // Ctrl+V and Super+V paste a link into the input from anywhere on the
        // home page, focusing it on the way. Handled before everything else,
        // and only these two: the other modifier combinations belong to the
        // desktop (Super+C must not land on "delete marked").
        var mod = modifiers || 0
        if ((mod & Qt.ControlModifier) || (mod & Qt.MetaModifier)) {
          if (t === "v" || t === "V" || t === "\u0016") keyCatcher.pasteIntoInput()
          return
        }
        if (t === "s" || t === "S") {
          if (!root.typing()) root.toggleSettings()
          return
        }
        // Queue actions stay off while settings are up or a field is
        // typing — a stray "c" must not clear marks from another screen.
        if (root.typing() || root.showingSettings) return
        root.hold()
        if (t === "c" || t === "C") {
          if (root.markedCount() > 0) root.deleteMarked()
        }
        else if (t === "a" || t === "A") root.toggleAllMarks()
        else if (t === "o" || t === "O") root.openFolder()
        else if (t === "x" || t === "X") {
          // Stop, not remove: the row survives so it can be re-queued.
          var s = root.queue.downloads[root.cursorIndex]
          if (s && root.stoppable(s)) root.cancelEntry(s.id)
        }
        else if (t === "p" || t === "P") {
          var e = root.queue.downloads[root.cursorIndex]
          if (!e) return
          if (root.pausable(e)) root.pauseEntry(e.id)
          else if (root.resumable(e)) root.resumeEntry(e.id)
        }
      }

      Flickable {
              id: contentScroll
              anchors.fill: parent
              contentWidth: width
              contentHeight: column.implicitHeight
              clip: true
              boundsBehavior: Flickable.StopAtBounds
              flickableDirection: Flickable.VerticalFlick
              interactive: contentHeight > height
              ScrollBar.vertical: ScrollBar { policy: ScrollBar.AsNeeded }

              Column {
                id: column
                width: parent.width
                spacing: Style.spacing.xxl

                // ------------------------------------------------------------ head
                // Big title, letterspaced small-caps subtitle, two bare actions.
                Item {
                  width: parent.width
                  height: Math.max(headerLabels.implicitHeight, headerActions.implicitHeight)

                  Column {
                    id: headerLabels
                    anchors.left: parent.left
                    anchors.right: headerActions.left
                    anchors.rightMargin: Style.spacing.lg
                    anchors.verticalCenter: parent.verticalCenter
                    spacing: Style.spacing.xxs

                    Text {
                      textFormat: Text.PlainText
                      text: root.showingSettings ? "Catch settings" : "Catch"
                      color: root.foreground
                      font.family: root.fontFamily
                      font.pixelSize: Style.font.display
                      font.bold: true
                    }

                    Text {
                      width: parent.width
                      textFormat: Text.PlainText
                      text: root.headerMeta.toUpperCase()
                      color: root.dim
                      font.family: root.fontFamily
                      font.pixelSize: Style.font.caption
                      font.bold: true
                      font.letterSpacing: 1.2
                      elide: Text.ElideRight
                    }
                  }

                  Row {
                    id: headerActions
                    anchors.right: parent.right
                    anchors.verticalCenter: parent.verticalCenter
                    spacing: Style.spacing.sm

                    PanelActionButton {
                      iconText: "󰉋"
                      fontSize: Style.font.heading
                      tooltipText: "Open download folder (o)"
                      foreground: root.foreground
                      fontFamily: root.fontFamily
                      focusable: true
                      onClicked: root.openFolder()
                    }

                    PanelActionButton {
                      iconText: "󰒓"
                      fontSize: Style.font.heading
                      tooltipText: root.showingSettings ? "Back to queue (s)" : "Settings (s)"
                      foreground: root.showingSettings ? root.accent : root.foreground
                      fontFamily: root.fontFamily
                      focusable: true
                      onClicked: root.toggleSettings()
                    }
                  }
                }

                Label {
                  visible: root.actionError !== ""
                  width: parent.width
                  text: root.actionError
                  color: root.urgent
                  wrapMode: Text.Wrap
                  font.family: root.fontFamily
                }

                // ------------------------------------------------------ link input
                // One bordered surface holding the field and its filled submit, the
                // way the rest of Omarchy's apps take a link.
                BorderSurface {
                  id: inputBar
                  width: parent.width
                  height: Style.space(46)
                  radius: root.fieldRadius
                  clip: true
                  visible: !root.showingSettings && root.conflict === null
                  color: linkField.activeFocus
                    ? Style.focusFillFor(root.foreground, root.accent)
                    : Style.controlFill(false, false, root.foreground, root.accent)
                  borderSpec: Border.controlSpec(linkField.activeFocus ? "focus" : "normal", root.foreground, root.accent)

                  TextField {
                    id: linkField
                    anchors.left: parent.left
                    anchors.leftMargin: Style.spacing.xl + parent.borderLeft
                    anchors.right: linkGo.left
                    anchors.rightMargin: Style.spacing.sm
                    anchors.verticalCenter: parent.verticalCenter
                    height: parent.height - Style.spacing.md
                    foreground: root.foreground
                    accent: root.accent
                    font.family: root.fontFamily
                    font.pixelSize: Style.font.subtitle
                    horizontalPadding: 0
                    verticalPadding: 0
                    placeholderTextColor: root.dim
                    placeholderText: "Paste download URL…"
                    // The surface above owns the chrome.
                    background: Item {}
                    text: ""
                    Keys.onEscapePressed: function (event) {
                      keyCatcher.release()
                      event.accepted = true
                    }
                    Keys.onPressed: function (event) {
                      // Tab leaves for the row list, Ctrl/Super+V paste: all of it
                      // has to be handled here, because the catcher is blocked
                      // while a field owns the keyboard.
                      keyCatcher.fieldKey(linkField, event)
                    }
                    onActiveFocusChanged: {
                      keyCatcher.syncEditing()
                      if (activeFocus) root.focusZone = "input"
                    }
                    onAccepted: {
                      root.catchUrl(linkField.text.trim())
                      linkField.text = ""
                      if (!root.conflict) keyCatcher.release()
                    }
                  }

                  SolidButton {
                    id: linkGo
                    anchors.right: parent.right
                    anchors.rightMargin: root.fieldButtonInset + parent.borderRight
                    anchors.verticalCenter: parent.verticalCenter
                    height: parent.height - root.fieldButtonInset * 2
                    text: "Download ↵"
                    onClicked: {
                      root.catchUrl(linkField.text.trim())
                      linkField.text = ""
                      if (!root.conflict) keyCatcher.release()
                    }
                  }
                }

                // ---------------------------------------------------- section head
                Item {
                  width: parent.width
                  height: Style.space(22)
                  visible: !root.showingSettings && root.conflict === null

                  Text {
                    anchors.left: parent.left
                    anchors.verticalCenter: parent.verticalCenter
                    textFormat: Text.PlainText
                    text: "Downloads"
                    color: root.foreground
                    font.family: root.fontFamily
                    font.pixelSize: Style.font.title
                    font.bold: true
                  }

                  Text {
                    anchors.right: parent.right
                    anchors.verticalCenter: parent.verticalCenter
                    textFormat: Text.PlainText
                    text: Model.itemWord(root.queue.downloads.length)
                    color: root.dim
                    font.family: root.fontFamily
                    font.pixelSize: Style.font.body
                  }
                }

                Label {
                  visible: !root.showingSettings && root.conflict === null && root.queue.downloads.length === 0
                  width: parent.width
                  text: "Nothing yet — send a download from the browser (Alt+Shift+C)."
                  color: root.dim
                  wrapMode: Text.Wrap
                  font.family: root.fontFamily
                }

                // ----------------------------------------------------------- rows
                Repeater {
                  model: queueModel

                  BorderSurface {
                    id: card
                    required property int index
                    required property string did
                    required property string url
                    required property string filename
                    required property string title
                    required property string kind
                    required property string status
                    required property double bytes
                    required property double total
                    required property string error

                    width: column.width
                    radius: Style.cornerRadius
                    visible: !root.showingSettings && root.conflict === null


                    // Stable proxy over this row's roles: role updates re-run
                    // bindings without recreating the delegate, so progress and
                    // speed glide instead of blinking.
                    readonly property var entry: ({
                      id: did,
                      url: url,
                      filename: filename,
                      title: title,
                      kind: kind,
                      status: status,
                      bytes: bytes,
                      total: total,
                      error: error
                    })
                    readonly property bool isCursor: root.cursorIndex === index
                    readonly property bool isMarked: root.isMarked(did)
                    readonly property bool isOpen: root.expandedRow === did
                    readonly property bool _hot: isCursor || cardHover.hovered
                    readonly property bool _busy: status === "downloading" || status === "queued"

                    implicitHeight: cardBody.implicitHeight + Style.spacing.xl * 2
                    // A marked row has to read as marked without being opened:
                    // the cursor only moves while the keys are in use, but a
                    // mark has to stay visible afterwards. So the mark gets the
                    // tint and the cursor keeps the border.
                    color: isCursor
                      ? Style.selectedFillFor(root.foreground, root.accent)
                      : (isMarked
                        ? Util.alpha(root.accent, 0.10)
                        : (_hot
                          ? Style.hoverFillFor(root.foreground, root.accent)
                          : Style.normalFillFor(root.foreground, root.accent)))
                    borderSpec: Border.controlSpec(isCursor || isMarked ? "selected" : (_hot ? "hover-cursor" : "normal"), root.foreground, root.accent)

                    Behavior on color { ColorAnimation { duration: Style.duration(120) } }

                    // Declared before the body so it sits underneath: it catches
                    // row clicks and hover, while the buttons inside the body keep
                    // their own clicks.
                    MouseArea {
                      id: cardHover
                      anchors.fill: parent
                      hoverEnabled: true
                      acceptedButtons: Qt.LeftButton
                      onClicked: root.cursorIndex = card.index
                    }

                    Column {
                      id: cardBody
                      anchors.left: parent.left
                      anchors.right: parent.right
                      anchors.top: parent.top
                      anchors.margins: Style.spacing.xl
                      spacing: Style.spacing.md

// Anchors, not a layout: the title column is pinned between the glyph and
                      // the badge, so a long title is forced to elide rather than
                      // negotiating width with the badge and painting under it.
                      Item {
                        id: cardHead
                        width: parent.width
                        height: Math.max(cardText.implicitHeight, cardEnd.implicitHeight)

                        Text {
                          id: cardGlyph
                          anchors.left: parent.left
                          anchors.verticalCenter: parent.verticalCenter
                          textFormat: Text.PlainText
                          text: root.kindGlyph(card.entry)
                          color: card.isCursor ? root.accent : root.dim
                          font.family: root.fontFamily
                          font.pixelSize: Style.font.iconLarge
                        }

                        Column {
                          id: cardText
                          anchors.left: cardGlyph.right
                          anchors.leftMargin: Style.spacing.xl
                          anchors.right: cardEnd.left
                          anchors.rightMargin: Style.spacing.xl
                          anchors.verticalCenter: parent.verticalCenter
                          spacing: Style.spacing.xxs

                          Text {
                            width: parent.width
                            textFormat: Text.PlainText
                            text: root.displayName(card.entry)
                            color: root.foreground
                            font.family: root.fontFamily
                            font.pixelSize: Style.font.subtitle
                            font.bold: true
                            elide: Text.ElideMiddle
                          }

                          Text {
                            width: parent.width
                            textFormat: Text.PlainText
                            text: Model.rowMeta(card.entry) + (function(){ var s = root.segmentsFor(card.entry.id); return (s && Number(s.total) > 1) ? " · " + (Number(s.done)||0) + "/" + s.total + " seg" : "" })() + (card._busy && root.fraction(card.entry) > 0 ? " · " + Math.floor(root.fraction(card.entry) * 100) + "%" : "") + root.speedFor(card.entry.id) + root.etaFor(card.entry.id)
                            color: root.dim
                            font.family: root.fontFamily
                            font.pixelSize: Style.font.body
                            elide: Text.ElideRight
                          }
                        }

                        Row {
                          id: cardEnd
                          anchors.right: parent.right
                          anchors.verticalCenter: parent.verticalCenter
                          spacing: Style.spacing.md

                          // Status badge: the accent for progress and
                          // completion, urgent for a failure, quiet for
                          // everything waiting.
                          Rectangle {
                            id: badge
                            visible: !card.isOpen
                            implicitHeight: Style.space(20)
                            implicitWidth: badgeText.implicitWidth + Style.spacing.xl
                            radius: height / 2
                            color: Util.alpha(root.badgeColor(card.status), 0.18)

                            Text {
                              id: badgeText
                              anchors.centerIn: parent
                              textFormat: Text.PlainText
                              text: Model.statusLabel(card.status)
                              color: root.badgeColor(card.status)
                              font.family: root.fontFamily
                              font.pixelSize: Style.font.caption
                              font.bold: true
                            }
                          }

                          // Mark, stop and remove stay on the row itself.
                          // They used to live inside the "..." menu, which is
                          // what made a download look unstoppable and
                          // unremovable: the row showed a badge and a button
                          // whose only job was hiding the rest.
                          PanelActionButton {
                            iconText: card.isMarked ? "󰄲" : "󰄱"
                            tooltipText: card.isMarked ? "Unmark (space)" : "Mark for bulk delete (space)"
                            foreground: card.isMarked ? root.accent : root.dim
                            hoverColor: root.accent
                            fontFamily: root.fontFamily
                            onClicked: root.toggleMark(card.did)
                          }

                          // Stop ends a live transfer and keeps the row, so it
                          // can be re-queued. A cancelled entry resolves to an
                          // abort inside the backend on its next poll.
                          // One toggle: stop while a transfer is live, play
                          // (resume) once it has been stopped. Never an exit
                          // icon here.
                          PanelActionButton {
                            visible: root.stoppable(card.entry) || root.resumable(card.entry)
                            iconText: root.stoppable(card.entry) ? "󰓛" : "󰐊"
                            tooltipText: root.stoppable(card.entry) ? "Stop (x), keeps the row" : "Resume (p)"
                            foreground: root.stoppable(card.entry) ? root.urgent : root.accent
                            hoverColor: root.stoppable(card.entry) ? root.urgent : root.accent
                            fontFamily: root.fontFamily
                            onClicked: root.stoppable(card.entry) ? root.cancelEntry(card.did) : root.resumeEntry(card.did)
                          }

                          // Remove drops the row and keeps the file. On a live
                          // row it stops the transfer on the way out.
                          PanelActionButton {
                            iconText: "󰅖"
                            tooltipText: card._busy ? "Stop and remove (delete)" : "Remove (delete)"
                            foreground: root.dim
                            hoverColor: root.urgent
                            fontFamily: root.fontFamily
                            onClicked: root.removeEntry(card.did)
                          }

                          PanelActionButton {
                            iconText: card.isOpen ? "󰅖" : "•••"
                            tooltipText: card.isOpen ? "Hide actions" : "Row actions"
                            foreground: card.isOpen ? root.accent : root.dim
                            hoverColor: root.accent
                            fontFamily: root.fontFamily
                            onClicked: root.toggleRowActions(card.did)
                          }
                        }
                      }

                      // Progress bar: a clean, visible fill, IDM-style.
                      Rectangle {
                        width: parent.width
                        height: 5
                        radius: 2.5
                        visible: card._busy
                        color: Util.alpha(root.dim, 0.22)

                        Rectangle {
                          width: parent.width * root.fraction(card.entry)
                          height: parent.height
                          radius: 2.5
                          color: card.status === "error" ? root.urgent : root.accent
                          Behavior on width {
                            NumberAnimation { duration: 250; easing.type: Easing.OutCubic }
                          }
                        }
                      }

                      // Segment bar: one block per byte range, IDM-style. Each
                      // block is done (accent), active (accent, partial fill
                      // approximated by overall fraction), or pending (dim).
                      Row {
                        width: parent.width
                        height: 3
                        spacing: 2
                        property var segs: root.segmentsFor(card.entry.id)
                        visible: card._busy && segs && Number(segs.total) > 1

                        Repeater {
                          model: parent.segs ? Number(parent.segs.total) : 0
                          delegate: Rectangle {
                            width: (parent.parent.width - (Number(parent.parent.segs.total) - 1) * 2) / Number(parent.parent.segs.total)
                            height: parent.parent.height
                            radius: 1.5
                            color: {
                              var done = Number(parent.parent.segs.done) || 0
                              if (index < done) return root.accent
                              if (index === done) return Util.alpha(root.accent, 0.55)
                              return Util.alpha(root.dim, 0.18)
                            }
                          }
                        }
                      }

                      Text {
                        width: parent.width
                        textFormat: Text.PlainText
                        visible: card.entry.error === ""
                        text: root.folderLine(card.entry)
                        color: root.dim
                        font.family: root.fontFamily
                        font.pixelSize: Style.font.caption
                        elide: Text.ElideMiddle
                      }

                      Text {
                        width: parent.width
                        textFormat: Text.PlainText
                        visible: card.entry.error !== ""
                        text: card.entry.error
                        color: root.urgent
                        font.family: root.fontFamily
                        font.pixelSize: Style.font.caption
                        wrapMode: Text.Wrap
                      }

                      // Row actions, hidden until "…" asks for them. Only the
                      // ones that are not already on the row itself: mark,
                      // stop and remove stay visible so a row never looks
                      // unstoppable. The keys (space marks, p pauses, x stops,
                      // delete removes) work whether or not this
                      // is open.
                      Flow {
                        width: parent.width
                        spacing: Style.spacing.sm
                        visible: card.isOpen

                        PanelActionButton {
                          visible: root.pausable(card.entry)
                          iconText: "󰏤"
                          tooltipText: "Pause (p)"
                          foreground: root.foreground
                          fontFamily: root.fontFamily
                          onClicked: root.pauseEntry(card.entry.id)
                        }

                        PanelActionButton {
                          visible: root.resumable(card.entry)
                          iconText: "󰐊"
                          tooltipText: "Re-queue (p)"
                          foreground: root.foreground
                          fontFamily: root.fontFamily
                          onClicked: root.resumeEntry(card.entry.id)
                        }

                        PanelActionButton {
                          visible: card.entry.status === "done"
                          iconText: "󰏏"
                          tooltipText: "Open the file"
                          foreground: root.foreground
                          fontFamily: root.fontFamily
                          onClicked: root.openFile(card.entry.id)
                        }
                      }
                    }
                  }
                }

                // ---------------------------------------------------------- foot
                PanelSeparator {
                  width: parent.width
                  foreground: root.foreground
                  strength: 0.1
                  visible: !root.showingSettings && root.conflict === null
                }

                Item {
                  width: parent.width
                  height: Style.space(30)
                  visible: !root.showingSettings && root.conflict === null

                  // A RowLayout, not a Row: children of a positioner may not
                  // be anchored, so vertical centring is a layout attachment.
                  RowLayout {
                    id: shortcutsToggle
                    anchors.fill: parent
                    spacing: Style.spacing.md

                    Rectangle {
                      Layout.preferredWidth: Style.space(20)
                      Layout.preferredHeight: Style.space(20)
                      Layout.alignment: Qt.AlignVCenter
                      radius: Style.cornerRadius
                      color: Util.alpha(root.dim, 0.2)

                      Text {
                        anchors.centerIn: parent
                        textFormat: Text.PlainText
                        text: "?"
                        color: root.dim
                        font.family: root.fontFamily
                        font.pixelSize: Style.font.bodySmall
                        font.bold: true
                      }
                    }

                    Text {
                      Layout.alignment: Qt.AlignVCenter
                      textFormat: Text.PlainText
                      text: "Keyboard shortcuts"
                      color: root.foreground
                      font.family: root.fontFamily
                      font.pixelSize: Style.font.body
                    }

                    Item { Layout.fillWidth: true }

                    Text {
                      Layout.alignment: Qt.AlignVCenter
                      textFormat: Text.PlainText
                      text: "›"
                      // Turns down when the sheet is open.
                      rotation: root.showShortcuts ? 90 : 0
                      color: root.dim
                      font.family: root.fontFamily
                      font.pixelSize: Style.font.title
                      Behavior on rotation {
                        NumberAnimation { duration: Style.duration(120); easing.type: Easing.OutCubic }
                      }
                    }
                  }

                  MouseArea {
                    anchors.fill: parent
                    hoverEnabled: true
                    cursorShape: Qt.PointingHandCursor
                    onClicked: {
                      root.hold()
                      root.showShortcuts = !root.showShortcuts
                    }
                  }
                }

                Column {
                  width: parent.width
                  spacing: Style.spacing.sm
                  visible: !root.showingSettings && root.conflict === null && root.showShortcuts

                  Repeater {
                    model: [
                      { keys: "↑ ↓ / j k", what: "move between rows" },
                      { keys: "space", what: "mark a row · a marks all" },
                      { keys: "c", what: "delete marked (files stay)" },
                      { keys: "p", what: "pause or re-queue" },
                      { keys: "x", what: "stop a download, keep the row" },
                      { keys: "delete", what: "remove a row, file stays" },
                      { keys: "tab", what: "link field ⇄ rows" },
                      { keys: "ctrl+v", what: "paste a link from anywhere" },
                      { keys: "o", what: "open the download folder" },
                      { keys: "s", what: "settings" },
                      { keys: "esc", what: "back, then close" }
                    ]

                    RowLayout {
                      required property var modelData
                      width: column.width
                      height: Style.space(20)
                      spacing: Style.spacing.md

                      Rectangle {
                        Layout.preferredWidth: Math.max(Style.space(52), keyText.implicitWidth + Style.spacing.md)
                        Layout.preferredHeight: Style.space(20)
                        Layout.alignment: Qt.AlignVCenter
                        radius: Style.cornerRadius
                        color: Util.alpha(root.dim, 0.14)

                        Text {
                          id: keyText
                          anchors.centerIn: parent
                          textFormat: Text.PlainText
                          text: modelData.keys
                          color: root.dim
                          font.family: root.fontFamily
                          font.pixelSize: Style.font.caption
                        }
                      }

                      Text {
                        Layout.fillWidth: true
                        Layout.minimumWidth: 0
                        Layout.alignment: Qt.AlignVCenter
                        textFormat: Text.PlainText
                        text: modelData.what
                        color: root.dim
                        font.family: root.fontFamily
                        font.pixelSize: Style.font.caption
                        elide: Text.ElideRight
                      }
                    }
                  }
                }

                // ------------------------------------------------- already saved
                Column {
                  id: conflictView
                  width: parent.width
                  spacing: Style.spacing.lg
                  visible: root.conflict !== null
                  onVisibleChanged: {
                    if (visible) {
                      // Focus lands on "Download again", never on Open. The
                      // panel opens by itself for a question, so Enter is the
                      // first thing pressed — and landing on Open meant the
                      // file launched instead of the user being offered the
                      // rename/replace choice they actually came for.
                      Qt.callLater(function() {
                        if (root.conflict !== null) conflictCopy.forceActiveFocus()
                      })
                    } else {
                      keyCatcher.release()
                    }
                  }

                  Shortcut {
                    enabled: root.conflict !== null && root.opened && root.replacePending === ""
                    sequence: "Esc"
                    context: Qt.ApplicationShortcut
                    onActivated: root.dismissAsk()
                  }
                  Shortcut {
                    enabled: root.conflict !== null && root.opened && root.replacePending === ""
                    sequence: "Ctrl+R"
                    context: Qt.ApplicationShortcut
                    onActivated: root.replaceConflict()
                  }

                  PanelSectionHeader {
                    width: parent.width
                    text: root.conflict && root.conflict.live ? "STILL DOWNLOADING" : "ALREADY IN YOUR FOLDER"
                    foreground: root.dim
                    fontFamily: root.fontFamily
                  }

                  BorderSurface {
                    width: parent.width
                    radius: Style.cornerRadius
                    color: Style.normalFillFor(root.foreground, root.accent)
                    borderSpec: Border.controlSpec("normal", root.foreground, root.accent)
                    implicitHeight: conflictBody.implicitHeight + Style.spacing.xl * 2

                    Column {
                      id: conflictBody
                      anchors.left: parent.left
                      anchors.right: parent.right
                      anchors.top: parent.top
                      anchors.margins: Style.spacing.xl
                      spacing: Style.spacing.lg

                      Text {
                        width: parent.width
                        textFormat: Text.PlainText
                        text: root.conflictTitle
                        color: root.foreground
                        font.family: root.fontFamily
                        font.pixelSize: Style.font.title
                        font.bold: true
                        elide: Text.ElideMiddle
                      }

                      Text {
                        width: parent.width
                        textFormat: Text.PlainText
                        text: root.savedWhere()
                        color: root.dim
                        font.family: root.fontFamily
                        font.pixelSize: Style.font.body
                        elide: Text.ElideMiddle
                        visible: text !== ""
                      }

                      Text {
                        width: parent.width
                        textFormat: Text.PlainText
                        text: root.conflict && root.conflict.live
                          ? "This link is already downloading. Fetch it again as a second copy, or leave the first one alone."
                          : "This file is already in your download folder. Save it again under another name, replace the one you have, or open it."
                        color: root.dim
                        font.family: root.fontFamily
                        font.pixelSize: Style.font.body
                        wrapMode: Text.Wrap
                      }

                      Flow {
                        width: parent.width
                        spacing: Style.spacing.sm

                        SolidButton {
                          id: conflictOpen
                          // Nothing on disk yet: there is no file to open.
                          visible: !(root.conflict && root.conflict.live)
                          text: "Open"
                          onClicked: root.openSaved()
                        }

                        Button {
                          id: conflictCopy
                          radius: root.fieldButtonRadius
                          text: root.conflict && root.conflict.live ? "Download again anyway ↵" : "Download again ↵"
                          tooltipText: "Keep the file you have and save this link again under another name."
                          bordered: true
                          foreground: root.foreground
                          accent: root.accent
                          fontFamily: root.fontFamily
                          focusable: true
                          onClicked: root.copyConflict()
                        }

                        Button {
                          id: conflictReplace
                          radius: root.fieldButtonRadius
                          // Replace means "throw the saved bytes away", and
                          // a transfer still running has none yet.
                          visible: !(root.conflict && root.conflict.live)
                          text: "Replace"
                          tooltipText: "Fetch again, discarding the saved file (ctrl+r)"
                          bordered: true
                          foreground: root.foreground
                          accent: root.accent
                          fontFamily: root.fontFamily
                          focusable: true
                          onClicked: root.replaceConflict()
                        }

                        Button {
                          id: conflictCancel
                          radius: root.fieldButtonRadius
                          text: "Cancel"
                          tooltipText: "esc"
                          bordered: true
                          foreground: root.dim
                          accent: root.dim
                          fontFamily: root.fontFamily
                          focusable: true
                          onClicked: root.dismissAsk()
                        }
                      }
                    }
                  }
                }

                // ------------------------------------------------------- settings
                Column {
                  id: settingsView
                  width: parent.width
                  spacing: Style.spacing.lg
                  visible: root.showingSettings
                  Keys.onEscapePressed: function (event) {
                    root.toggleSettings()
                    event.accepted = true
                  }
                  onVisibleChanged: {
                    if (!visible) {
                      // Focus must never stay trapped in a hidden field, or
                      // every panel shortcut silently stops working. A popup
                      // outlives its hidden parent too, so those go as well.
                      keyCatcher.release()
                      root.closeDropdowns()
                    }
                  }

                  // --------------------------------------------------------- NOT READY YET
                  // The schedule and the "when out of hours" power action are
                  // implemented (crates/catch/src/schedule.rs, `catch
                  // schedule --install`) but not finished: nothing parks a
                  // transfer outside the window yet, so the controls are
                  // hidden rather than offering a switch that does not hold.
                  // Kept here, and out of manifest.json, so the work is not
                  // lost. To bring them back, drop the `visible: false` on
                  // the "when out of hours" row and the SCHEDULE section.

                  PanelSectionHeader {
                    width: parent.width
                    text: "DOWNLOAD FOLDER"
                    foreground: root.dim
                    fontFamily: root.fontFamily
                  }

                  BorderSurface {
                    width: parent.width
                    radius: Style.cornerRadius
                    color: Style.normalFillFor(root.foreground, root.accent)
                    borderSpec: Border.controlSpec("normal", root.foreground, root.accent)
                    implicitHeight: dirBody.implicitHeight + Style.spacing.xl * 2

                    Column {
                      id: dirBody
                      anchors.left: parent.left
                      anchors.right: parent.right
                      anchors.top: parent.top
                      anchors.margins: Style.spacing.xl
                      spacing: Style.spacing.md

                      Text {
                        width: parent.width
                        textFormat: Text.PlainText
                        text: root.downloadDir
                        color: root.foreground
                        font.family: root.fontFamily
                        font.pixelSize: Style.font.body
                        elide: Text.ElideMiddle
                      }

                      BorderSurface {
                        width: parent.width
                        height: Style.space(40)
                        radius: root.fieldRadius
                        clip: true
                        color: dirField.activeFocus
                          ? Style.focusFillFor(root.foreground, root.accent)
                          : Style.controlFill(false, false, root.foreground, root.accent)
                        borderSpec: Border.controlSpec(dirField.activeFocus ? "focus" : "normal", root.foreground, root.accent)

                        TextField {
                          id: dirField
                          anchors.left: parent.left
                          anchors.leftMargin: Style.spacing.lg + parent.borderLeft
                          anchors.right: dirSave.left
                          anchors.rightMargin: Style.spacing.sm
                          anchors.verticalCenter: parent.verticalCenter
                          height: parent.height - Style.spacing.sm
                          foreground: root.foreground
                          accent: root.accent
                          font.family: root.fontFamily
                          horizontalPadding: 0
                          verticalPadding: 0
                          placeholderTextColor: root.dim
                          placeholderText: "Folder path, ~ for home. Empty resets."
                          background: Item {}
                          text: ""
                          Keys.onEscapePressed: function (event) {
                            keyCatcher.release()
                            event.accepted = true
                          }
                          Keys.onPressed: function (event) {
                            keyCatcher.fieldKey(dirField, event)
                          }
                          onActiveFocusChanged: keyCatcher.syncEditing()
                          onAccepted: {
                            root.saveDownloadDir(dirField.text)
                            dirField.text = ""
                            keyCatcher.release()
                          }
                        }

                        SolidButton {
                          id: dirSave
                          anchors.right: parent.right
                          anchors.rightMargin: root.fieldButtonInset + parent.borderRight
                          anchors.verticalCenter: parent.verticalCenter
                          height: parent.height - root.fieldButtonInset * 2
                          text: "Save"
                          onClicked: {
                            root.saveDownloadDir(dirField.text)
                            dirField.text = ""
                            keyCatcher.release()
                          }
                        }
                      }
                    }
                  }

                  PanelSectionHeader {
                    width: parent.width
                    text: "TRANSFERS"
                    foreground: root.dim
                    fontFamily: root.fontFamily
                  }

                  BorderSurface {
                    width: parent.width
                    radius: Style.cornerRadius
                    color: Style.normalFillFor(root.foreground, root.accent)
                    borderSpec: Border.controlSpec("normal", root.foreground, root.accent)
                    implicitHeight: transferBody.implicitHeight + Style.spacing.xl * 2

                    Column {
                      id: transferBody
                      anchors.left: parent.left
                      anchors.right: parent.right
                      anchors.top: parent.top
                      anchors.margins: Style.spacing.xl
                      spacing: Style.spacing.xl

                      RowLayout {
                        id: parallelRow
                        width: parent.width
                        spacing: Style.spacing.xl

                        Text {
                          text: "Parallel downloads"
                          color: root.foreground
                          font.family: root.fontFamily
                          font.pixelSize: Style.font.body
                        }

                        Item { Layout.fillWidth: true; height: 1 }

                        PanelDropdown {
                          id: parallelDrop
                          spaceItem: contentScroll
                          spaceHeight: contentScroll.height
                          Layout.preferredWidth: Style.space(140)
                          Layout.preferredHeight: Style.space(30)
                          Layout.alignment: Qt.AlignVCenter
                          foreground: root.foreground
                          accent: root.accent
                          options: ["1", "3", "5", "10"]
                          value: "" + root.currentParallel()
                          onPopupToggled: function (isOpen) {
                            root.noteDropdown("parallel", isOpen, function () { parallelDrop.close() })
                            if (!isOpen) keyCatcher.release()
                          }
                          onPicked: function (v) {
                            root.writeSetting("maxParallel", parseInt(v, 10))
                          }
                        }
                      }

                      RowLayout {
                        id: segmentsRow
                        width: parent.width
                        spacing: Style.spacing.xl

                        Text {
                          text: "Download segments"
                          color: root.foreground
                          font.family: root.fontFamily
                          font.pixelSize: Style.font.body
                        }

                        Item { Layout.fillWidth: true; height: 1 }

                        PanelDropdown {
                          id: segmentsDrop
                          spaceItem: contentScroll
                          spaceHeight: contentScroll.height
                          Layout.preferredWidth: Style.space(140)
                          Layout.preferredHeight: Style.space(30)
                          Layout.alignment: Qt.AlignVCenter
                          foreground: root.foreground
                          accent: root.accent
                          options: ["1", "2", "4", "8"]
                          value: "" + root.currentSegments()
                          onPopupToggled: function (isOpen) {
                            root.noteDropdown("segments", isOpen, function () { segmentsDrop.close() })
                            if (!isOpen) keyCatcher.release()
                          }
                          onPicked: function (v) {
                            root.writeSetting("segments", parseInt(v, 10))
                          }
                        }
                      }

                      Text {
                        width: parent.width
                        text: "1 disables segmented downloads; larger files download faster on servers that support byte ranges."
                        color: root.dim
                        font.family: root.fontFamily
                        font.pixelSize: Style.font.caption
                        wrapMode: Text.Wrap
                      }

                      // NOT READY: parking the machine once the queue empties needs the same
                      // missing piece as the window below.
                      RowLayout {
                        width: parent.width
                        height: 0
                        spacing: Style.spacing.xl
                        visible: false

                        Text {
                          text: "When out of hours"
                          color: root.foreground
                          font.family: root.fontFamily
                          font.pixelSize: Style.font.body
                        }

                        Item { Layout.fillWidth: true; height: 1 }

                        PanelDropdown {
                          id: whenDoneDrop
                          Layout.preferredWidth: Style.space(140)
                          Layout.preferredHeight: Style.space(30)
                          Layout.alignment: Qt.AlignVCenter
                          foreground: root.foreground
                          accent: root.accent
                          options: [
                            { value: "nothing", label: "stay on" },
                            { value: "suspend", label: "suspend" },
                            { value: "shutdown", label: "shutdown" }
                          ]
                          value: String(root.setting("schedWhenDone", "nothing"))
                          onPopupToggled: function (isOpen) {
                            root.noteDropdown("whenDone", isOpen, function () { whenDoneDrop.close() })
                            if (!isOpen) keyCatcher.release()
                          }
                          onPicked: function (v) { root.writeSetting("schedWhenDone", v) }
                        }
                      }

                      // NOT READY: describes the out-of-hours power action hidden above.
                      Text {
                        id: scheduleHint
                        width: parent.width
                        textFormat: Text.PlainText
                        text: "Suspend sleeps, shutdown powers off once the queue empties."
                        color: root.dim
                        font.family: root.fontFamily
                        font.pixelSize: Style.font.caption
                        wrapMode: Text.Wrap
                        visible: false
                      }
                    }
                  }

                  // NOT READY: the window itself. Hidden with the row above; see the note
                  // at the top of this view.
                  PanelSectionHeader {
                    width: parent.width
                    text: "SCHEDULE"
                    foreground: root.dim
                    fontFamily: root.fontFamily
                    visible: false
                  }

                  BorderSurface {
                    width: parent.width
                    radius: Style.cornerRadius
                    visible: false
                    color: Style.normalFillFor(root.foreground, root.accent)
                    borderSpec: Border.controlSpec("normal", root.foreground, root.accent)
                    implicitHeight: windowBody.implicitHeight + Style.spacing.xl * 2

                    Column {
                      id: windowBody
                      anchors.left: parent.left
                      anchors.right: parent.right
                      anchors.top: parent.top
                      anchors.margins: Style.spacing.xl
                      spacing: Style.spacing.lg

                      PanelDropdown {
                        id: scheduleDrop
                        spaceItem: contentScroll
                        spaceHeight: contentScroll.height
                        width: parent.width
                        height: Style.space(30)
                        foreground: root.foreground
                        accent: root.accent
                        options: [
                          { value: "off", label: "anytime" },
                          { value: "on", label: "scheduled window" }
                        ]
                        value: String(root.setting("schedEnabled", "off"))
                        onPopupToggled: function (isOpen) {
                          root.noteDropdown("schedule", isOpen, function () { scheduleDrop.close() })
                          if (!isOpen) keyCatcher.release()
                        }
                        onPicked: function (v) {
                          root.writeSetting("schedEnabled", v)
                          // The timer is what enforces the window; keep it in
                          // sync so the user never touches the terminal.
                          if (v === "on") root.runCatch(["schedule", "--install"], "Could not install the scheduler timer.")
                          else root.runCatch(["schedule", "--remove"], "Could not remove the scheduler timer.")
                        }
                      }

                      Text {
                        width: parent.width
                        textFormat: Text.PlainText
                        text: "Transfers start inside the window and park outside it."
                        color: root.dim
                        font.family: root.fontFamily
                        font.pixelSize: Style.font.caption
                        wrapMode: Text.Wrap
                      }

                      BorderSurface {
                        width: parent.width
                        height: Style.space(40)
                        radius: root.fieldRadius
                        clip: true
                        color: (schedStartField.activeFocus || schedStopField.activeFocus)
                          ? Style.focusFillFor(root.foreground, root.accent)
                          : Style.controlFill(false, false, root.foreground, root.accent)
                        borderSpec: Border.controlSpec((schedStartField.activeFocus || schedStopField.activeFocus) ? "focus" : "normal", root.foreground, root.accent)

                        // Split point between the two halves. anchors.left/right take an item,
                        // not a number, so the midpoint is a real zero-width item
                        // that both fields and the rule anchor to.
                        Item {
                          id: schedMid
                          anchors.horizontalCenter: parent.horizontalCenter
                          width: 0
                          height: 1
                        }

                        TextField {
                          id: schedStartField
                          anchors.left: parent.left
                          anchors.leftMargin: Style.spacing.lg + parent.borderLeft
                          anchors.right: schedMid.left
                          anchors.rightMargin: Style.spacing.md
                          anchors.verticalCenter: parent.verticalCenter
                          height: parent.height - Style.spacing.sm
                          foreground: root.foreground
                          accent: root.accent
                          font.family: root.fontFamily
                          horizontalPadding: 0
                          verticalPadding: 0
                          placeholderTextColor: root.dim
                          placeholderText: "From " + String(root.setting("schedStart", "02:00"))
                          background: Item {}
                          text: ""
                          Keys.onEscapePressed: function (event) {
                            keyCatcher.release()
                            event.accepted = true
                          }
                          Keys.onPressed: function (event) {
                            keyCatcher.fieldKey(schedStartField, event)
                          }
                          onActiveFocusChanged: keyCatcher.syncEditing()
                          onAccepted: {
                            root.saveSchedule(schedStartField.text.trim(), schedStopField.text.trim())
                            schedStartField.text = ""
                            schedStopField.text = ""
                            keyCatcher.release()
                          }
                        }

                        Rectangle {
                          anchors.horizontalCenter: schedMid.horizontalCenter
                          anchors.verticalCenter: parent.verticalCenter
                          width: 1
                          height: parent.height - Style.spacing.xl
                          color: Util.alpha(root.dim, 0.3)
                        }

                        TextField {
                          id: schedStopField
                          anchors.left: schedMid.right
                          anchors.leftMargin: Style.spacing.md
                          anchors.right: schedSave.left
                          anchors.rightMargin: Style.spacing.sm
                          anchors.verticalCenter: parent.verticalCenter
                          height: parent.height - Style.spacing.sm
                          foreground: root.foreground
                          accent: root.accent
                          font.family: root.fontFamily
                          horizontalPadding: 0
                          verticalPadding: 0
                          placeholderTextColor: root.dim
                          placeholderText: "Until " + String(root.setting("schedStop", "06:00"))
                          background: Item {}
                          text: ""
                          Keys.onEscapePressed: function (event) {
                            keyCatcher.release()
                            event.accepted = true
                          }
                          Keys.onPressed: function (event) {
                            keyCatcher.fieldKey(schedStopField, event)
                          }
                          onActiveFocusChanged: keyCatcher.syncEditing()
                          onAccepted: {
                            root.saveSchedule(schedStartField.text.trim(), schedStopField.text.trim())
                            schedStartField.text = ""
                            schedStopField.text = ""
                            keyCatcher.release()
                          }
                        }

                        SolidButton {
                          id: schedSave
                          anchors.right: parent.right
                          anchors.rightMargin: root.fieldButtonInset + parent.borderRight
                          anchors.verticalCenter: parent.verticalCenter
                          height: parent.height - root.fieldButtonInset * 2
                          text: "Save"
                          onClicked: {
                            root.saveSchedule(schedStartField.text.trim(), schedStopField.text.trim())
                            schedStartField.text = ""
                            schedStopField.text = ""
                            keyCatcher.release()
                          }
                        }
                      }
                    }
                  }
                }
              }
            }

            // Destructive replace asks first. A sibling over the content that
            // owns the keyboard while it is up — the catcher is blocked for
            // exactly this case (see `blocked`), so the keys reach here.
            Item {
              id: replaceShade
              anchors.fill: parent
              visible: root.replacePending !== ""
              focus: visible
              // 1 is Replace, the destructive choice, preselected the way
              // every other confirm in the shell is.
              property int choice: 1

              onVisibleChanged: {
                if (visible) choice = 1
                else keyCatcher.release()
              }

              function choose() {
                if (choice === 0) root.cancelReplace()
                else root.confirmReplace()
              }

              Keys.priority: Keys.BeforeItem
              Keys.onPressed: function (event) {
                if (event.key === Qt.Key_Escape) {
                  root.cancelReplace()
                  event.accepted = true
                } else if (event.key === Qt.Key_Left || event.key === Qt.Key_Right
                           || event.key === Qt.Key_Tab || event.key === Qt.Key_Backtab) {
                  choice = choice === 0 ? 1 : 0
                  event.accepted = true
                } else if (event.key === Qt.Key_Return || event.key === Qt.Key_Enter) {
                  choose()
                  event.accepted = true
                }
              }

              Rectangle {
                anchors.fill: parent
                color: Util.alpha(Commons.Color.background, 0.7)

                MouseArea { anchors.fill: parent; onClicked: root.cancelReplace() }

                BorderSurface {
                  anchors.centerIn: parent
                  width: Math.min(parent.width - Style.space(32), Style.space(370))
                  radius: root.fieldRadius
                  color: Commons.Color.background
                  borderSpec: Border.controlSpec("normal", root.foreground, root.accent)
                  implicitHeight: replaceBody.implicitHeight + Style.spacing.xl * 2

                  MouseArea { anchors.fill: parent; onClicked: {} }

                  Column {
                    id: replaceBody
                    anchors.left: parent.left
                    anchors.right: parent.right
                    anchors.top: parent.top
                    anchors.margins: Style.spacing.xl
                    spacing: Style.spacing.lg

                    Text {
                      width: parent.width
                      textFormat: Text.PlainText
                      text: "Replace the saved file?\n\nCatch fetches the link again and throws away the copy you have."
                      color: root.foreground
                      font.family: root.fontFamily
                      font.pixelSize: Style.font.title
                      wrapMode: Text.Wrap
                    }

                    // The shell confirm paints its selected action in the
                    // theme's urgent color. Here that color is the same sage
                    // as the accent, so "Replace" sat on a wash of itself.
                    // These two keep ordinary panel ink.
                    Row {
                      anchors.right: parent.right
                      spacing: Style.spacing.sm

                      Button {
                        text: "Keep it"
                        radius: root.fieldButtonRadius
                        bordered: true
                        // Cursor, not `selected`: selected text follows the
                        // theme's selected color, which on a filled control
                        // is the same low-contrast sage as the old dialog.
                        hasCursor: replaceShade.choice === 0
                        foreground: root.foreground
                        accent: root.accent
                        fontFamily: root.fontFamily
                        fontSize: Style.font.subtitle
                        focusable: true
                        onClicked: root.cancelReplace()
                      }

                      SolidButton {
                        text: "Replace"
                        marked: replaceShade.choice === 1
                        onClicked: root.confirmReplace()
                      }
                    }
                  }
                }
              }
            }
    }
  }
}
