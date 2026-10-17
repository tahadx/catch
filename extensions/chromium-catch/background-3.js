// Catch menu: one parent with everything downloadable.
// Single items act on what was right-clicked; the "all …" items scrape the
// page (links, images incl. srcset, videos/audios incl. <source> children).
// The native host owns queueing, downloading, clipboard, and toasts.
const HOST = "io.github.tahadx.catch";

// One long-lived native process, not one per message.
//
// sendNativeMessage execs catch-host for every message, so each click paid a
// process spawn before Catch could answer — and the answer is what releases
// the browser's download. connectNative keeps one host alive and correlates
// replies with a `req` id, so the handoff is a postMessage away.
let nativePort = null;
let nextReq = 1;
const waiting = new Map();

function port() {
  if (nativePort) return nativePort;
  try {
    nativePort = chrome.runtime.connectNative(HOST);
  } catch (e) {
    nativePort = null;
    return null;
  }
  nativePort.onMessage.addListener((reply) => {
    const cb = waiting.get(reply && reply.req);
    if (cb) {
      waiting.delete(reply.req);
      cb(reply);
    }
  });
  // The browser drops the host when the extension idles or on browser exit.
  // Forget it so the next message reconnects instead of writing into a dead
  // port and failing silently forever.
  nativePort.onDisconnect.addListener(() => {
    // Chrome flags any unread lastError as "Unchecked runtime.lastError:
    // Native host has exited", so it must be read even though a disconnect is
    // expected here.
    void chrome.runtime.lastError;
    nativePort = null;
    for (const cb of waiting.values()) cb(null);
    waiting.clear();
  });
  return nativePort;
}

// Chrome suspends an idle MV3 service worker after roughly 30 seconds, which
// closes the native port and kills catch-host with it. The user then clicks
// and gets "Native host has exited" on a port that no longer exists. Pinging
// on an alarm keeps the worker and the port alive, so a click costs the same
// whether it comes a second or an hour after the last one.
const KEEPALIVE = "catch-keepalive";
let lastPing = 0;
function startKeepalive() {
  chrome.alarms.create(KEEPALIVE, { periodInMinutes: 1 / 3 });
}
chrome.alarms.onAlarm.addListener((a) => {
  if (a.name !== KEEPALIVE) return;
  const now = Date.now();
  if (now - lastPing < 20000) return; // Chrome's floor is one alarm per minute
  lastPing = now;
  ask({ action: "ping" }, null);
});
chrome.runtime.onStartup.addListener(startKeepalive);
chrome.runtime.onInstalled.addListener(startKeepalive);
startKeepalive();

// Send and (optionally) wait for the reply. cb(null) means Catch is not
// reachable, which every caller treats as "let the browser have it".
function ask(msg, cb) {
  const p = port();
  if (!p) {
    if (cb) cb(null);
    return;
  }
  const req = nextReq++;
  if (cb) waiting.set(req, cb);
  try {
    p.postMessage(Object.assign({ req }, msg));
  } catch (e) {
    waiting.delete(req);
    if (cb) cb(null);
  }
}

function send(msg) {
  ask(msg, null);
}

function httpUrl(u) {
  return typeof u === "string" && /^https?:/i.test(u) ? u : "";
}

function sendUrl(url, title) {
  url = httpUrl(url);
  if (!url) return;
  send(title ? { url, title } : { url });
}

function sendVideo(url, title, audioOnly) {
  url = httpUrl(url);
  if (!url) return;
  const msg = { video: url, title: title || "" };
  if (audioOnly) msg.audioOnly = true;
  send(msg);
}

function copyUrl(url) {
  url = httpUrl(url);
  if (!url) return;
  send({ copy: url });
}

function sendAll(urls) {
  const seen = new Set();
  for (const u of urls || []) {
    const clean = httpUrl(u);
    if (!clean || seen.has(clean)) continue;
    seen.add(clean);
    sendUrl(clean);
  }
}

