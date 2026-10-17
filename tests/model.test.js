#!/usr/bin/env node
// Model.js is plain logic by design; run it under node with the QML
// `.pragma library` directive stripped. Usage: node tests/model.test.js
"use strict"

const assert = require("node:assert")
const fs = require("node:fs")
const path = require("node:path")

const src = fs
  .readFileSync(path.join(__dirname, "..", "Model.js"), "utf8")
  .replace(/^\.pragma library\s*$/m, "")
const Model = new Function(src + "\nreturn { parse, displayName, statusText, doneFor, savedFor, empty, clipboardLink, statusLabel, kindLabel, rowMeta, itemWord, shortPath, expandHome, scheduleSignature, safeRelativePath }")()

assert.strictEqual(Model.expandHome("~", "/home/you"), "/home/you")
assert.strictEqual(Model.expandHome("~/Downloads/catch", "/home/you"), "/home/you/Downloads/catch")
assert.strictEqual(Model.expandHome("~ ", "/home/you"), "/home/you")
assert.strictEqual(Model.expandHome("~other/x", "/home/you"), "~other/x")
assert.strictEqual(Model.expandHome("/tmp/catch", "/home/you"), "/tmp/catch")

// Questions the panel shows when a link is already in the download folder.
const asked = Model.parse(JSON.stringify({
  version: 1,
  downloads: [],
  asks: [
    { url: "https://x.test/a", title: "App", filename: "app.apk", kind: "file", path: "programs/app.apk", bytes: 4 },
    { url: "", path: "nope.zip" },
    { url: "https://x.test/escape", path: "../outside.zip" },
    { path: "programs/x.zip" },
  ],
}))
assert.strictEqual(asked.asks.length, 1, "only a real, in-folder path is a question")
assert.strictEqual(asked.asks[0].filename, "app.apk")
assert.strictEqual(asked.asks[0].path, "programs/app.apk")
assert.strictEqual(Model.empty().asks.length, 0)

// Path checks are component-wise. A ".." *inside* a name is not traversal.
for (const ok of ["programs/app.apk", "a..b/c.zip", "Notes..final.pdf", "dir/.hidden", "dir/..name"]) {
  assert.strictEqual(Model.safeRelativePath(ok), true, "accepted " + ok)
}
for (const bad of ["", "/etc/passwd", "../outside.zip", "a/../../b", "a/..", "..", "a\\b.zip"]) {
  assert.strictEqual(Model.safeRelativePath(bad), false, "rejected " + bad)
}
const traversal = Model.parse(JSON.stringify({
  version: 1,
  downloads: [],
  asks: [
    { url: "https://x.test/ok", path: "a..b/c.zip" },
    { url: "https://x.test/up", path: "a/../../b" },
  ],
}))
assert.strictEqual(traversal.asks.length, 1, "only the real path survives")
assert.strictEqual(traversal.asks[0].path, "a..b/c.zip")

function state(download) {
  return Model.parse(JSON.stringify({ version: 1, downloads: [download] }))
}

// kind must survive the state transform: the row's placeholder logic
// needs it (a dropped kind is how "watch" leaked through before).
const video = {
  id: "1",
  url: "https://www.youtube.com/watch?v=x",
  filename: "watch",
  title: "",
  kind: "video",
  status: "downloading",
  bytes: 0,
  total: 0,
  error: "",
}
const parsed = state(video)
assert.strictEqual(parsed.downloads[0].kind, "video")
assert.strictEqual(Model.displayName(parsed.downloads[0]), "fetching video info…")

// The title wins the moment yt-dlp reports it.
const titled = state({ ...video, title: "Me at the zoo" })
assert.strictEqual(Model.displayName(titled.downloads[0]), "Me at the zoo")

// File downloads keep their filename.
assert.strictEqual(Model.displayName({ filename: "f.zip", kind: "file", status: "downloading" }), "f.zip")

// Done video rows fall back to the on-disk name, never a loading note.
assert.strictEqual(Model.displayName({ filename: "Me at the zoo.webm", kind: "video", status: "done", title: "" }), "Me at the zoo.webm")

// The bar summary uses the same names.
assert.ok(Model.statusText(parsed).includes("fetching video info…"), "bar text shows the loading note")
assert.ok(Model.statusText(titled).includes("Me at the zoo"), "bar text shows the title")

