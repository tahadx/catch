import QtQuick
import Quickshell
import Quickshell.Io
import "Model.js" as Model

// Catch background service: watches the state file and wakes the queue
// drainer (bounded by the maxParallel setting) when the *queue shape*
// changes. Progress writes every few hundred milliseconds update bytes, but
// they must not spawn a drainer each: that turned the background service
// into a constant fork storm. The queue itself is owned by the `catch` CLI;
// this item never writes it.
Item {
  id: root
  visible: false

  property var shell: null
  property var settings: ({})

  readonly property string home: Quickshell.env("HOME") || ""
  readonly property string stateDir: (Quickshell.env("XDG_STATE_HOME") || root.home + "/.local/state") + "/omarchy/catch"
  readonly property string statePath: root.stateDir + "/state.json"

  // drain reads its settings straight from shell.json; catch is the only
  // backend this panel drives.
  property string stateText: ""

  // Last scheduling shape seen (queued/downloading rows). Only a change here
  // needs a drainer; a byte counter moving does not.
  property string lastSchedule: ""
  // Parsed state, only for the next one-time due time. The backend still
  // decides whether a row may start.
  property var queue: Model.empty()
  // True once backend configuration (binary links, host manifests) has
  // completed. Drains must not run before that.
  property bool configured: false
  // An event landed while a drainer was already running. Re-check once when
  // it exits instead of losing the wake-up.
  property bool drainPending: false

  // First-run bootstrap: the plugin id is stable, so the installed path is
  // deterministic. Links backend binaries and writes browser hosts, then
  // the watcher below drains. (Forks: update the id in this path.)
  Component.onCompleted: {
    configureProcess.command = [(Quickshell.env("HOME") || "") + "/.config/omarchy/plugins/io.github.tahadx.catch/bin/configure"]
    configureProcess.running = true
  }

  // Evaluate at most every 300 ms: a burst of state writes is coalesced,
  // without the restarted-timer starvation that frequent progress flushes
  // would cause.
  function evaluate() {
    if (!root.configured) return
    root.queue = Model.parse(root.stateText)
    root.armDue()
    var next = Model.scheduleSignature(root.queue)
    if (next === root.lastSchedule) return
    root.lastSchedule = next
    root.maybeDrain()
  }

  // The systemd timer checks every minute; while the shell is running this
  // timer makes a one-time start land on its minute instead of up to a
  // minute late. It fires `tick`, never a start of its own: the window,
  // parallel limit and pause rules stay in the backend.
  function armDue() {
    dueTimer.stop()
    var now = Math.floor(Date.now() / 1000)
    var due = Model.nextDue(root.queue, now)
    if (!due) return
    var ms = Math.max(250, (due - now) * 1000)
    // QML timers take a 32-bit millisecond interval; one-time times further
    // out than ~24 days re-arm when they fire early.
    dueTimer.interval = Math.min(ms, 2147483000)
    dueTimer.start()
  }

  function dueFired() {
    if (drainProcess.running) {
      root.drainPending = true
      return
    }
    root.drainPending = false
    drainProcess.command = ["catch", "tick"]
    drainProcess.running = true
  }

  // Force one scheduling evaluation, ignoring the last signature. Used after
  // configuration completes so a queued item drains even when the signature
  // has not changed since the (deferred) first read.
  function forceEvaluate() {
    if (!root.configured) return
    root.lastSchedule = ""
    root.evaluate()
  }

  function maybeDrain() {
    if (drainProcess.running) {
      root.drainPending = true
      return
    }
    root.drainPending = false
    drainProcess.command = ["catch", "drain"]
    drainProcess.running = true
  }

  Timer {
    id: evaluateTimer
    interval: 300
    repeat: false
    onTriggered: root.evaluate()
  }

  Timer {
    id: dueTimer
    repeat: false
    onTriggered: root.dueFired()
  }

  FileView {
    id: watcher
    path: root.statePath
    watchChanges: true
    printErrors: false
    onFileChanged: watcher.reload()
    onLoaded: {
      root.stateText = watcher.text()
      if (root.configured) {
        if (!evaluateTimer.running) evaluateTimer.start()
        root.armDue()
      }
    }
    onLoadFailed: {
      root.stateText = ""
      root.lastSchedule = ""
      root.queue = Model.empty()
      dueTimer.stop()
    }
  }

  Process {
    id: configureProcess
    running: false
    onExited: {
      // Configuration may have (re)installed the binaries; mark ready and
      // force one scheduling evaluation even if the signature is unchanged.
      root.configured = true
      root.forceEvaluate()
    }
  }

  Process {
    id: drainProcess
    running: false
    onExited: {
      // A capped timer (a one-time time more than ~24 days out) fired early;
      // re-arm for what is still ahead. The state watcher also re-arms when
      // the tick changed the queue.
      root.armDue()
      if (root.drainPending) root.maybeDrain()
    }
  }
}
