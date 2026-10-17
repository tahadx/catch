.pragma library

// Pure helpers for Catch state. No QML imports: testable logic only.
//
// State file: $XDG_STATE_HOME/omarchy/catch/state.json, e.g.
// {"version":1,"downloads":[
//   {"id":"1728991234-1","url":"https://…","filename":"video.mp4",
//    "status":"downloading","bytes":4200000,"total":9000000,"error":""}]}
// Statuses: queued | downloading | paused | done | error | cancelled

function empty() {
  return { version: 1, downloads: [], saved: {}, asks: [] }
}

function parse(text) {
  var raw = String(text || "").trim()
  if (raw.length === 0) return empty()
  var data
  try {
    data = JSON.parse(raw)
  } catch (e) {
    return empty()
  }
  // The saved index outlives the queue rows: it is what still knows a link
  // was downloaded after `catch clear` or a remove.
  var savedRaw = data && typeof data.saved === "object" && data.saved ? data.saved : null
  var saved = {}
  if (savedRaw) {
    for (var key in savedRaw) {
      var rec = savedRaw[key]
      if (!rec || typeof rec !== "object") continue
      var path = String(rec.path || "")
      if (path === "") continue
      saved[String(key)] = {
        path: path,
        kind: String(rec.kind || "file"),
        filename: path.split("/").pop(),
        bytes: Number(rec.bytes) || 0,
        at: Number(rec.at) || 0,
      }
    }
  }
  var list = data && data.downloads
  if (!Array.isArray(list)) return { version: 1, downloads: [], saved: saved, asks: parseAsks(data) }
  var out = []
  for (var i = 0; i < list.length; i++) {
    var d = list[i]
    if (!d || typeof d !== "object") continue
    out.push({
      id: String(d.id || ""),
      url: String(d.url || ""),
      filename: String(d.filename || "download"),
      title: String(d.title || ""),
      kind: String(d.kind || "file"),
      status: String(d.status || "queued"),
      bytes: Number(d.bytes) || 0,
      total: Number(d.total) || 0,
      error: String(d.error || "")
    })
  }
  return { version: 1, downloads: out, saved: saved, asks: parseAsks(data) }
}

// Questions waiting on the panel: a link whose file is already in the
// download folder, or one whose transfer is still running. Entries without
// a url, or without a path and without being live, cannot be acted on.
function parseAsks(data) {
  var list = data && data.asks
  if (!Array.isArray(list)) return []
  var out = []
  for (var i = 0; i < list.length; i++) {
    var a = list[i]
    if (!a || typeof a !== "object") continue
    var url = String(a.url || "")
    var path = String(a.path || "")
    if (url === "") continue
    // A transfer still running has no file yet, so it carries no path and
    // is not held to the path rules. Only a saved question is.
    var live = a.live === true || a.live === "true"
    if (!live) {
      if (path === "") continue
      if (path.indexOf("..") >= 0 || path.charAt(0) === "/" || path.indexOf("\\") >= 0) continue
    }
    out.push({
      url: url,
      title: String(a.title || ""),
      filename: String(a.filename || path.split("/").pop()),
      kind: String(a.kind || "file"),
      path: path,
      bytes: Number(a.bytes) || 0,
      live: live,
    })
  }
  return out
}

function activeDownloads(state) {
  var out = []
  var list = state ? state.downloads : []
  for (var i = 0; i < list.length; i++) {
    if (list[i].status === "downloading" || list[i].status === "queued") out.push(list[i])
  }
  return out
}

function activeCount(state) {
  return activeDownloads(state).length
}

function totalCount(state) {
  return state && state.downloads ? state.downloads.length : 0
}

function hasActive(state) {
  return activeCount(state) > 0
}

function overallFraction(state) {
  var act = activeDownloads(state)
  var bytes = 0, total = 0
  for (var i = 0; i < act.length; i++) {
    bytes += act[i].bytes
    total += act[i].total
  }
  if (total <= 0) return act.length > 0 ? -1 : 0
  return Math.max(0, Math.min(1, bytes / total))
}

function grouped(n) {
  var s = String(Math.max(0, Math.floor(Number(n) || 0)))
  var out = ""
  while (s.length > 3) {
    out = "," + s.slice(-3) + out
    s = s.slice(0, -3)
  }
  return s + out
}

function formatBytes(n) {
  var v = Number(n) || 0
  if (v < 1024) return Math.floor(v) + " B"
  var units = ["KB", "MB", "GB", "TB"]
  var u = -1
  do {
    v /= 1024
    u++
  } while (v >= 1024 && u < units.length - 1)
  return (v >= 100 ? Math.round(v) : Math.round(v * 10) / 10) + " " + units[u]
}

// What a row (and the bar) calls a download: the title once it lands, a
// loading note for video links still resolving, else the file name.
function displayName(d) {
  if (!d) return ""
  var title = String(d.title || "")
  if (title !== "") return title
  var status = String(d.status || "")
  if (String(d.kind || "file") === "video" && (status === "queued" || status === "downloading")) {
    return "fetching video info…"
  }
  return String(d.filename || "download")
}