// Page scrapers run in the page, so srcset/currentSrc resolve to real URLs.
const SCRAPERS = {
  links: () => Array.from(document.links || []).map((a) => a.href),
  images: () => {
    const out = [];
    for (const img of document.images || []) {
      // Prefer the largest srcset candidate (originals over thumbnails);
      // fall back to what the browser actually rendered.
      let best = "", bestW = -1;
      const srcset = img.srcset || "";
      for (const part of srcset.split(",")) {
        const bits = part.trim().split(/\s+/);
        const u = bits[0] || "", d = bits[1] || "";
        if (!u) continue;
        let w = -1;
        if (d.endsWith("w")) w = parseInt(d, 10) || -1;
        else if (d.endsWith("x")) w = (parseFloat(d) || 0) * 1000;
        if (w >= bestW) { bestW = w; best = u; }
      }
      const picks = best ? [best] : [img.currentSrc, img.src].filter(Boolean);
      for (const u of picks) {
        try { out.push(new URL(u, document.baseURI).href); } catch (e) {}
      }
    }
    return out;
  },
  videos: () => {
    const out = [];
    for (const m of document.querySelectorAll("video, audio")) {
      if (m.currentSrc) out.push(m.currentSrc);
      if (m.src) out.push(m.src);
      for (const s of m.querySelectorAll("source")) {
        if (s.src) out.push(s.src);
      }
      const set = m.poster || "";
      if (set) out.push(new URL(set, document.baseURI).href);
    }
    return out;
  },
  selected: () => {
    const sel = window.getSelection();
    if (!sel || sel.isCollapsed || sel.rangeCount === 0) return [];
    const range = sel.getRangeAt(0);
    const out = [];
    const take = (el, url) => {
      try {
        if (url && range.intersectsNode(el)) out.push(url);
      } catch (e) {}
    };
    for (const a of document.links || []) take(a, a.href);
    for (const img of document.images || []) {
      take(img, img.currentSrc);
      take(img, img.src);
    }
    for (const m of document.querySelectorAll("video, audio")) {
      take(m, m.currentSrc || m.src);
      for (const s of m.querySelectorAll("source")) take(s, s.src);
    }
    return out;
  }
};

function scrape(tab, kind) {
  if (!tab || tab.id === undefined) return;
  chrome.scripting
    .executeScript({ target: { tabId: tab.id }, func: SCRAPERS[kind] })
    .then((results) => sendAll(results && results[0] && results[0].result))
    .catch(() => {});
}

function scrapeFiles(tab) {
  if (!tab || tab.id === undefined) return;
  chrome.scripting
    .executeScript({
      target: { tabId: tab.id },
      func: (re) => {
        const out = [];
        const rx = new RegExp(re);
        for (const a of document.links || []) {
          if (a.href && rx.test(a.href.split("?")[0].split("#")[0])) out.push(a.href);
        }
        return out;
      },
      args: ["\\.(pdf|docx?|xlsx?|pptx?|txt|odt|zip|rar|7z|tar|gz|bz2|xz|mp4|mkv|avi|webm|mov|flv|m4v|mp3|flac|wav|m4a|ogg|aac|appimage|deb|rpm|exe|bin)(\\?|#|$)"]
    })
    .then((results) => sendAll(results && results[0] && results[0].result))
    .catch(() => {});
}

function pageUrl(tab) {
  return tab && typeof tab.url === "string" ? tab.url : "";
}

function pageTitle(tab) {
  return tab && typeof tab.title === "string" ? tab.title : "";
}

// IDM-style takeover: a plain left-click lands here first. Hand it to
// Catch; if the native host is missing or refuses, leave the browser
// download running so nothing is ever swallowed.
//
// Canceling a download a click started makes Chromium re-navigate to the
// URL and fetch the file again, which is why a click can look like it is
// "not working" and then take a while. Catch still has the row and the file
// in about 35 ms; the second fetch is Chromium's, and it only costs what the
// file costs. Dropping this makes clicks instant but sends them to the
// browser instead of Catch, which is not what Catch is for.
function intercept(item) {
  const url = httpUrl(item && item.url);
  if (!url) return; // blob:, data:, chrome: stay in the browser
  if (item.state && item.state !== "in_progress") return; // too late, browser won
  if (bypassed(url)) return;
  const msg = { url, title: "", filenameHint: item.filename || "" };
  ask(msg, (reply) => {
    if (!reply || !reply.ok) return;
    chrome.downloads.cancel(item.id, () => {
      chrome.downloads.erase({ id: item.id }, () => {
        void chrome.runtime.lastError;
      });
    });
  });
}