// The conflict probe: the finished entry for the exact same link.
const caught = Model.parse(
  JSON.stringify({
    version: 1,
    downloads: [
      {
        id: "done-1",
        url: "https://x.test/a",
        filename: "a.zip",
        title: "",
        kind: "file",
        status: "done",
        bytes: 9,
        total: 9,
        error: "",
      },
      {
        id: "live-1",
        url: "https://x.test/b",
        filename: "b.zip",
        title: "",
        kind: "file",
        status: "downloading",
        bytes: 1,
        total: 9,
        error: "",
      },
    ],
  }),
)
assert.strictEqual(Model.doneFor(caught, "https://x.test/a").id, "done-1")
assert.strictEqual(Model.doneFor(caught, "https://x.test/b"), null, "active rows are not conflicts")
assert.strictEqual(Model.doneFor(caught, "https://x.test/missing"), null)
assert.strictEqual(Model.doneFor(Model.empty(), "https://x.test/a"), null)

// Clipboard paste into the input: the link, not the surrounding chat.
assert.strictEqual(Model.clipboardLink("https://x.test/a"), "https://x.test/a")
assert.strictEqual(Model.clipboardLink("  https://x.test/a\n"), "https://x.test/a")
assert.strictEqual(Model.clipboardLink("HTTP://x.test/A"), "HTTP://x.test/A")
assert.strictEqual(
  Model.clipboardLink("look at https://x.test/a.zip it's good"),
  "https://x.test/a.zip",
)
// Trailing prose punctuation is not part of the link.
assert.strictEqual(Model.clipboardLink("see https://x.test/a."), "https://x.test/a")
assert.strictEqual(Model.clipboardLink("https://x.test/a, then"), "https://x.test/a")
assert.strictEqual(Model.clipboardLink(""), "")
assert.strictEqual(Model.clipboardLink("   \n "), "")
assert.strictEqual(Model.clipboardLink("no link here"), "")
assert.strictEqual(Model.clipboardLink("ftp://x.test/a"), "")

// The saved index is what still knows a link after the queue is cleared.
const withSaved = Model.parse(JSON.stringify({
  version: 1,
  downloads: [
    { id: "gone", url: "https://x.test/a", filename: "a.zip", title: "", kind: "file", status: "done", bytes: 9, total: 9, error: "" },
  ],
  saved: {
    "https://x.test/a": { path: "a.zip", kind: "file", bytes: 9, at: 1700000000 },
    "https://youtu.be/v": { path: "video/clip.webm", kind: "video", bytes: 42, at: 1700000001 },
  },
}))

// The queue was cleared: no rows at all, but the link is still known.
const cleared = Model.parse(JSON.stringify({
  version: 1,
  downloads: [],
  saved: { "https://x.test/a": { path: "a.zip", kind: "file", bytes: 9, at: 1 } },
}))

assert.strictEqual(Model.doneFor(cleared, "https://x.test/a"), null, "no row once cleared")
const stillKnown = Model.savedFor(cleared, "https://x.test/a")
assert.ok(stillKnown, "a cleared link is still recognised")
assert.strictEqual(stillKnown.filename, "a.zip")
assert.strictEqual(stillKnown.kind, "file")
assert.strictEqual(Model.savedFor(cleared, "https://x.test/unknown"), null)

// With the row present, the dialog gets its id and title.
const withRow = Model.savedFor(withSaved, "https://x.test/a")
assert.strictEqual(withRow.id, "gone", "the row id is offered when there is one")
assert.strictEqual(withRow.path, "a.zip")

// A saved video keeps its kind, so the dialog offers copy rather than rename.
const vid = Model.savedFor(withSaved, "https://youtu.be/v")
assert.strictEqual(vid.kind, "video")
assert.strictEqual(vid.filename, "clip.webm", "filename comes off the recorded path")

// Garbage index entries are dropped rather than offered.
const junk = Model.parse(JSON.stringify({ version: 1, downloads: [], saved: { u: null, v: {}, w: { path: "" } } }))
assert.strictEqual(Model.savedFor(junk, "u"), null)
assert.strictEqual(Model.savedFor(junk, "v"), null)
assert.strictEqual(Model.savedFor(junk, "w"), null)

// Older state files with no saved index still parse.
assert.strictEqual(
  Model.savedFor(Model.parse(JSON.stringify({ version: 1, downloads: [] })), "x"),
  null,
)