// The finished entry for this exact URL, when Catch already caught it —
// the panel asks before fetching a quiet second copy.
function doneFor(queue, url) {
  var list = (queue && queue.downloads) || []
  for (var i = 0; i < list.length; i++) {
    var d = list[i]
    if (String(d.url || "") === url && String(d.status || "") === "done") return d
  }
  return null
}

// The badge on a card: a word, not a status code.
function statusLabel(status) {
  switch (String(status || "")) {
    case "downloading": return "Downloading"
    case "paused": return "Paused"
    case "done": return "Completed"
    case "error": return "Error"
    case "cancelled": return "Cancelled"
    default: return "Queued"
  }
}

// The type beside the size: the file's own extension, or the kind when the
// name has none worth showing ("Video" for a stream still resolving).
function kindLabel(d) {
  if (!d) return ""
  var name = String(d.filename || "")
  var dot = name.lastIndexOf(".")
  var slash = Math.max(name.lastIndexOf("/"), name.lastIndexOf("\\"))
  // Needs a stem before the dot, else ".hidden" reads as a HIDDEN file.
  if (dot > slash + 1 && dot < name.length - 1) return name.slice(dot + 1).toUpperCase()
  return String(d.kind || "file") === "video" ? "Video" : "File"
}

// A card's second line: "APK · 6 MB", "MP4 · 3 MB / 9 MB" while it is still
// going, and the state alone when no bytes are known yet.
function rowMeta(d) {
  if (!d) return ""
  var status = String(d.status || "")
  var bytes = Number(d.bytes) || 0
  var total = Number(d.total) || 0
  var parts = [kindLabel(d)]
  if (bytes > 0 && total > bytes) parts.push(formatBytes(bytes) + " / " + formatBytes(total))
  else if (bytes > 0) parts.push(formatBytes(bytes))
  else parts.push(statusLabel(status))
  return parts.join(" · ")
}

// "1 item" / "1,234 items" for the section header.
function itemWord(n) {
  var count = Math.max(0, Math.floor(Number(n) || 0))
  return grouped(count) + (count === 1 ? " item" : " items")
}

// A path with the home directory dropped, the way the panel shows where a
// download landed: "Downloads/catch" rather than "/home/you/Downloads/catch".
function shortPath(path, home) {
  var p = String(path || "")
  var h = String(home || "")
  if (h !== "" && p.indexOf(h + "/") === 0) return p.slice(h.length + 1)
  return p
}

// What Catch already saved for a link. The saved index answers it, and it
// outlives the queue rows: clearing the queue keeps the files, so this still
// says "you already have this one" where doneFor cannot. Links caught before
// the index existed fall back to their finished row, so they are not asked
// about twice either.
function savedFor(queue, url) {
  var rec = ((queue && queue.saved) || {})[url]
  var row = doneFor(queue, url)
  if (!rec) {
    if (!row) return null
    return {
      url: url,
      id: row.id,
      filename: String(row.filename || ""),
      title: String(row.title || ""),
      kind: String(row.kind || "file"),
      path: "",
      bytes: Number(row.bytes) || 0,
      at: 0,
    }
  }
  return {
    url: url,
    id: row ? row.id : "",
    filename: String(rec.filename || rec.path || ""),
    title: row ? String(row.title || "") : "",
    kind: String(rec.kind || "file"),
    path: String(rec.path || ""),
    bytes: Number(rec.bytes) || 0,
    at: Number(rec.at) || 0,
  }
}

// The link a clipboard paste should put in the input: the whole text when
// it is already a link, otherwise the first http(s) link inside it, so
// pasting a line of chat around the URL still catches that URL. Empty when
// there is nothing to catch.
function clipboardLink(text) {
  var raw = String(text || "").trim()
  if (raw === "") return ""
  if (/^https?:\/\/\S+$/i.test(raw)) return raw
  var found = raw.match(/https?:\/\/[^\s<>"'()\[\]]+/i)
  if (!found) return ""
  // Trailing punctuation picked up from prose, not part of the link.
  return found[0].replace(/[.,;:!?]+$/, "")
}

function statusText(state) {
  var active = activeDownloads(state)
  if (active.length === 0) {
    var total = totalCount(state)
    return total === 0 ? "Catch — no downloads yet" : "Catch — " + grouped(total) + " finished"
  }
  var first = active[0]
  var frac = overallFraction(state)
  var pct = frac >= 0 ? " · " + Math.floor(frac * 100) + "%" : ""
  var more = active.length > 1 ? " (+" + (active.length - 1) + " more)" : ""
  return "Catch — " + displayName(first) + pct + more
}

function defaultDownloadDir(home) {
  return (home || "") + "/Downloads/catch"
}

// "~" and "~/…" mean the home directory. "~someone" is left as typed:
// Catch does not look up other accounts.
function expandHome(path, home) {
  var p = String(path || "").trim()
  var h = String(home || "")
  if (p === "~") return h
  if (p.indexOf("~/") === 0) return h + p.slice(1)
  return p
}