chrome.downloads.onCreated.addListener(intercept);

// URLs the user explicitly asked the browser to take (bypass interception).
const browserBypass = new Map();

function bypassed(url) {
  const t = browserBypass.get(url);
  if (t === undefined) return false;
  browserBypass.delete(url);
  return Date.now() - t < 30000;
}

// Worker restarts lose menus; onInstalled alone misses reloads.
function buildMenus() {
  chrome.contextMenus.removeAll(() => {
    try {
      chrome.contextMenus.create({ id: "catch", title: "Catch",
        contexts: ["page", "link", "image", "video", "audio", "selection"] });
      const kids = [
        // Video and audio only. On every other link these two used to show
        // up, which was noise: a .zip or .apk has no "catch as video" and the
        // backend routes a watch link to yt-dlp on its own.
        ["catch-video", "Catch as video", ["video", "audio"]],
        ["catch-audio", "Catch audio only", ["video", "audio"]],
        ["sep-top", "", []],
        ["catch-all-links", "Catch links", ["page"]],
        ["catch-all-images", "Catch images", ["page"]],
        ["catch-all-videos", "Catch videos", ["page"]],
        ["catch-all-files", "Catch documents & archives", ["page"]],
        ["catch-selected", "Catch selected links", ["selection"]],
        ["sep-bottom", "", []],
        ["catch-this", "Catch this", ["link", "image", "video", "audio"]],
        ["browser-download", "Download with browser", ["link"]],
        ["open-folder", "Open Catch folder", ["page", "link"]],
        ["copy-page", "Copy page URL", ["page"]]
      ];
      for (const [id, title, contexts] of kids) {
        if (title === "") chrome.contextMenus.create({ id, parentId: "catch", type: "separator", contexts: ["page", "link", "image", "video", "audio", "selection"] });
        else chrome.contextMenus.create({ id, parentId: "catch", title, contexts });
      }
    } catch (e) {}
  });
}

// Worker restarts lose menus; onInstalled alone misses reloads.
buildMenus();
chrome.runtime.onInstalled.addListener(buildMenus);
chrome.runtime.onStartup.addListener(buildMenus);

function targetUrl(info) {
  // Strict: item actions only ever use the item's own URL. Falling back to
  // the page here once saved release pages as files.
  return httpUrl(info.linkUrl) || httpUrl(info.srcUrl);
}

chrome.contextMenus.onClicked.addListener((info, tab) => {
  const url = targetUrl(info);
  const page = httpUrl(pageUrl(tab));
  switch (info.menuItemId) {
    case "catch-video": sendVideo(url || page, pageTitle(tab), false); break;
    case "catch-audio": sendVideo(url || page, pageTitle(tab), true); break;
    case "open-folder": send({ action: "open-folder" }); break;
    case "catch-this": sendUrl(url || page, pageTitle(tab)); break;
    case "catch-all-links": scrape(tab, "links"); break;
    case "catch-all-images": scrape(tab, "images"); break;
    case "catch-all-videos": scrape(tab, "videos"); break;
    case "catch-all-files": scrapeFiles(tab); break;
    case "catch-selected": scrape(tab, "selected"); break;
    case "browser-download": {
      const burl = httpUrl(info.linkUrl);
      if (burl) {
        browserBypass.set(burl, Date.now());
        chrome.downloads.download({ url: burl }, () => {
          void chrome.runtime.lastError;
        });
      }
      break;
    }
    case "copy-page": copyUrl(pageUrl(tab)); sendUrl(pageUrl(tab), pageTitle(tab)); break;
  }
});

chrome.commands.onCommand.addListener((command) => {
  if (command !== "catch-download") return;
  chrome.tabs.query({ active: true, currentWindow: true }, (tabs) => {
    const tab = tabs[0];
    sendUrl(pageUrl(tab), pageTitle(tab));
  });
});

chrome.action.onClicked.addListener((tab) => {
  sendUrl(pageUrl(tab), pageTitle(tab));
});