// A link caught before the index existed: the finished row carries it, so
// it is still asked about rather than silently downloaded again.
const legacy = Model.parse(JSON.stringify({
  version: 1,
  downloads: [
    { id: "old", url: "https://x.test/legacy", filename: "app.apk", title: "App", kind: "file", status: "done", bytes: 5, total: 5, error: "" },
    { id: "busy", url: "https://x.test/busy", filename: "b.zip", title: "", kind: "file", status: "downloading", bytes: 1, total: 9, error: "" },
  ],
}))
const legacyRec = Model.savedFor(legacy, "https://x.test/legacy")
assert.ok(legacyRec, "a pre-index finished row is still recognised")
assert.strictEqual(legacyRec.id, "old")
assert.strictEqual(legacyRec.filename, "app.apk")
assert.strictEqual(legacyRec.path, "", "no recorded path on the fallback")
assert.strictEqual(Model.savedFor(legacy, "https://x.test/busy"), null, "a live row is not saved")
assert.strictEqual(Model.savedFor(legacy, "https://x.test/nothing"), null)

// Card copy: badge words, the "APK · 6 MB" meta line, and the header count.
assert.strictEqual(Model.statusLabel("done"), "Completed")
assert.strictEqual(Model.statusLabel("downloading"), "Downloading")
assert.strictEqual(Model.statusLabel(""), "Queued")
assert.strictEqual(Model.statusLabel("nonsense"), "Queued", "unknown status still reads as queued")

assert.strictEqual(Model.kindLabel({ filename: "kleeamp-0.4.1 (9).apk", kind: "file" }), "APK")
assert.strictEqual(Model.kindLabel({ filename: "clip (1).webm", kind: "video" }), "WEBM")
assert.strictEqual(Model.kindLabel({ filename: "archive.tar.gz", kind: "file" }), "GZ")
assert.strictEqual(Model.kindLabel({ filename: "noext", kind: "video" }), "Video")
assert.strictEqual(Model.kindLabel({ filename: "noext", kind: "file" }), "File")
assert.strictEqual(Model.kindLabel({ filename: ".hidden", kind: "file" }), "File", "a dotfile has no extension")
assert.strictEqual(Model.kindLabel({ filename: "trailing.", kind: "file" }), "File")
assert.strictEqual(Model.kindLabel({ filename: "dir.d/inner", kind: "file" }), "File", "the folder's dot is not an extension")

assert.strictEqual(Model.rowMeta({ filename: "app.apk", kind: "file", status: "done", bytes: 6291456, total: 6291456 }), "APK · 6 MB")
assert.strictEqual(Model.rowMeta({ filename: "clip.mp4", kind: "video", status: "downloading", bytes: 3145728, total: 9437184 }), "MP4 · 3 MB / 9 MB")
assert.strictEqual(Model.rowMeta({ filename: "clip.webm", kind: "video", status: "queued", bytes: 0, total: 0 }), "WEBM · Queued")
assert.strictEqual(Model.rowMeta(null), "")

assert.strictEqual(Model.itemWord(1), "1 item")
assert.strictEqual(Model.itemWord(4), "4 items")
assert.strictEqual(Model.itemWord(0), "0 items")
assert.strictEqual(Model.itemWord(1234), "1,234 items")
assert.strictEqual(Model.itemWord(-3), "0 items")

assert.strictEqual(Model.shortPath("/home/you/Downloads/catch", "/home/you"), "Downloads/catch")
assert.strictEqual(Model.shortPath("/other/place", "/home/you"), "/other/place")
assert.strictEqual(Model.shortPath("/home/youish/x", "/home/you"), "/home/youish/x", "a prefix that is not a path segment")
assert.strictEqual(Model.shortPath("/home/you", "/home/you"), "/home/you")
assert.strictEqual(Model.shortPath("", "/home/you"), "")

// The drainer is woken by scheduling changes only, never by progress writes.
const two = (status, bytes) => Model.parse(JSON.stringify({
  version: 1,
  downloads: [
    { id: "a", url: "u", filename: "a", status: "downloading", bytes: bytes, total: 100, error: "" },
    { id: "b", url: "v", filename: "b", status: status, bytes: 0, total: 0, error: "" },
  ],
}))
const base = Model.scheduleSignature(two("queued", 1))
assert.strictEqual(base, Model.scheduleSignature(two("queued", 99)), "progress is not a scheduling event")
assert.strictEqual(Model.scheduleSignature(Model.parse(JSON.stringify({
  version: 1,
  downloads: [{ id: "a", url: "u", filename: "a", status: "downloading", bytes: 3, total: 100, error: "" }],
}))), "a:downloading", "a finished row frees its slot")
assert.notStrictEqual(base, Model.scheduleSignature(two("done", 1)), "a completion is a scheduling event")
assert.strictEqual(Model.scheduleSignature(Model.empty()), "")
assert.strictEqual(Model.scheduleSignature(null), "")

console.log("model.test.js: ok")
