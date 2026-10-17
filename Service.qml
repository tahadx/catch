import QtQuick
import Quickshell
import Quickshell.Io

// Catch background service: watches the state file and wakes the queue
// drainer (bounded by the maxParallel setting) whenever anything changes.
// The queue itself is owned by the `catch` CLI; this item never writes it.
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

  // First-run bootstrap: the plugin id is stable, so the installed path is
  // deterministic. Links backend binaries and writes browser hosts, then
  // the watcher below drains. (Forks: update the id in this path.)
  Component.onCompleted: {
    configureProcess.command = [(Quickshell.env("HOME") || "") + "/.config/omarchy/plugins/io.github.tahadx.catch/bin/configure"]
    configureProcess.running = true
  }

  function maybeDrain() {
    if (drainProcess.running) return
    drainProcess.command = ["catch", "drain"]
    drainProcess.running = true
  }

  FileView {
    id: watcher
    path: root.statePath
    watchChanges: true
    printErrors: false
    onFileChanged: watcher.reload()
    onLoaded: {
      root.stateText = watcher.text()
      root.maybeDrain()
    }
    onLoadFailed: root.stateText = ""
  }

  Process {
    id: configureProcess
    running: false
    onExited: root.maybeDrain()
  }

  Process {
    id: drainProcess
    running: false
  }
}
