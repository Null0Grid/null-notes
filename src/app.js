// Null Notes desktop UI.
import { api, native, onFileDrop, onEvent } from "./backend.js";
import { render, linkTargets, tagsIn, esc } from "./markdown.js";
import { createGraph } from "./graph.js";
import { createCanvas } from "./canvas.js";

const $ = (id) => document.getElementById(id);

/* ---------------- state ---------------- */
const S = {
  base: {},
  vault: null,
  entries: [],
  byPath: new Map(),
  texts: {},            // path -> content for text files (and unlocked locked notes, in memory only)
  images: new Map(),    // path -> Promise<dataURL>
  current: null,
  selDir: "",           // folder that new items and imports go into
  expanded: new Set(),
  mode: "split",
  tagFilter: null,
  dirty: false,
  saveTimer: null,
  saving: null,
  lock: { configured: false, unlocked: false },
  settings: null,
};

const DEFAULT_SETTINGS = {
  links: { autoUpdate: true },
  daily: { enabled: false, folder: "Daily", format: "YYYY-MM-DD", template: "", titleDate: true, timeStamp: false, clock: "24" },
  lock: { enabled: false, autoLockMin: 10 },
};

/* ---------------- path helpers ---------------- */
const fileName = (p) => p.split("/").pop();
const folderOf = (p) => (p.includes("/") ? p.slice(0, p.lastIndexOf("/")) : "");
const join = (dir, name) => (dir ? dir + "/" + name : name);
const ext = (p) => ((fileName(p).match(/\.([^.]+)$/) || [, ""])[1] || "").toLowerCase();
const kindFromExt = (p) => {
  const x = ext(p);
  if (x === "lock") return kindFromExt(p.slice(0, -5)) && kindFromExt(p.slice(0, -5)) !== "img" ? "locked" : null;
  return x === "md" || x === "markdown" ? "md" : x === "txt" ? "txt" : x === "html" || x === "htm" ? "html" : x === "canvas" ? "canvas" : ["png", "jpg", "jpeg"].includes(x) ? "img" : null;
};
const kindOf = (p) => { const e = S.byPath.get(p); return e ? e.kind : kindFromExt(p); };
const isLocked = (p) => kindOf(p) === "locked";
/** What a file is underneath: "Plan.md.lock" -> "md". */
const baseKind = (p) => (isLocked(p) ? kindFromExt(p.slice(0, -5)) : kindOf(p));
const isImg = (p) => kindOf(p) === "img";
const isDir = (p) => !!(S.byPath.get(p) || {}).dir;
const display = (p) => {
  const n = fileName(p).replace(/\.lock$/i, "");
  return baseKind(p) === "md" ? n.replace(/\.(md|markdown)$/i, "") : n;
};
const files = () => S.entries.filter((e) => !e.dir).map((e) => e.path);
const folders = () => S.entries.filter((e) => e.dir).map((e) => e.path);
const within = (p, dir) => p === dir || p.startsWith(dir + "/");
const hasText = (p) => p in S.texts;

function resolveIn(list, name) {
  const n = String(name).trim().toLowerCase().replace(/^\.?\//, "");
  if (!n) return null;
  const noLock = (p) => p.replace(/\.lock$/i, "").toLowerCase();
  return (
    list.find((p) => p.toLowerCase() === n) ||
    list.find((p) => fileName(p).toLowerCase() === n) ||
    list.find((p) => noLock(p) === n || fileName(noLock(p)) === n) ||
    list.find((p) => baseKind(p) === "md" && (display(p).toLowerCase() === n || noLock(p).replace(/\.(md|markdown)$/i, "") === n)) ||
    null
  );
}
const resolve = (name) => resolveIn(files(), name);
const linksOf = (p) => (baseKind(p) === "md" && hasText(p) ? linkTargets(S.texts[p]) : []);
const tagsOf = (p) => (baseKind(p) === "md" && hasText(p) ? tagsIn(S.texts[p]) : []);
const errText = (e) => (typeof e === "string" ? e : (e && e.message) || "Something went wrong.");
const renderCtx = { resolve, display, isImg };

/* ---------------- prefs (per vault, local to this machine) ---------------- */
const prefKey = () => "nullnotes.prefs." + (S.vault ? S.vault.path : "none");
function loadPrefs() {
  try {
    const p = JSON.parse(localStorage.getItem(prefKey()) || "{}");
    S.expanded = new Set(p.expanded || []);
    S.mode = ["edit", "split", "read"].includes(p.mode) ? p.mode : "split";
    return p;
  } catch { return {}; }
}
function savePrefs() {
  try { localStorage.setItem(prefKey(), JSON.stringify({ expanded: [...S.expanded], mode: S.mode, last: S.current })); } catch {}
}

/* ---------------- vault settings (saved in the vault, .nullnotes/settings.json) ---------------- */
async function loadSettings() {
  const s = JSON.parse(JSON.stringify(DEFAULT_SETTINGS));
  try {
    const raw = await api("read_meta", { name: "settings" });
    const m = raw ? JSON.parse(raw) : {};
    for (const k of Object.keys(s)) Object.assign(s[k], m[k] || {});
  } catch {}
  S.settings = s;
}
async function saveSettings() {
  try { await api("write_meta", { name: "settings", content: JSON.stringify(S.settings, null, 1) }); }
  catch (e) { toast("Couldn't save settings. " + errText(e), true); }
}
function applyAddons() {
  $("todayBtn").hidden = !S.settings.daily.enabled;
  $("lockBtn").hidden = !S.settings.lock.enabled;
  drawLockBtn();
}

/* ---------------- toast ---------------- */
let toastTimer;
function toast(msg, warn) {
  const t = $("toast");
  t.textContent = msg;
  t.className = "toast" + (warn ? " warn" : "");
  t.hidden = false;
  clearTimeout(toastTimer);
  toastTimer = setTimeout(() => (t.hidden = true), warn ? 6500 : 2800);
}

/* ---------------- welcome ---------------- */
async function showWelcome() {
  $("app").hidden = true;
  $("welcome").hidden = false;
  const recent = await api("recent_vaults").catch(() => []);
  $("wRecentBox").hidden = !recent.length;
  $("wRecent").innerHTML = recent
    .map((v) => `<li><button data-vault="${esc(v.path)}"><div class="n">${esc(v.name)}</div><div class="p">${esc(v.path)}</div></button></li>`)
    .join("");
}
async function pickVault() {
  try {
    const v = await api("pick_vault");
    if (v) await enterVault(v);
  } catch (e) { toast(errText(e), true); }
}
async function enterVault(v) {
  S.vault = v;
  S.current = null;
  S.images.clear();
  S.texts = {};
  S.base = {};
  if (graph) graph.reset();
  closeCanvas();
  S.tagFilter = null;
  const prefs = loadPrefs();
  $("welcome").hidden = true;
  $("app").hidden = false;
  $("vaultName").textContent = v.name;
  $("vaultPath").textContent = v.path;
  document.title = v.name + " · Null Notes";
  await loadSettings();
  applyAddons();
  refreshBackup();
  setMode(S.mode, true);
  await refresh();
  const first =
    (prefs.last && S.byPath.has(prefs.last) && prefs.last) ||
    resolve("Welcome.md") ||
    files().find((p) => kindOf(p) === "md") ||
    files()[0];
  if (first) await open(first);
  else drawAll();
}

/* ---------------- loading ---------------- */
async function refresh() {
  if (!S.vault) return;
  let entries, texts, lock;
  try {
    [entries, texts, lock] = await Promise.all([api("list_entries"), api("read_all_text"), api("lock_status").catch(() => ({ configured: false, unlocked: false }))]);
  } catch (e) {
    toast(errText(e), true);
    if (/No vault is open|isn't a folder/.test(errText(e))) { S.vault = null; showWelcome(); }
    return;
  }
  S.entries = entries;
  S.byPath = new Map(entries.map((e) => [e.path, e]));
  S.lock = lock;
  if (lock.unlocked) {
    try { Object.assign(texts, await api("read_all_locked")); } catch {}
  }
  for (const p in texts) if (!(S.dirty && p === S.current)) S.base[p] = texts[p];
  if (S.current && S.dirty && S.current in S.texts) texts[S.current] = S.texts[S.current];
  for (const p in S.texts) if (!(p in texts) && S.byPath.has(p) && !isImg(p) && (!isLocked(p) || lock.unlocked)) texts[p] = S.texts[p]; // big files read on demand
  S.texts = texts;
  for (const p of [...S.images.keys()]) if (!S.byPath.has(p)) S.images.delete(p);
  for (const d of [...S.expanded]) if (!S.byPath.has(d)) S.expanded.delete(d);
  if (S.current && !S.byPath.has(S.current)) S.current = null;
  if (S.selDir && !isDir(S.selDir)) S.selDir = "";
  if (S.current && !S.dirty && hasText(S.current) && baseKind(S.current) !== "canvas" && $("ed").value !== S.texts[S.current]) {
    $("ed").value = S.texts[S.current];
  }
  drawTree();
  drawAll();
  drawLockBtn();
  if (!$("searchPane").hidden) runFullSearch();
  if (canvas && S.current && baseKind(S.current) === "canvas") canvas.refresh();
}

function loadImage(p) {
  if (!S.images.has(p)) {
    S.images.set(p, api("read_image", { path: p }).catch((e) => { S.images.delete(p); throw e; }));
  }
  return S.images.get(p);
}
function hydrateImages(root) {
  root.querySelectorAll("img[data-img]").forEach((img) => {
    loadImage(img.dataset.img).then((src) => (img.src = src)).catch(() => {
      img.replaceWith(Object.assign(document.createElement("span"), { className: "missingimg", textContent: "couldn't load " + img.dataset.img }));
    });
  });
}

/* ---------------- diagrams (mermaid, loaded on first use, runs offline) ---------------- */
let mermaidReady = null, mermaidSeq = 0;
const mermaidCache = new Map();
function loadMermaid() {
  if (!mermaidReady) {
    mermaidReady = new Promise((res, rej) => {
      const s = document.createElement("script");
      s.src = "vendor/mermaid.min.js";
      s.onload = () => {
        window.mermaid.initialize({
          startOnLoad: false,
          securityLevel: "strict",
          theme: "base",
          fontFamily: '"IBM Plex Mono", monospace',
          themeVariables: {
            darkMode: true, background: "#10121a", fontSize: "14px",
            primaryColor: "#151826", primaryTextColor: "#d9def0", primaryBorderColor: "#00e5ff",
            secondaryColor: "#1b1f33", secondaryBorderColor: "#ff2e88", secondaryTextColor: "#d9def0",
            tertiaryColor: "#10121a", tertiaryBorderColor: "#39ff88", tertiaryTextColor: "#d9def0",
            lineColor: "#5a6a9a", textColor: "#d9def0", mainBkg: "#151826", nodeBorder: "#00e5ff",
            clusterBkg: "#10121a", clusterBorder: "#2f3658", edgeLabelBackground: "#0a0a0f",
            noteBkgColor: "#1b1f33", noteTextColor: "#d9def0", noteBorderColor: "#ffb000",
            actorBkg: "#151826", actorBorder: "#00e5ff", actorTextColor: "#d9def0", signalColor: "#8a92b4", signalTextColor: "#d9def0",
          },
        });
        res(window.mermaid);
      };
      s.onerror = () => rej(new Error("The diagram engine couldn't load."));
      document.head.appendChild(s);
    });
  }
  return mermaidReady;
}
function hydrateDiagrams(root) {
  const blocks = root.querySelectorAll(".mermaid-block");
  if (!blocks.length) return;
  blocks.forEach((b) => {
    const code = b.querySelector(".mermaid-src").textContent;
    const out = b.querySelector(".mermaid-out");
    if (mermaidCache.has(code)) { out.innerHTML = mermaidCache.get(code); return; }
    loadMermaid().then(async (mm) => {
      try {
        const { svg } = await mm.render("nnm" + ++mermaidSeq, code);
        if (mermaidCache.size > 60) mermaidCache.delete(mermaidCache.keys().next().value);
        mermaidCache.set(code, svg);
        if (out.isConnected) out.innerHTML = svg;
      } catch (e) {
        const msg = String((e && e.message) || e).split("\n").slice(0, 4).join("\n");
        if (out.isConnected) out.innerHTML = `<div class="mermaid-err">Diagram error: ${esc(msg)}</div>`;
        document.querySelectorAll("body > [id^='dnnm']").forEach((n) => n.remove()); // mermaid leaves error svgs behind
      }
    }).catch((e) => { out.innerHTML = `<div class="mermaid-err">${esc(errText(e))}</div>`; });
  });
}

/* ---------------- saving ---------------- */
function setSaved(state, text) {
  const s = $("saved");
  s.textContent = text;
  s.className = "saved" + (state === "busy" ? " busy" : state === "err" ? " err" : "");
}
// S.base[p]: the text on disk that the current edit of p started from. The
// backend refuses to overwrite a file that changed since (sync, git, another
// editor) and saves the edit as a "(conflict YYYY-MM-DD)" copy instead.
async function writeFile(p, content) {
  if (isLocked(p)) return api("write_locked", { path: p, content });
  const conflict = await api("write_text", { path: p, content, base: p in S.base ? S.base[p] : null });
  if (conflict) {
    toast(`${fileName(p)} was changed somewhere else while you edited it. Both are kept: your edit is in "${fileName(conflict)}".`, true);
    S.dirty = false;
    await refresh();
    return conflict;
  }
  S.base[p] = content;
  return null;
}
async function save() {
  clearTimeout(S.saveTimer);
  S.saveTimer = null;
  const p = S.current;
  if (!p || !S.dirty) return;
  const content = S.texts[p];
  setSaved("busy", "SAVING");
  S.saving = writeFile(p, content);
  try {
    await S.saving;
    if (S.texts[p] === content) S.dirty = false;
    setSaved("ok", S.dirty ? "EDITING" : isLocked(p) ? "SAVED · ENCRYPTED" : "SAVED");
    const e = S.byPath.get(p);
    if (e) e.size = new Blob([content]).size;
  } catch (e) {
    setSaved("err", "NOT SAVED");
    toast("Couldn't save " + fileName(p) + ". " + errText(e), true);
  } finally {
    S.saving = null;
  }
}
async function flush() {
  if (S.saving) await S.saving.catch(() => {});
  if (S.dirty) await save();
}
function markDirty() {
  S.dirty = true;
  setSaved("busy", "EDITING");
  clearTimeout(S.saveTimer);
  S.saveTimer = setTimeout(save, 450);
}

/* ---------------- tree ---------------- */
function childrenOf(dir) {
  const kids = S.entries.filter((e) => folderOf(e.path) === dir);
  return kids.sort((a, b) => (a.dir !== b.dir ? (a.dir ? -1 : 1) : a.name.toLowerCase().localeCompare(b.name.toLowerCase(), undefined, { numeric: true })));
}
function visibleUnderTag(e) {
  if (!S.tagFilter) return true;
  if (e.dir) return S.entries.some((x) => !x.dir && within(x.path, e.path) && tagsOf(x.path).includes(S.tagFilter));
  return tagsOf(e.path).includes(S.tagFilter);
}
function badgeFor(p) {
  const k = kindOf(p);
  if (k === "img") return ["img", ext(p).toUpperCase()];
  if (k === "locked") return ["locked", "LOCK"];
  return [k, k.toUpperCase()];
}
function drawTree() {
  const html = (dir, depth) =>
    childrenOf(dir)
      .filter(visibleUnderTag)
      .map((e) => {
        const pad = `padding-left:${10 + depth * 14}px`;
        if (e.dir) {
          const open = S.expanded.has(e.path) || !!S.tagFilter;
          const n = S.entries.filter((x) => !x.dir && folderOf(x.path) === e.path).length;
          return `<li role="treeitem" aria-expanded="${open}"><button class="row dir${open ? " open" : ""}${S.selDir === e.path && !S.current ? " sel" : ""}" data-path="${esc(e.path)}" data-dir="1" style="${pad}"><span class="car">▶</span><span class="nm">${esc(e.name)}</span>${n ? `<span class="cnt">${n}</span>` : ""}</button>${open ? `<ul role="group">${html(e.path, depth + 1)}</ul>` : ""}</li>`;
        }
        const [k, badge] = badgeFor(e.path);
        return `<li role="treeitem"><button class="row${e.path === S.current ? " on" : ""}" data-path="${esc(e.path)}" style="${pad}"><span class="car"></span><span class="nm">${esc(display(e.path))}</span><span class="badge k-${k}">${badge}</span></button></li>`;
      })
      .join("");
  const out = html("", 0);
  $("tree").innerHTML = out || `<li class="treeempty">${S.tagFilter ? "No files with this tag." : "This vault is empty. Make a note or import some files."}</li>`;

  const counts = {};
  files().forEach((p) => tagsOf(p).forEach((g) => (counts[g] = (counts[g] || 0) + 1)));
  const tags = Object.keys(counts).sort();
  $("tags").innerHTML = tags.length
    ? tags.map((g) => `<button class="tag${g === S.tagFilter ? " on" : ""}" data-tagf="${esc(g)}">#${esc(g)} <span style="opacity:.6">${counts[g]}</span></button>`).join("")
    : `<span class="empty" style="padding:0">Add #tags in notes to see them here.</span>`;
  const nf = files().length, nd = folders().length;
  const nl = files().reduce((a, p) => a + linksOf(p).length, 0);
  $("count").textContent = `${nf} FILES · ${nd} FOLDERS · ${nl} LINKS`;
}
function expandTo(p) {
  let d = folderOf(p);
  while (d) { S.expanded.add(d); d = folderOf(d); }
}

/* ---------------- center + side panels ---------------- */
function fmtBytes(n) {
  return n > 1048576 ? (n / 1048576).toFixed(2) + " MB" : n > 1024 ? (n / 1024).toFixed(1) + " KB" : n + " B";
}
let frameTimer;
function drawPreview() {
  const p = S.current, k = baseKind(p), text = S.texts[p] ?? "";
  const pv = $("preview"), doc = $("doc"), alt = $("alt");
  pv.classList.toggle("frame", k === "html");
  doc.hidden = k !== "md";
  alt.hidden = k === "md";
  if (k === "md") {
    const r = render(text, renderCtx);
    doc.innerHTML = r.html || '<p style="color:var(--faint)">Empty note. Start typing on the left.</p>';
    hydrateImages(doc);
    hydrateDiagrams(doc);
    return r.heads;
  }
  if (k === "txt") alt.innerHTML = `<pre class="plaintext">${text ? esc(text) : '<span style="color:var(--faint)">Empty file.</span>'}</pre>`;
  if (k === "html") {
    let fr = alt.querySelector("iframe");
    if (!fr) {
      alt.innerHTML = `<div class="framebar"><span>RENDERED</span><span>scripts and network blocked</span></div><iframe sandbox="" title="Rendered page"></iframe>`;
      fr = alt.querySelector("iframe");
    }
    clearTimeout(frameTimer);
    frameTimer = setTimeout(() => (fr.srcdoc = text), fr.srcdoc ? 250 : 0);
  }
  if (k === "img") {
    const name = fileName(p), e = S.byPath.get(p);
    alt.innerHTML = `<div class="imgview"><div class="stage"><img id="imgEl" data-img="${esc(p)}" alt="${esc(name)}"></div>
      <div class="meta"><span id="imgDims">…</span><span>${fmtBytes(e ? e.size : 0)}</span><span>${ext(p).toUpperCase()}</span></div>
      <div class="meta">Embed with <code>![[${esc(name)}]]</code><button class="btn" id="copyEmbed">Copy</button></div></div>`;
    const im = $("imgEl");
    im.addEventListener("load", () => ($("imgDims").textContent = `${im.naturalWidth} × ${im.naturalHeight} px`));
    hydrateImages(alt);
    $("copyEmbed").onclick = () => copyText(`![[${name}]]`);
  }
  return [];
}
function drawSide(heads) {
  const p = S.current, k = baseKind(p), text = S.texts[p] ?? "";
  const f = folderOf(p);
  $("path").innerHTML = (f ? esc(f.split("/").join(" / ")) + " / " : "") + "<b>" + esc(fileName(p)) + "</b>" + (isLocked(p) ? ' <span class="badge k-locked">ENCRYPTED</span>' : "");
  $("outBox").hidden = k !== "md";
  $("outline").innerHTML = heads.length ? heads.map((h) => `<li class="h${h.lv}" data-jump="${h.id}">${esc(h.text)}</li>`).join("") : '<li style="cursor:default">No headings</li>';

  const back = files().filter((q) => q !== p && linksOf(q).some((l) => resolve(l) === p));
  const needles = [fileName(p).toLowerCase(), display(p).toLowerCase()];
  $("back").innerHTML = back.length
    ? back.map((q) => {
        const line = (S.texts[q] || "").split("\n").find((l) => needles.some((n) => l.toLowerCase().includes(n))) || "";
        const snip = esc(line.replace(/^[#>\-*\s\d.]+/, "").slice(0, 120)).replace(/\[\[([^\]]+)\]\]/g, "<mark>$1</mark>");
        return `<li><button data-open="${esc(q)}"><div class="t">${esc(display(q))}</div><div class="s">${snip}</div></button></li>`;
      }).join("")
    : `<li class="empty">Nothing links here yet.${k === "img" ? " Embed it in a note to connect it." : ""}</li>`;

  if (k === "md") {
    const outs = [...new Set(linksOf(p))];
    $("out").innerHTML = outs.length
      ? outs.map((l) => {
          const hit = resolve(l);
          return `<li><button data-wl="${esc(l)}"><div class="t" style="${hit ? "" : "color:var(--amber)"}">${esc(hit ? display(hit) : l)}</div><div class="s">${hit ? esc((folderOf(hit) || "vault root") + " · " + (kindOf(hit) === "img" ? "image" : kindOf(hit))) : "Not created yet · click to create"}</div></button></li>`;
        }).join("")
      : '<li class="empty">No links in this note.</li>';
  }
  const e = S.byPath.get(p);
  let st = `TYPE ${k === "img" ? ext(p).toUpperCase() + " IMAGE" : (k || "?").toUpperCase()}${isLocked(p) ? " · ENCRYPTED" : ""}<br>SIZE ${fmtBytes(e ? e.size : 0)}`;
  if (e && e.modified) st += `<br>MODIFIED ${new Date(e.modified).toLocaleString()}`;
  if (k === "canvas") {
    try { const d = JSON.parse(text || "{}"); st += `<br>CARDS ${(d.nodes || []).length}<br>CONNECTIONS ${(d.edges || []).length}`; } catch {}
  } else if (k !== "img" && hasText(p)) st += `<br>WORDS ${(text.match(/\S+/g) || []).length}<br>LINES ${text.split("\n").length}`;
  if (k === "md") st += `<br>TAGS ${tagsOf(p).map((t) => "#" + esc(t)).join(" ") || "none"}`;
  $("stats").innerHTML = st;
}
function drawAll() {
  const has = !!S.current && S.byPath.has(S.current);
  const p = S.current, k = has ? baseKind(p) : null;
  const lockedAway = has && isLocked(p) && !S.lock.unlocked;
  $("blank").hidden = has;
  $("panes").hidden = !has || lockedAway || k === "canvas";
  $("canvasHost").hidden = !has || lockedAway || k !== "canvas";
  $("lockedPane").hidden = !lockedAway;
  [$("renameBtn"), $("delBtn"), $("saved")].forEach((b) => (b.hidden = !has));
  if (!has) {
    $("path").innerHTML = S.selDir ? esc(S.selDir.split("/").join(" / ")) : "";
    $("back").innerHTML = ""; $("out").innerHTML = ""; $("outline").innerHTML = ""; $("stats").innerHTML = "";
    return;
  }
  const modeLess = k === "img" || k === "canvas" || lockedAway;
  ["mEdit", "mSplit", "mRead"].forEach((id) => ($(id).disabled = modeLess));
  $("saved").hidden = k === "img" || lockedAway;
  if (lockedAway) {
    $("lockedTitle").textContent = display(p) + " is locked";
    $("lockedUnlock").textContent = S.lock.configured ? "Unlock vault" : "Set up encryption";
    drawSide([]);
    return;
  }
  if (k === "canvas") { drawSide([]); return; }
  $("panes").className = "panes " + (k === "img" ? "image" : S.mode);
  drawSide(drawPreview());
}

/* ---------------- canvas ---------------- */
let canvas = null, canvasPath = null;
function closeCanvas() { if (canvas) { canvas.destroy(); canvas = null; canvasPath = null; } }
function showCanvas(p) {
  if (!canvas) {
    canvas = createCanvas($("canvasHost"), {
      renderCtx,
      hydrate: (el) => { hydrateImages(el); hydrateDiagrams(el); },
      loadImage,
      readText: async (q) => {
        if (hasText(q)) return S.texts[q];
        if (isLocked(q)) return S.lock.unlocked ? (S.texts[q] = await api("read_locked", { path: q })) : null;
        return (S.texts[q] = await api("read_text", { path: q }));
      },
      open: (q) => open(q),
      kindOf: (q) => (S.byPath.has(q) ? kindOf(q) : null),
      display,
      files,
      menu: (x, y, items, header) => showMenu(x, y, items, header),
      askText,
      askLine,
      pickFile: pickVaultFile,
      onChange: (json) => {
        if (!canvasPath || S.current !== canvasPath) return;
        S.texts[canvasPath] = json;
        markDirty();
        drawSide([]);
      },
      graphSnapshot: () => graph.snapshot(),
      toast,
    });
  }
  canvasPath = p;
  canvas.load(S.texts[p] ?? "");
}

/* ---------------- open ---------------- */
async function open(p, select) {
  if (!S.byPath.has(p) || isDir(p)) return;
  await flush();
  S.current = p;
  S.selDir = folderOf(p);
  S.dirty = false;
  expandTo(p);
  if (!isImg(p) && !hasText(p)) {
    if (isLocked(p)) {
      if (S.lock.unlocked) { try { S.texts[p] = await api("read_locked", { path: p }); } catch (e) { toast(errText(e), true); } }
    } else {
      try { S.texts[p] = await api("read_text", { path: p }); S.base[p] = S.texts[p]; }
      catch (e) { toast(errText(e), true); S.texts[p] = ""; }
    }
  }
  const k = baseKind(p);
  $("ed").value = !isImg(p) && k !== "canvas" ? S.texts[p] ?? "" : "";
  $("ed").scrollTop = 0;
  $("preview").scrollTop = 0;
  $("alt").innerHTML = "";
  setSaved("ok", isLocked(p) ? "ENCRYPTED" : "SAVED");
  drawTree();
  drawAll();
  if (k === "canvas" && hasText(p)) showCanvas(p);
  else canvasPath = null;
  if (select && k !== "canvas" && k !== "img") selectInEditor(select.start, select.end);
  savePrefs();
}
function selectInEditor(start, end) {
  const ed = $("ed");
  if (S.mode === "read") setMode("split");
  ed.focus();
  ed.setSelectionRange(start, end);
  const line = ed.value.slice(0, start).split("\n").length - 1;
  const lh = parseFloat(getComputedStyle(ed).lineHeight) || 24;
  ed.scrollTop = Math.max(0, line * lh - ed.clientHeight / 3);
}

/* ---------------- modals ---------------- */
function modal(html, setup, wide) {
  return new Promise((resolve) => {
    const m = $("modal");
    m.innerHTML = `<form novalidate${wide ? ' class="wide"' : ""}>${html}</form>`;
    m.hidden = false;
    const form = m.querySelector("form");
    const done = (v) => { m.hidden = true; m.innerHTML = ""; document.removeEventListener("keydown", onKey, true); resolve(v); };
    const onKey = (e) => { if (e.key === "Escape") { e.preventDefault(); e.stopPropagation(); done(null); } };
    document.addEventListener("keydown", onKey, true);
    m.onclick = (e) => { if (e.target === m) done(null); };
    form.querySelectorAll("[data-cancel]").forEach((b) => (b.onclick = () => done(null)));
    setup(form, done);
  });
}
function askName({ title, label, value = "", hint = "", ok = "Create", kinds = null, kind = "md", validate }) {
  const kindBtns = kinds ? `<div class="kinds" role="group" aria-label="File type">${kinds.map((k) => `<button type="button" data-kind="${k}" aria-pressed="${k === kind}">.${k}</button>`).join("")}</div>` : "";
  return modal(
    `<h2>${esc(title)}</h2>
     <div><label for="mName">${esc(label)}</label><input type="text" id="mName" value="${esc(value)}" autocomplete="off" spellcheck="false"></div>
     ${kindBtns}${hint ? `<div class="hint">${esc(hint)}</div>` : ""}
     <div class="err" id="mErr"></div>
     <div class="row2"><button type="button" class="btn" data-cancel>Cancel</button><button type="submit" class="btn">${esc(ok)}</button></div>`,
    (form, done) => {
      let k = kind;
      const inp = form.querySelector("#mName");
      form.querySelectorAll("[data-kind]").forEach((b) => (b.onclick = () => {
        k = b.dataset.kind;
        form.querySelectorAll("[data-kind]").forEach((x) => x.setAttribute("aria-pressed", x === b));
        inp.focus();
      }));
      setTimeout(() => {
        inp.focus();
        const dot = inp.value.lastIndexOf(".");
        if (dot > 0 && !kinds) inp.setSelectionRange(0, dot); else inp.select();
      }, 0);
      form.onsubmit = (e) => {
        e.preventDefault();
        const v = inp.value.trim();
        const msg = !v ? "Give it a name." : /[<>:"|?*\\]/.test(v) ? 'Names can\'t contain < > : " | ? * or \\.' : /(^|\/)\.{1,2}(\/|$)/.test(v) ? "Use a real name, not . or .." : validate ? validate(v, k) : "";
        if (msg) { form.querySelector("#mErr").textContent = msg; return; }
        done({ name: v, kind: k });
      };
    }
  );
}
function askLine({ title, label, value = "", ok = "Save", allowEmpty = false }) {
  return modal(
    `<h2>${esc(title)}</h2>
     <div><label for="mLine">${esc(label)}</label><input type="text" id="mLine" value="${esc(value)}" autocomplete="off" spellcheck="false"></div>
     <div class="row2"><button type="button" class="btn" data-cancel>Cancel</button><button type="submit" class="btn">${esc(ok)}</button></div>`,
    (form, done) => {
      const inp = form.querySelector("#mLine");
      setTimeout(() => { inp.focus(); inp.select(); }, 0);
      form.onsubmit = (e) => { e.preventDefault(); const v = inp.value.trim(); if (!v && !allowEmpty) return; done(v); };
    }
  );
}
function askText({ title, label, placeholder = "", ok = "OK" }) {
  return modal(
    `<h2>${esc(title)}</h2>
     <div><label for="mText">${esc(label)}</label><textarea id="mText" class="big" placeholder="${esc(placeholder)}" spellcheck="false"></textarea></div>
     <div class="row2"><button type="button" class="btn" data-cancel>Cancel</button><button type="submit" class="btn">${esc(ok)}</button></div>`,
    (form, done) => {
      const ta = form.querySelector("#mText");
      setTimeout(() => ta.focus(), 0);
      ta.addEventListener("keydown", (e) => { if (e.key === "Enter" && (e.ctrlKey || e.metaKey)) form.requestSubmit(); });
      form.onsubmit = (e) => { e.preventDefault(); done(ta.value.trim() || null); };
    }, true
  );
}
function confirmBox({ title, body, ok = "OK", danger = false }) {
  return modal(
    `<h2>${esc(title)}</h2><p>${body}</p>
     <div class="row2"><button type="button" class="btn" data-cancel>Cancel</button><button type="submit" class="btn${danger ? " danger" : ""}" id="mOk">${esc(ok)}</button></div>`,
    (form, done) => {
      setTimeout(() => form.querySelector("#mOk").focus(), 0);
      form.onsubmit = (e) => { e.preventDefault(); done(true); };
    }
  );
}
function pickFolder(title, exclude) {
  const all = ["", ...folders()];
  return modal(
    `<h2>${esc(title)}</h2>
     <div class="folders">${all.map((d) => {
       const bad = exclude && (within(d, exclude.path) && exclude.dir || d === folderOf(exclude.path));
       const depth = d ? d.split("/").length : 0;
       return `<button type="button" data-to="${esc(d)}" ${bad ? "disabled" : ""} style="padding-left:${10 + depth * 14}px">${d ? esc(fileName(d)) : "/ Vault root"}</button>`;
     }).join("")}</div>
     <div class="row2"><button type="button" class="btn" data-cancel>Cancel</button></div>`,
    (form, done) => {
      form.querySelectorAll("[data-to]").forEach((b) => (b.onclick = () => done(b.dataset.to)));
      form.onsubmit = (e) => e.preventDefault();
    }
  );
}
function pickVaultFile(title, filter) {
  const list = files().filter(filter);
  return modal(
    `<h2>${esc(title)}</h2>
     <input type="text" class="filter" id="mFilter" placeholder="Type to filter…" autocomplete="off" spellcheck="false" style="width:100%;background:var(--bg);border:1px solid var(--line2);border-radius:4px;padding:8px 10px;font-family:var(--f-mono);font-size:13px">
     <div class="folders" id="mList"></div>
     <div class="row2"><button type="button" class="btn" data-cancel>Cancel</button></div>`,
    (form, done) => {
      const inp = form.querySelector("#mFilter"), box = form.querySelector("#mList");
      const draw = () => {
        const q = inp.value.trim().toLowerCase();
        const shown = list.filter((p) => !q || p.toLowerCase().includes(q)).slice(0, 200);
        box.innerHTML = shown.length
          ? shown.map((p) => `<button type="button" data-pick="${esc(p)}">${esc(display(p))} <span style="color:var(--faint)">· ${esc(folderOf(p) || "root")}</span></button>`).join("")
          : '<div class="hint" style="padding:10px">No matching files.</div>';
        box.querySelectorAll("[data-pick]").forEach((b) => (b.onclick = () => done(b.dataset.pick)));
      };
      inp.addEventListener("input", draw);
      draw();
      setTimeout(() => inp.focus(), 0);
      form.onsubmit = (e) => { e.preventDefault(); const first = box.querySelector("[data-pick]"); if (first) done(first.dataset.pick); };
    }
  );
}
function askPassphrase({ title, body = "", confirm = false, old = false, ok = "Unlock" }) {
  return modal(
    `<h2>${esc(title)}</h2>${body ? `<p>${body}</p>` : ""}
     ${old ? '<div><label for="mOld">Current passphrase</label><input type="password" id="mOld" autocomplete="off"></div>' : ""}
     <div><label for="mPass">${old ? "New passphrase" : "Passphrase"}</label><input type="password" id="mPass" autocomplete="off"></div>
     ${confirm ? '<div><label for="mPass2">Type it again</label><input type="password" id="mPass2" autocomplete="off"></div>' : ""}
     <div class="err" id="mErr"></div>
     <div class="row2"><button type="button" class="btn" data-cancel>Cancel</button><button type="submit" class="btn" id="mOk">${esc(ok)}</button></div>`,
    (form, done) => {
      const first = form.querySelector(old ? "#mOld" : "#mPass");
      setTimeout(() => first.focus(), 0);
      form.onsubmit = (e) => {
        e.preventDefault();
        const p = form.querySelector("#mPass").value;
        const err = form.querySelector("#mErr");
        if (confirm && p.length < 8) return (err.textContent = "Use at least 8 characters. A few random words works well.");
        if (confirm && p !== form.querySelector("#mPass2").value) return (err.textContent = "The two passphrases don't match.");
        if (!p) return;
        done(old ? { old: form.querySelector("#mOld").value, pass: p } : p);
      };
    }
  );
}

/* ---------------- file operations ---------------- */
const TEMPLATE = {
  md: (name) => `# ${name}\n\n`,
  txt: () => "",
  canvas: () => JSON.stringify({ nodes: [], edges: [] }, null, "\t"),
  html: (name) => `<!doctype html>\n<html>\n<head>\n<meta charset="utf-8">\n<title>${esc(name)}</title>\n<style>\n  body { font-family: system-ui, sans-serif; margin: 2rem; }\n</style>\n</head>\n<body>\n  <h1>${esc(name)}</h1>\n</body>\n</html>\n`,
};
function withExt(name, kind) {
  const e = ext(name);
  if (["md", "markdown", "txt", "html", "htm", "canvas"].includes(e)) return name;
  return name + "." + kind;
}
async function newFile(dir = S.selDir, kind = "md", locked = false) {
  if (locked && !(await ensureUnlocked())) return;
  const r = await askName({
    title: (locked ? "New locked note" : "New file") + (dir ? " in " + fileName(dir) : ""),
    label: "Name",
    kinds: locked ? ["md", "txt"] : ["md", "txt", "html", "canvas"],
    kind,
    hint: locked ? "It's encrypted from the first keystroke. Nothing is written to disk in plain text." : "Use / to put it in a new subfolder, like Ideas/Big one",
    validate: (v, k) => {
      const p = join(dir, withExt(v, k)) + (locked ? ".lock" : "");
      return S.byPath.has(p) ? "A file with that name is already here." : "";
    },
  });
  if (!r) return;
  const plain = join(dir, withExt(r.name, r.kind));
  const base = fileName(plain).replace(/\.[^.]+$/, "");
  const content = TEMPLATE[kindFromExt(plain) || r.kind](base);
  const path = locked ? plain + ".lock" : plain;
  try {
    if (locked) await api("create_locked", { path, content });
    else await api("create_file", { path, content });
  } catch (e) { return toast(errText(e), true); }
  expandTo(path);
  await refresh();
  await open(path);
  if (baseKind(path) === "canvas") return;
  if (S.mode === "read") setMode("split");
  const ed = $("ed");
  ed.focus();
  ed.setSelectionRange(ed.value.length, ed.value.length);
}
async function newFolder(dir = S.selDir) {
  const r = await askName({
    title: "New folder" + (dir ? " in " + fileName(dir) : ""),
    label: "Folder name",
    hint: "Use / to make nested folders at once, like 2026/October",
    validate: (v) => (S.byPath.has(join(dir, v)) ? "Something with that name is already here." : ""),
  });
  if (!r) return;
  const path = join(dir, r.name);
  try { await api("create_folder", { path }); } catch (e) { return toast(errText(e), true); }
  expandTo(path + "/x");
  S.selDir = path;
  await refresh();
  toast("Created folder " + path);
}
function remap(oldP, newP) {
  const move = (p) => (within(p, oldP) ? newP + p.slice(oldP.length) : p);
  if (S.current) S.current = move(S.current);
  if (canvasPath) canvasPath = move(canvasPath);
  S.selDir = move(S.selDir);
  S.expanded = new Set([...S.expanded].map(move));
  const t = {};
  for (const p in S.texts) t[move(p)] = S.texts[p];
  S.texts = t;
  S.images = new Map([...S.images].map(([p, v]) => [move(p), v]));
}

/* ----- keeping links intact when things move ----- */
const stripLinkExt = (p) => p.replace(/\.lock$/i, "").replace(/\.(md|markdown)$/i, "");
/** Rewrite every link in `text` whose target resolves (in the old index) to a moved file. */
function rewriteLinks(text, oldFiles, moved, newFiles) {
  const nameFor = (newP, original) => {
    const hadPath = original.includes("/");
    const hadExt = /\.[a-z0-9]+$/i.test(fileName(original)) && !/\.(md|markdown)$/i.test(original);
    const isMd = baseKind(newP) === "md" || /\.(md|markdown)(\.lock)?$/i.test(newP);
    const short = isMd && !/\.(md|markdown)$/i.test(original) ? stripLinkExt(fileName(newP)) : fileName(newP).replace(/\.lock$/i, "");
    // Keep the short form only if it still points at exactly one file.
    const clash = newFiles.filter((f) => f !== newP && (fileName(f).toLowerCase() === fileName(newP).toLowerCase() || (isMd && stripLinkExt(fileName(f)).toLowerCase() === short.toLowerCase()))).length;
    if (!hadPath && !clash) return hadExt || !isMd ? fileName(newP).replace(/\.lock$/i, "") : short;
    return isMd && !/\.(md|markdown)$/i.test(original) ? stripLinkExt(newP) : newP.replace(/\.lock$/i, "");
  };
  let changed = 0;
  let out = text.replace(/(!?)\[\[([^\]|#]+)(#[^\]|]*)?(\|[^\]]*)?\]\]/g, (m, bang, target, head = "", alias = "") => {
    const old = resolveIn(oldFiles, target.trim());
    if (!old || !moved.has(old)) return m;
    const next = nameFor(moved.get(old), target.trim());
    if (next === target.trim()) return m;
    changed++;
    return `${bang}[[${next}${head}${alias}]]`;
  });
  out = out.replace(/(!\[[^\]]*\]\()([^)\s]+)(\))/g, (m, a, src, b) => {
    if (/^[a-z]+:/i.test(src)) return m;
    let dec = src; try { dec = decodeURIComponent(src); } catch {}
    const old = resolveIn(oldFiles, dec);
    if (!old || !moved.has(old)) return m;
    changed++;
    return a + encodeURI(moved.get(old)) + b;
  });
  return { text: out, changed };
}
function rewriteCanvas(json, moved) {
  try {
    const d = JSON.parse(json);
    let changed = 0;
    for (const n of d.nodes || []) if (n.type === "file" && moved.has(n.file)) { n.file = moved.get(n.file); changed++; }
    return { text: changed ? JSON.stringify(d, null, "\t") : json, changed };
  } catch { return { text: json, changed: 0 }; }
}
async function movePath(from, to) {
  if (from === to) return true;
  await flush();
  const oldFiles = files();
  const moved = new Map(oldFiles.filter((p) => within(p, from)).map((p) => [p, to + p.slice(from.length)]));
  try { await api("rename_path", { from, to }); }
  catch (e) { toast(errText(e), true); return false; }
  remap(from, to);
  graph && graph.remap(from, to);
  expandTo(to);
  await refresh();
  if (S.settings.links.autoUpdate && moved.size) {
    const newFiles = files();
    let notes = 0, links = 0, skipped = 0;
    for (const p of newFiles) {
      if (!hasText(p)) { if (isLocked(p)) skipped++; continue; }
      const k = baseKind(p);
      const r = k === "md" ? rewriteLinks(S.texts[p], oldFiles, moved, newFiles) : k === "canvas" ? rewriteCanvas(S.texts[p], moved) : null;
      if (!r || !r.changed) continue;
      try {
        await writeFile(p, r.text);
        S.texts[p] = r.text;
        notes++; links += r.changed;
        if (p === S.current && k !== "canvas") $("ed").value = r.text;
        if (p === S.current && k === "canvas" && canvas) canvas.load(r.text);
      } catch (e) { toast(`Couldn't update links in ${fileName(p)}. ${errText(e)}`, true); }
    }
    if (notes) { await refresh(); toast(`Updated ${links} link${links === 1 ? "" : "s"} in ${notes} file${notes === 1 ? "" : "s"}${skipped ? ` · ${skipped} locked note${skipped === 1 ? "" : "s"} skipped while the vault is locked` : ""}`); }
  }
  savePrefs();
  return true;
}
async function renameItem(p) {
  const e = S.byPath.get(p);
  if (!e) return;
  const locked = isLocked(p);
  const isMd = baseKind(p) === "md";
  const current = e.dir ? e.name : isMd ? display(p) : fileName(p).replace(/\.lock$/i, "");
  const finalName = (v) => {
    let n = e.dir ? v : isMd ? withExt(v, "md") : v;
    if (locked && !/\.lock$/i.test(n)) n += ".lock";
    return join(folderOf(p), n);
  };
  const r = await askName({
    title: "Rename " + (e.dir ? "folder" : "file"),
    label: "New name",
    value: current,
    ok: "Rename",
    hint: e.dir ? "" : isMd ? "The .md extension is kept for you." + (locked ? " It stays encrypted." : "") : "Keep the extension so the file stays in the vault.",
    validate: (v) => {
      const next = finalName(v);
      return next !== p && S.byPath.has(next) && next.toLowerCase() !== p.toLowerCase() ? "Something with that name is already here." : "";
    },
  });
  if (!r) return;
  const to = finalName(r.name);
  if (await movePath(p, to)) toast("Renamed to " + fileName(to));
}
async function moveItemDialog(p) {
  const e = S.byPath.get(p);
  if (!e) return;
  const dest = await pickFolder(`Move ${e.name} to…`, e);
  if (dest === null || dest === undefined) return;
  const to = join(dest, fileName(p));
  if (S.byPath.has(to)) return toast(`${dest || "The vault root"} already has something named ${fileName(p)}.`, true);
  if (await movePath(p, to)) toast(`Moved to ${dest || "vault root"}`);
}
async function deleteItem(p) {
  const e = S.byPath.get(p);
  if (!e) return;
  const inside = e.dir ? S.entries.filter((x) => !x.dir && within(x.path, p)).length : 0;
  const ok = await confirmBox({
    title: `Delete ${e.name}?`,
    body: e.dir
      ? `This moves the folder${inside ? ` and the ${inside} file${inside === 1 ? "" : "s"} in it` : ""} to the Recycle Bin. You can restore it from there.`
      : "This moves the file to the Recycle Bin. You can restore it from there.",
    ok: "Move to Recycle Bin",
    danger: true,
  });
  if (!ok) return;
  if (S.current && within(S.current, p)) { clearTimeout(S.saveTimer); S.dirty = false; }
  try {
    await api("delete_path", { path: p, permanent: false });
  } catch (err) {
    const hard = await confirmBox({
      title: "Recycle Bin unavailable",
      body: `${esc(errText(err))}<br><br>Delete <b>${esc(e.name)}</b> permanently instead? This can't be undone.`,
      ok: "Delete permanently",
      danger: true,
    });
    if (!hard) return;
    try { await api("delete_path", { path: p, permanent: true }); } catch (err2) { return toast(errText(err2), true); }
  }
  const wasCurrent = S.current && within(S.current, p);
  if (wasCurrent) { S.current = null; closeCanvas(); }
  await refresh();
  if (wasCurrent) {
    const next = files().find((q) => folderOf(q) === folderOf(p)) || files()[0];
    if (next) await open(next); else drawAll();
  }
  toast(`Moved ${e.name} to the Recycle Bin`);
}
function reportImport(rep, dest) {
  if (!rep) return;
  const parts = [];
  if (rep.added.length) parts.push(`Imported ${rep.added.length} file${rep.added.length === 1 ? "" : "s"} into ${dest || "the vault root"}`);
  else parts.push("Nothing was imported");
  if (rep.skipped.length) parts.push(`skipped ${rep.skipped.slice(0, 4).join(", ")}${rep.skipped.length > 4 ? ` and ${rep.skipped.length - 4} more` : ""} (only .md .txt .html .canvas .png .jpg)`);
  toast(parts.join(" · "), !rep.added.length || rep.skipped.length > 0);
}
async function afterImport(rep, dest) {
  if (!rep) return;
  rep.added.forEach(expandTo);
  if (dest) S.expanded.add(dest);
  await refresh();
  reportImport(rep, dest);
  if (rep.added.length === 1) await open(rep.added[0]);
}
async function importInto(dest = S.selDir, folders = false) {
  try {
    const rep = await api("pick_import", { dest, folders });
    await afterImport(rep, dest);
  } catch (e) { toast(errText(e), true); }
}
async function reveal(p) {
  try { await api("reveal_path", { path: p }); } catch (e) { toast(errText(e), true); }
}
async function followLink(target) {
  const hit = resolve(target);
  if (hit) return open(hit);
  if (["png", "jpg", "jpeg"].includes(ext(target))) return toast(`No image named ${target} yet. Import it and the embed will show.`, true);
  const dir = S.current ? folderOf(S.current) : S.selDir;
  const path = join(dir, withExt(target, "md"));
  try { await api("create_file", { path, content: TEMPLATE[kindFromExt(path) || "md"](fileName(path).replace(/\.[^.]+$/, "")) }); }
  catch (e) { return toast(errText(e), true); }
  await refresh();
  await open(path);
  toast("Created " + fileName(path));
}
function copyText(s) {
  const done = () => toast("Copied " + s);
  const fallback = () => {
    const ta = document.createElement("textarea");
    ta.value = s; document.body.appendChild(ta); ta.select();
    try { document.execCommand("copy"); done(); } catch { toast("Select and copy: " + s, true); }
    ta.remove();
  };
  try { navigator.clipboard.writeText(s).then(done, fallback); } catch { fallback(); }
}

/* ---------------- daily notes (add-on) ---------------- */
const pad2 = (n) => String(n).padStart(2, "0");
const DAYS = ["Sunday", "Monday", "Tuesday", "Wednesday", "Thursday", "Friday", "Saturday"];
const MONTHS = ["January", "February", "March", "April", "May", "June", "July", "August", "September", "October", "November", "December"];
function fmtDate(d, fmt) {
  return fmt
    .replace(/YYYY/g, d.getFullYear())
    .replace(/MMMM/g, MONTHS[d.getMonth()])
    .replace(/MM/g, pad2(d.getMonth() + 1))
    .replace(/dddd/g, DAYS[d.getDay()])
    .replace(/DD/g, pad2(d.getDate()));
}
function fmtTime(d) {
  if (S.settings.daily.clock === "12") { const h = d.getHours() % 12 || 12; return `${h}:${pad2(d.getMinutes())} ${d.getHours() < 12 ? "AM" : "PM"}`; }
  return `${pad2(d.getHours())}:${pad2(d.getMinutes())}`;
}
const longDate = (d) => `${DAYS[d.getDay()]}, ${MONTHS[d.getMonth()]} ${d.getDate()}, ${d.getFullYear()}`;
function fillTemplate(t, d, title) {
  return t.replace(/\{\{\s*date\s*\}\}/gi, longDate(d)).replace(/\{\{\s*time\s*\}\}/gi, fmtTime(d)).replace(/\{\{\s*title\s*\}\}/gi, title).replace(/\{\{\s*date:([^}]+)\}\}/gi, (_, f) => fmtDate(d, f.trim()));
}
async function openToday() {
  const cfg = S.settings.daily;
  if (!cfg.enabled) return;
  const d = new Date();
  const name = fmtDate(d, cfg.format || "YYYY-MM-DD");
  const path = join((cfg.folder || "").replace(/^\/+|\/+$/g, ""), name + ".md");
  let created = false;
  if (!S.byPath.has(path)) {
    let body = "";
    if (cfg.template) {
      const tp = resolve(cfg.template);
      if (tp && hasText(tp)) body = fillTemplate(S.texts[tp], d, name);
      else toast(`Template "${cfg.template}" wasn't found, so today's note starts blank.`, true);
    }
    if (!body) body = cfg.titleDate ? `# ${longDate(d)}\n\n` : "";
    try { await api("create_file", { path, content: body }); created = true; }
    catch (e) { return toast(errText(e), true); }
    expandTo(path);
    await refresh();
  }
  await open(path);
  if (S.mode === "read") setMode("split");
  const ed = $("ed");
  if (cfg.timeStamp) {
    const stamp = `## ${fmtTime(d)}\n`;
    const v = ed.value;
    ed.value = (v.trim() ? v.replace(/\s*$/, "") + "\n\n" : "") + stamp;
    ed.dispatchEvent(new Event("input"));
  }
  ed.focus();
  ed.setSelectionRange(ed.value.length, ed.value.length);
  ed.scrollTop = ed.scrollHeight;
  if (created) toast("Started today's note");
}
function insertAtCursor(s) {
  const ed = $("ed");
  if (!S.current || isImg(S.current) || baseKind(S.current) === "canvas" || $("panes").hidden) return;
  ed.focus();
  ed.setRangeText(s, ed.selectionStart, ed.selectionEnd, "end");
  ed.dispatchEvent(new Event("input"));
}

/* ---------------- encryption (add-on) ---------------- */
function drawLockBtn() {
  const b = $("lockBtn");
  if (!S.settings || !S.settings.lock.enabled) { b.hidden = true; return; }
  b.hidden = false;
  if (!S.lock.configured) { b.textContent = "SET UP ENCRYPTION"; b.className = "lockbtn"; b.title = "Choose a passphrase to start locking notes"; }
  else if (S.lock.unlocked) { b.textContent = "UNLOCKED"; b.className = "lockbtn open"; b.title = "Locked notes are readable. Click to lock them now."; }
  else { b.textContent = "LOCKED"; b.className = "lockbtn shut"; b.title = "Click to unlock your locked notes"; }
}
async function setupEncryption() {
  const pass = await askPassphrase({
    title: "Set a vault passphrase",
    body: "Locked notes are encrypted with this passphrase. <b>There is no way to recover it.</b> If you forget it, locked notes are gone for good. Write it down somewhere safe.",
    confirm: true,
    ok: "Set passphrase",
  });
  if (!pass) return false;
  try { await api("lock_setup", { passphrase: pass }); }
  catch (e) { toast(errText(e), true); return false; }
  await refresh();
  armAutoLock();
  toast("Encryption is ready. Right-click a note and choose Lock note.");
  return true;
}
async function unlockVault() {
  if (!S.lock.configured) return setupEncryption();
  for (let attempt = 0; attempt < 5; attempt++) {
    const pass = await askPassphrase({ title: "Unlock locked notes", body: attempt ? '<span style="color:var(--amber)">Wrong passphrase. Try again.</span>' : "" });
    if (!pass) return false;
    try { await api("lock_unlock", { passphrase: pass }); await refresh(); armAutoLock(); toast("Unlocked"); return true; }
    catch (e) { if (!/Wrong passphrase/i.test(errText(e))) { toast(errText(e), true); return false; } }
  }
  return false;
}
async function lockVault(quiet) {
  if (!S.lock.unlocked) return;
  if (S.current && isLocked(S.current)) await flush();
  await api("lock_lock").catch(() => {});
  for (const p of Object.keys(S.texts)) if (isLocked(p)) delete S.texts[p];
  if (S.current && isLocked(S.current) && baseKind(S.current) !== "canvas") $("ed").value = "";
  if (S.current && isLocked(S.current) && canvas) closeCanvas();
  clearTimeout(autoLockTimer);
  await refresh();
  if (!quiet) toast("Locked. Encrypted notes are hidden until you unlock again.");
}
async function ensureUnlocked() {
  if (!S.settings.lock.enabled) { toast("Turn on the Encryption add-on in Settings first.", true); return false; }
  if (!S.lock.configured) return setupEncryption();
  if (S.lock.unlocked) return true;
  return unlockVault();
}
async function lockFile(p) {
  if (!(await ensureUnlocked())) return;
  await flush();
  try {
    const np = await api("encrypt_path", { path: p });
    remap(p, np);
    graph && graph.remap(p, np);
    await refresh();
    if (S.current === np) { $("ed").value = S.texts[np] ?? ""; setSaved("ok", "ENCRYPTED"); }
    toast(`${display(np)} is now encrypted on disk`);
  } catch (e) { toast(errText(e), true); }
}
async function unlockFile(p) {
  if (!(await ensureUnlocked())) return;
  const ok = await confirmBox({ title: `Remove encryption from ${display(p)}?`, body: "The note will be saved as a normal, readable file again.", ok: "Remove encryption" });
  if (!ok) return;
  await flush();
  try {
    const np = await api("decrypt_path", { path: p });
    delete S.texts[p];
    remap(p, np);
    graph && graph.remap(p, np);
    await refresh();
    if (S.current === np) setSaved("ok", "SAVED");
    toast(`${display(np)} is a normal file again`);
  } catch (e) { toast(errText(e), true); }
}
async function changePassphrase() {
  const r = await askPassphrase({ title: "Change passphrase", old: true, confirm: true, ok: "Change" });
  if (!r) return;
  try { await api("lock_change", { old: r.old, new: r.pass }); toast("Passphrase changed"); await refresh(); }
  catch (e) { toast(errText(e), true); }
}
let autoLockTimer;
function armAutoLock() {
  clearTimeout(autoLockTimer);
  const min = +S.settings.lock.autoLockMin;
  if (!S.lock.unlocked || !min) return;
  autoLockTimer = setTimeout(() => lockVault(false), min * 60000);
}

/* ---------------- settings dialog ---------------- */
async function openSettings() {
  const st = JSON.parse(JSON.stringify(S.settings));
  const ag = await api("agent_info").catch(() => null);
  const bk = await api("backup_status").catch(() => null);
  const tog = (id, label, on) => `<label class="gtog"><span>${label}</span><input type="checkbox" id="${id}" ${on ? "checked" : ""}><i></i></label>`;
  const formats = ["YYYY-MM-DD", "YYYY-MM-DD dddd", "DD-MM-YYYY", "MM-DD-YYYY", "YYYY/MM/YYYY-MM-DD"];
  const result = await modal(
    `<h2>Settings</h2>
     <div class="setsec">
       <h3>Links</h3>
       ${tog("sLinks", "Update links when notes are renamed or moved", st.links.autoUpdate)}
     </div>
     <div class="setsec">
       <h3>Daily notes <span class="badge k-md">ADD-ON</span></h3>
       ${tog("sDaily", "Turn on daily notes", st.daily.enabled)}
       <div class="sub" id="dailySub" ${st.daily.enabled ? "" : "hidden"}>
         <div class="setrow"><span>Folder</span><input type="text" id="sDFolder" value="${esc(st.daily.folder)}" placeholder="Daily"></div>
         <div class="setrow"><span>File name</span><select id="sDFormat">${formats.map((f) => `<option ${f === st.daily.format ? "selected" : ""} value="${f}">${f} → ${esc(fmtDate(new Date(), f))}</option>`).join("")}</select></div>
         <div class="setrow"><span>Template note</span><input type="text" id="sDTemplate" value="${esc(st.daily.template)}" placeholder="optional, e.g. Templates/Daily"></div>
         ${tog("sDTitle", "Add the date as the note's title", st.daily.titleDate)}
         ${tog("sDTime", "Add a time stamp each time I open today's note", st.daily.timeStamp)}
         <div class="setrow"><span>Clock</span><select id="sDClock"><option value="24" ${st.daily.clock === "24" ? "selected" : ""}>24-hour (15:20)</option><option value="12" ${st.daily.clock === "12" ? "selected" : ""}>12-hour (3:20 PM)</option></select></div>
         <div class="setnote">Ctrl+D opens today's note. In any note, Ctrl+; inserts the date and Ctrl+Shift+; inserts the time. Templates can use <code>{{date}}</code>, <code>{{time}}</code>, <code>{{title}}</code> and <code>{{date:YYYY-MM-DD}}</code>.</div>
       </div>
     </div>
     <div class="setsec">
       <h3>Encryption <span class="badge k-locked">ADD-ON</span></h3>
       ${tog("sLock", "Turn on locked notes", st.lock.enabled)}
       <div class="sub" id="lockSub" ${st.lock.enabled ? "" : "hidden"}>
         <div class="setrow"><span>Lock again after</span><select id="sLockMin">${[[0, "Never (until I lock it)"], [5, "5 minutes"], [10, "10 minutes"], [30, "30 minutes"], [60, "1 hour"]].map(([v, l]) => `<option value="${v}" ${+st.lock.autoLockMin === v ? "selected" : ""}>${l}</option>`).join("")}</select></div>
         <div class="setnote">${S.lock.configured ? "A passphrase is set for this vault." : "You'll choose a passphrase the first time you lock a note."} Locked notes are encrypted with XChaCha20-Poly1305 using a key derived from your passphrase with Argon2id. Their text never touches the disk unencrypted.</div>
         <div class="setnote warn">There's no passphrase recovery. Forget it and locked notes can't be opened.</div>
         ${S.lock.configured ? '<div class="row2" style="justify-content:flex-start"><button type="button" class="btn" id="sChangePass">Change passphrase</button></div>' : ""}
       </div>
     </div>
     ${ag ? `<div class="setsec" id="agentSec">
       <h3>Null Search <span class="badge k-md">PAIRING</span></h3>
       <div class="setnote">Null Search can find notes in this vault through a small search service that only listens on this computer (${esc(ag.address)}). Paste the pairing token into Null Search to connect it.</div>
       <div class="setrow"><span>Pairing token</span><input type="text" id="sToken" class="tokenfield" readonly value="${esc(ag.token)}" spellcheck="false"></div>
       <div class="row2" style="justify-content:flex-start"><button type="button" class="btn" id="sCopyToken">Copy token</button><button type="button" class="btn" id="sNewToken">New token</button></div>
       <div class="setrow"><span>Null Search address</span><input type="text" id="sOrigin" value="${esc(ag.origin)}" placeholder="http://search.lan" spellcheck="false"></div>
       <div class="setnote">Only this address can query your notes from a browser. Leave it empty to block all browser access.</div>
       <div class="setnote ${/^Listening/.test(ag.status) ? "" : "warn"}" id="sAgentStatus">${esc(ag.status)}</div>
     </div>` : ""}
     ${bk ? `<div class="setsec" id="backupSec">
       <h3>Null Drive <span class="badge k-md">ADD-ON</span></h3>
       ${tog("sBackup", "Connect this vault to Null Drive", bk.enabled)}
       <div class="sub" id="backupSub" ${bk.enabled ? "" : "hidden"}>
         <div class="setrow"><span>Mode</span><select id="sBkMode">
           <option value="backup" ${bk.mode !== "sync" ? "selected" : ""}>Back up: this computer → Drive</option>
           <option value="sync" ${bk.mode === "sync" ? "selected" : ""}>Two-way sync: also bring changes from your other devices</option>
         </select></div>
         <div class="setnote" id="sBkModeNote"></div>
         <div class="setrow"><span>Null Drive address</span><input type="text" id="sBkServer" value="${esc(bk.server)}" placeholder="https://drive.example.com" spellcheck="false"></div>
         <div class="setrow"><span>API token</span><input type="password" id="sBkToken" value="" autocomplete="off" placeholder="${bk.has_token ? "Saved. Paste a new one to replace it" : "Paste a token from Drive"}" spellcheck="false"></div>
         <div class="setnote">In Drive: Settings → Devices → Add device → Create an API token instead, scope <b>Everything</b>. The token is kept in Windows Credential Manager.</div>
         <div class="setrow"><span>Folder on Drive</span><input type="text" id="sBkFolder" value="${esc(bk.folder)}" placeholder="${esc(bk.default_folder)}" spellcheck="false"></div>
         <div class="row2" style="justify-content:flex-start"><button type="button" class="btn" id="sBkTest">Test connection</button><button type="button" class="btn" id="sBkNow" ${bk.enabled ? "" : "disabled"}>Back up now</button></div>
         <div class="setnote" id="sBkStatus"></div>
       </div>
     </div>` : ""}
     <div class="row2"><button type="button" class="btn" data-cancel>Cancel</button><button type="submit" class="btn">Save</button></div>`,
    (form, done) => {
      form.querySelector("#sDaily").onchange = (e) => (form.querySelector("#dailySub").hidden = !e.target.checked);
      form.querySelector("#sLock").onchange = (e) => (form.querySelector("#lockSub").hidden = !e.target.checked);
      if (ag) {
        const tok = form.querySelector("#sToken");
        form.querySelector("#sCopyToken").onclick = async () => {
          try { await navigator.clipboard.writeText(tok.value); } catch { tok.select(); document.execCommand("copy"); }
          toast("Pairing token copied");
        };
        form.querySelector("#sNewToken").onclick = async () => {
          if (!confirm("Make a new pairing token? Null Search stops working until you paste the new one in.")) return;
          try { tok.value = (await api("agent_new_token")).token; toast("New pairing token made"); } catch (e) { toast(errText(e), true); }
        };
      }
      if (bk) {
        const q = (id) => form.querySelector("#" + id);
        q("sBackup").onchange = (e) => (q("backupSub").hidden = !e.target.checked);
        const modeNote = () => {
          const two = q("sBkMode").value === "sync";
          q("sBkModeNote").textContent = two
            ? "Changes go both ways, matched by note, so renames on any device follow. If a note changed in two places, both versions are kept as \"Title (conflict YYYY-MM-DD)\". Deletes go to the Recycle Bin here and to Drive's Recently deleted for 30 days."
            : "One way: changes in this vault are copied to your Drive. Nothing on Drive is ever copied back here. Files you delete go to Drive's Recently deleted for 30 days. Use two-way sync if other devices write to this folder.";
          q("sBkNow").textContent = two ? "Sync now" : "Back up now";
        };
        q("sBkMode").onchange = modeNote;
        modeNote();
        drawBackupStatus(q("sBkStatus"), bk);
        q("sBkTest").onclick = async () => {
          const b = q("sBkTest"); b.disabled = true; q("sBkStatus").textContent = "Connecting…"; q("sBkStatus").className = "setnote";
          try { q("sBkStatus").textContent = await api("backup_test", { server: q("sBkServer").value, token: q("sBkToken").value || null }); }
          catch (e) { q("sBkStatus").textContent = errText(e); q("sBkStatus").className = "setnote warn"; }
          b.disabled = false;
        };
        q("sBkNow").onclick = () => { api("backup_now").catch(() => {}); q("sBkStatus").textContent = "Backing up…"; };
      }
      const cp = form.querySelector("#sChangePass");
      if (cp) cp.onclick = () => { done("changepass"); };
      form.onsubmit = (e) => {
        e.preventDefault();
        const v = (id) => form.querySelector("#" + id);
        st.links.autoUpdate = v("sLinks").checked;
        st.daily.enabled = v("sDaily").checked;
        st.daily.folder = v("sDFolder").value.trim().replace(/[\\]/g, "/");
        st.daily.format = v("sDFormat").value;
        st.daily.template = v("sDTemplate").value.trim();
        st.daily.titleDate = v("sDTitle").checked;
        st.daily.timeStamp = v("sDTime").checked;
        st.daily.clock = v("sDClock").value;
        st.lock.enabled = v("sLock").checked;
        st.lock.autoLockMin = +v("sLockMin").value;
        if (ag) st.__origin = v("sOrigin").value.trim();
        if (bk) st.__backup = { enabled: v("sBackup").checked, mode: v("sBkMode").value, server: v("sBkServer").value.trim(), folder: v("sBkFolder").value.trim(), token: v("sBkToken").value.trim() || null };
        done(st);
      };
    }, true
  );
  if (result === "changepass") return changePassphrase();
  if (!result) return;
  if ("__origin" in result) {
    const origin = result.__origin;
    delete result.__origin;
    if (ag && origin !== ag.origin) {
      try { await api("agent_set_origin", { origin }); }
      catch (e) { toast(errText(e), true); return; }
    }
  }
  if ("__backup" in result) {
    const b = result.__backup;
    delete result.__backup;
    const changed = b.token || b.enabled !== bk.enabled || (b.enabled && (b.mode !== bk.mode || b.server !== bk.server || (b.folder || bk.default_folder) !== bk.folder));
    if (changed) {
      try { BK = await api("backup_configure", b); drawNet(); }
      catch (e) { toast(errText(e), true); return; }
    }
  }
  S.settings = result;
  await saveSettings();
  applyAddons();
  armAutoLock();
  toast("Settings saved");
}

/* ---------------- full-text search panel ---------------- */
function showTab(which) {
  const search = which === "search";
  $("tabFiles").setAttribute("aria-selected", String(!search));
  $("tabSearch").setAttribute("aria-selected", String(search));
  $("filesPane").hidden = search;
  $("searchPane").hidden = !search;
  if (search) { $("fq").focus(); $("fq").select(); runFullSearch(); }
}
let fsTimer;
function runFullSearch() {
  const raw = $("fq").value;
  const box = $("fResults"), sum = $("fSum");
  if (!raw.trim()) { box.innerHTML = ""; sum.textContent = "Searches the text of every note, page and canvas" + (S.lock.configured && !S.lock.unlocked ? ". Locked notes are skipped until you unlock." : "."); return; }
  const flags = $("fCase").checked ? "g" : "gi";
  const body = raw.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
  const re = new RegExp($("fWord").checked ? `\\b${body}\\b` : body, flags);
  let total = 0, filesHit = 0, html = "";
  const list = files().filter((p) => hasText(p)).sort((a, b) => display(a).localeCompare(display(b)));
  for (const p of list) {
    const text = S.texts[p];
    const lines = text.split("\n");
    const hits = [];
    let offset = 0;
    for (let i = 0; i < lines.length; i++) {
      const line = lines[i];
      re.lastIndex = 0;
      let m;
      while ((m = re.exec(line))) {
        hits.push({ i, start: offset + m.index, end: offset + m.index + m[0].length, col: m.index, len: m[0].length, line });
        if (!m[0].length) re.lastIndex++;
        if (hits.length > 200) break;
      }
      offset += line.length + 1;
    }
    const nameHit = (() => { re.lastIndex = 0; return re.test(display(p)); })();
    if (!hits.length && !nameHit) continue;
    filesHit++; total += hits.length;
    const shown = hits.slice(0, 8).map((h) => {
      const a = Math.max(0, h.col - 40), pre = h.line.slice(a, h.col), mid = h.line.slice(h.col, h.col + h.len), post = h.line.slice(h.col + h.len, h.col + h.len + 80);
      return `<button class="fhit" data-fopen="${esc(p)}" data-s="${h.start}" data-e="${h.end}"><span class="ln">${h.i + 1}</span>${a ? "…" : ""}${esc(pre)}<mark>${esc(mid)}</mark>${esc(post)}</button>`;
    }).join("");
    html += `<div class="fgroup"><button data-open="${esc(p)}">${esc(display(p))}<span>${hits.length || "name"}${isLocked(p) ? " · locked" : ""}</span></button>${shown}${hits.length > 8 ? `<div class="fhit" style="cursor:default">and ${hits.length - 8} more in this file</div>` : ""}</div>`;
    if (filesHit >= 150) break;
  }
  box.innerHTML = html || '<div class="empty" style="padding:10px 12px">No matches.</div>';
  sum.textContent = filesHit ? `${total} match${total === 1 ? "" : "es"} in ${filesHit} file${filesHit === 1 ? "" : "s"}` : "";
}
function bindFullSearch() {
  $("tabFiles").onclick = () => showTab("files");
  $("tabSearch").onclick = () => showTab("search");
  $("fq").addEventListener("input", () => { clearTimeout(fsTimer); fsTimer = setTimeout(runFullSearch, 120); });
  $("fq").addEventListener("keydown", (e) => {
    if (e.key === "Enter") { const first = $("fResults").querySelector(".fhit[data-fopen]"); if (first) first.click(); }
    if (e.key === "Escape") { $("fq").value = ""; runFullSearch(); }
  });
  $("fCase").onchange = runFullSearch;
  $("fWord").onchange = runFullSearch;
  $("fResults").addEventListener("click", (e) => {
    const h = e.target.closest("[data-fopen]");
    if (!h) return;
    e.stopPropagation();
    open(h.dataset.fopen, { start: +h.dataset.s, end: +h.dataset.e });
  });
}

/* ---------------- context menu ---------------- */
function showMenu(x, y, items, header) {
  const m = $("ctx");
  m.innerHTML = (header ? `<div class="hd">${esc(header)}</div>` : "") + items.map((it, i) =>
    it === "-" ? "<hr>" : `<button role="menuitem" data-i="${i}" class="${it.danger ? "danger" : ""}">${esc(it.label)}${it.key ? `<kbd>${it.key}</kbd>` : ""}</button>`
  ).join("");
  m.hidden = false;
  const r = m.getBoundingClientRect();
  m.style.left = Math.max(4, Math.min(x, innerWidth - r.width - 8)) + "px";
  m.style.top = Math.max(4, Math.min(y, innerHeight - r.height - 8)) + "px";
  m.querySelectorAll("[data-i]").forEach((b) => (b.onclick = () => { hideMenu(); items[+b.dataset.i].run(); }));
  const first = m.querySelector("button");
  if (first) first.focus();
}
function hideMenu() { $("ctx").hidden = true; }
function newItems(dir) {
  const items = [
    { label: dir ? "New note here" : "New note", key: dir ? "" : "Ctrl+N", run: () => newFile(dir, "md") },
    { label: dir ? "New canvas here" : "New canvas", run: () => newFile(dir, "canvas") },
    { label: dir ? "New text file here" : "New text file", run: () => newFile(dir, "txt") },
    { label: dir ? "New HTML page here" : "New HTML page", run: () => newFile(dir, "html") },
  ];
  if (S.settings.lock.enabled) items.push({ label: dir ? "New locked note here" : "New locked note", run: () => newFile(dir, "md", true) });
  items.push({ label: dir ? "New subfolder" : "New folder", key: dir ? "" : "Ctrl+Shift+N", run: () => newFolder(dir) });
  return items;
}
function menuFor(p, x, y) {
  const e = p ? S.byPath.get(p) : null;
  if (!e) {
    return showMenu(x, y, [
      ...newItems(""),
      "-",
      { label: "Import files…", run: () => importInto("", false) },
      { label: "Import folder…", run: () => importInto("", true) },
      "-",
      { label: "Show vault in Explorer", run: () => reveal("") },
    ], S.vault ? S.vault.name : "Vault");
  }
  if (e.dir) {
    return showMenu(x, y, [
      ...newItems(p),
      "-",
      { label: "Import files here…", run: () => importInto(p, false) },
      { label: "Import folder here…", run: () => importInto(p, true) },
      "-",
      { label: "Rename", key: "F2", run: () => renameItem(p) },
      { label: "Move to…", run: () => moveItemDialog(p) },
      { label: "Show in Explorer", run: () => reveal(p) },
      "-",
      { label: "Delete folder", key: "Del", danger: true, run: () => deleteItem(p) },
    ], p);
  }
  const items = [
    { label: "Open", run: () => open(p) },
    "-",
    { label: "Rename", key: "F2", run: () => renameItem(p) },
    { label: "Move to…", run: () => moveItemDialog(p) },
  ];
  if (e.kind === "img") items.push({ label: "Copy embed link", run: () => copyText(`![[${fileName(p)}]]`) });
  else items.push({ label: "Copy link", run: () => copyText(`[[${display(p)}]]`) });
  if (S.settings.lock.enabled) {
    if (isLocked(p)) items.push({ label: "Remove encryption…", run: () => unlockFile(p) });
    else if (["md", "txt", "html", "canvas"].includes(e.kind)) items.push({ label: "Lock note (encrypt)", run: () => lockFile(p) });
  }
  items.push({ label: "Show in Explorer", run: () => reveal(p) }, "-", { label: "Delete", key: "Del", danger: true, run: () => deleteItem(p) });
  showMenu(x, y, items, p);
}

/* ---------------- tree interaction: click, keyboard, drag to move ---------------- */
let drag = null, suppressClick = false;
function dropTargetAt(x, y) {
  const el = document.elementFromPoint(x, y);
  if (!el) return null;
  const row = el.closest(".row");
  if (row) {
    const p = row.dataset.path;
    return { dir: row.dataset.dir ? p : folderOf(p), el: row.dataset.dir ? row : null };
  }
  if (el.closest("#treeWrap")) return { dir: "", el: $("treeWrap") };
  return null;
}
function clearDropMarks() { document.querySelectorAll(".droptarget").forEach((n) => n.classList.remove("droptarget")); }

function bindTree() {
  const tree = $("treeWrap");
  tree.addEventListener("click", (e) => {
    if (suppressClick) { suppressClick = false; return; }
    const row = e.target.closest(".row");
    if (!row) { S.selDir = ""; drawTree(); return; }
    const p = row.dataset.path;
    if (row.dataset.dir) {
      S.selDir = p;
      S.expanded.has(p) ? S.expanded.delete(p) : S.expanded.add(p);
      drawTree(); savePrefs();
      if (!S.current) drawAll();
    } else open(p);
  });
  tree.addEventListener("contextmenu", (e) => {
    e.preventDefault();
    const row = e.target.closest(".row");
    menuFor(row ? row.dataset.path : null, e.clientX, e.clientY);
  });
  tree.addEventListener("keydown", (e) => {
    const rows = [...tree.querySelectorAll(".row")];
    const i = rows.indexOf(document.activeElement);
    if (e.key === "ArrowDown" || e.key === "ArrowUp") {
      e.preventDefault();
      const n = rows[Math.max(0, Math.min(rows.length - 1, i + (e.key === "ArrowDown" ? 1 : -1)))];
      if (n) n.focus();
    } else if ((e.key === "ArrowRight" || e.key === "ArrowLeft") && i >= 0 && rows[i].dataset.dir) {
      const p = rows[i].dataset.path;
      e.key === "ArrowRight" ? S.expanded.add(p) : S.expanded.delete(p);
      drawTree(); savePrefs();
      const again = tree.querySelector(`.row[data-path="${CSS.escape(p)}"]`);
      if (again) again.focus();
    } else if (e.key === "Delete" && i >= 0) { e.preventDefault(); deleteItem(rows[i].dataset.path); }
    else if (e.key === "F2" && i >= 0) { e.preventDefault(); renameItem(rows[i].dataset.path); }
  });
  tree.addEventListener("pointerdown", (e) => {
    const row = e.target.closest(".row");
    if (!row || e.button !== 0) return;
    drag = { path: row.dataset.path, x: e.clientX, y: e.clientY, started: false, row };
  });
  addEventListener("pointermove", (e) => {
    if (!drag) return;
    if (!drag.started) {
      if (Math.abs(e.clientX - drag.x) + Math.abs(e.clientY - drag.y) < 6) return;
      drag.started = true;
      drag.row.classList.add("dragging");
      $("ghost").textContent = "Move " + fileName(drag.path);
      $("ghost").hidden = false;
    }
    $("ghost").style.left = e.clientX + "px";
    $("ghost").style.top = e.clientY + "px";
    clearDropMarks();
    const t = dropTargetAt(e.clientX, e.clientY);
    if (t && t.el && t.dir !== folderOf(drag.path) && !(isDir(drag.path) && within(t.dir, drag.path))) t.el.classList.add("droptarget");
  });
  addEventListener("pointerup", async (e) => {
    if (!drag) return;
    const d = drag;
    drag = null;
    $("ghost").hidden = true;
    clearDropMarks();
    if (!d.started) return;
    suppressClick = true;
    setTimeout(() => (suppressClick = false), 0);
    d.row.classList.remove("dragging");
    const t = dropTargetAt(e.clientX, e.clientY);
    if (!t || t.dir === folderOf(d.path)) return;
    if (isDir(d.path) && within(t.dir, d.path)) return toast("A folder can't be moved inside itself.", true);
    const to = join(t.dir, fileName(d.path));
    if (S.byPath.has(to)) return toast(`${t.dir || "The vault root"} already has something named ${fileName(d.path)}.`, true);
    if (await movePath(d.path, to)) toast(`Moved ${fileName(d.path)} to ${t.dir || "vault root"}`);
  });
}

/* ---------------- files dropped from Explorer ---------------- */
function bindFileDrop() {
  let target = "";
  onFileDrop(async (ev) => {
    if (!S.vault) return;
    if (ev.type === "leave") { $("drop").hidden = true; clearDropMarks(); return; }
    const t = ev.x >= 0 ? dropTargetAt(ev.x, ev.y) : null;
    target = t ? t.dir : S.selDir;
    if (ev.type === "enter" || ev.type === "over") {
      clearDropMarks();
      if (t && t.el) t.el.classList.add("droptarget");
      $("dropText").textContent = "DROP TO IMPORT INTO " + (target ? fileName(target).toUpperCase() : "VAULT ROOT");
      $("drop").hidden = false;
      return;
    }
    if (ev.type === "drop") {
      $("drop").hidden = true;
      clearDropMarks();
      if (!ev.paths.length) return;
      try {
        const rep = await api("import_paths", { paths: ev.paths, dest: target });
        await afterImport(rep, target);
      } catch (e) { toast(errText(e), true); }
    }
  });
}

/* ---------------- editor ---------------- */
function bindEditor() {
  const ed = $("ed");
  ed.addEventListener("input", () => {
    if (!S.current || isImg(S.current) || baseKind(S.current) === "canvas") return;
    if (isLocked(S.current) && !S.lock.unlocked) return;
    S.texts[S.current] = ed.value;
    markDirty();
    drawAll();
    armAutoLock();
  });
  ed.addEventListener("keydown", (e) => {
    if (e.key === "Tab") {
      e.preventDefault();
      ed.setRangeText("  ", ed.selectionStart, ed.selectionEnd, "end");
      ed.dispatchEvent(new Event("input"));
    }
  });
  ed.addEventListener("paste", async (e) => {
    if (!S.current || baseKind(S.current) !== "md" || !e.clipboardData) return;
    const imgs = [...e.clipboardData.files].filter((f) => /^image\/(png|jpeg)$/.test(f.type));
    if (!imgs.length) return;
    e.preventDefault();
    if (isLocked(S.current)) toast("Heads up: pasted images are saved unencrypted in Attachments. Only note text is locked.", true);
    const pos = ed.selectionStart, embeds = [];
    for (const f of imgs) {
      const stamp = new Date().toISOString().replace(/[-:T]/g, "").slice(0, 14);
      const name = `Pasted ${stamp}${embeds.length ? "-" + embeds.length : ""}.${f.type === "image/png" ? "png" : "jpg"}`;
      const data = await new Promise((res, rej) => { const r = new FileReader(); r.onload = () => res(r.result); r.onerror = rej; r.readAsDataURL(f); });
      try { await api("write_image", { path: "Attachments/" + name, data }); embeds.push(`![[${name}]]`); }
      catch (err) { toast(errText(err), true); }
    }
    if (!embeds.length) return;
    await refresh();
    ed.setRangeText(embeds.join("\n"), pos, pos, "end");
    ed.dispatchEvent(new Event("input"));
    toast(`Saved ${embeds.length} image${embeds.length === 1 ? "" : "s"} to Attachments`);
  });
  $("doc").addEventListener("change", (e) => {
    if (!e.target.matches("input[type=checkbox][data-line]")) return;
    const lines = S.texts[S.current].split("\n"), n = +e.target.dataset.line;
    lines[n] = lines[n].replace(/\[( |x|X)\]/, e.target.checked ? "[x]" : "[ ]");
    ed.value = lines.join("\n");
    ed.dispatchEvent(new Event("input"));
  });
}

/* ---------------- modes ---------------- */
function setMode(m, quiet) {
  S.mode = m;
  [["mEdit", "edit"], ["mSplit", "split"], ["mRead", "read"]].forEach(([id, v]) => $(id).setAttribute("aria-pressed", v === m));
  if (!quiet) { drawAll(); savePrefs(); }
}

/* ---------------- quick search (top bar) ---------------- */
let hits = [], sel = 0;
function runSearch() {
  const raw = $("q").value.trim(), q = raw.toLowerCase(), box = $("results");
  if (!q) { box.hidden = true; return; }
  const tagQ = q.startsWith("#") ? q.slice(1) : null;
  const linkQ = q.match(/^\[\[(.+?)(\]\])?$/);
  hits = files().map((p) => {
    const name = fileName(p).toLowerCase();
    let score = 0, snip = "";
    if (tagQ) { if (tagsOf(p).some((t) => t.startsWith(tagQ))) score = 2; }
    else if (linkQ) { const target = linkQ[1].toLowerCase(); if (linksOf(p).some((l) => l.toLowerCase().includes(target))) score = 2; }
    else {
      if (name.includes(q)) score += 5;
      if (p.toLowerCase().includes(q)) score += 1;
      const text = baseKind(p) === "canvas" ? null : S.texts[p];
      if (text) {
        const idx = text.toLowerCase().indexOf(q);
        if (idx >= 0) { score += 1; const a = Math.max(0, idx - 40); snip = (a ? "…" : "") + text.slice(a, idx + q.length + 60).replace(/\n/g, " "); }
      }
    }
    return { p, score, snip };
  }).filter((h) => h.score > 0).sort((a, b) => b.score - a.score).slice(0, 14);
  sel = 0;
  const mark = (s) => {
    const e = esc(s);
    if (tagQ || linkQ) return e;
    const qq = esc(q), i = e.toLowerCase().indexOf(qq);
    return i < 0 ? e : e.slice(0, i) + "<mark>" + e.slice(i, i + qq.length) + "</mark>" + e.slice(i + qq.length);
  };
  box.innerHTML = hits.length
    ? hits.map((h, i) => `<button data-open="${esc(h.p)}" class="${i === 0 ? "on" : ""}"><div class="t">${esc(display(h.p))}<span style="color:var(--faint)"> · ${esc(folderOf(h.p) || "root")}${kindOf(h.p) !== "md" ? " · " + kindOf(h.p) : ""}</span></div>${h.snip ? `<div class="s">${mark(h.snip)}</div>` : ""}</button>`).join("") +
      `<button data-fullsearch="1"><div class="t" style="color:var(--dim)">Search every line for “${esc(raw)}” →</div></button>`
    : `<div class="none">No files match. Press Enter to create a note called “${esc(raw)}”.</div>`;
  box.hidden = false;
}
function bindSearch() {
  const q = $("q");
  q.addEventListener("input", runSearch);
  q.addEventListener("keydown", async (e) => {
    const btns = [...$("results").querySelectorAll("button")];
    if (e.key === "ArrowDown" || e.key === "ArrowUp") {
      e.preventDefault();
      if (!btns.length) return;
      sel = (sel + (e.key === "ArrowDown" ? 1 : -1) + btns.length) % btns.length;
      btns.forEach((b, i) => b.classList.toggle("on", i === sel));
      btns[sel].scrollIntoView({ block: "nearest" });
    }
    if (e.key === "Enter") {
      e.preventDefault();
      const v = q.value.trim();
      const pick = btns[sel];
      q.value = ""; $("results").hidden = true;
      if (pick && pick.dataset.fullsearch) { $("fq").value = v; showTab("search"); }
      else if (hits.length && btns.length) await open(hits[sel].p);
      else if (v && !/^[#\[]/.test(v)) await followLink(v);
    }
    if (e.key === "Escape") { q.value = ""; $("results").hidden = true; q.blur(); }
  });
  q.addEventListener("blur", () => setTimeout(() => ($("results").hidden = true), 150));
  $("results").addEventListener("mousedown", (e) => {
    const b = e.target.closest("[data-fullsearch]");
    if (!b) return;
    e.preventDefault();
    $("fq").value = q.value.trim();
    q.value = ""; $("results").hidden = true;
    showTab("search");
  });
}

/* ---------------- graph ---------------- */
let graph = null;
function bindGraph() {
  graph = createGraph({
    canvas: $("cv"),
    panel: $("gpanel"),
    files,
    linksOf,
    tagsOf,
    resolve,
    display,
    kindOf,
    current: () => S.current,
    onOpen: (p) => { closeGraph(); open(p); },
    onCreate: (label) => { closeGraph(); followLink(label); },
    menu: (x, y, items, header) => showMenu(x, y, items, header),
    loadMeta: () => api("read_meta", { name: "graph" }),
    saveMeta: (content) => api("write_meta", { name: "graph", content }),
  });
  $("graphBtn").onclick = openGraph;
  $("closeGraph").onclick = closeGraph;
  $("fitGraph").onclick = () => graph.fit();
  $("panelBtn").onclick = () => {
    const p = $("gpanel");
    p.hidden = !p.hidden;
    $("panelBtn").setAttribute("aria-expanded", String(!p.hidden));
    try { localStorage.setItem("nullnotes.graphPanel", p.hidden ? "0" : "1"); } catch {}
  };
  try { if (localStorage.getItem("nullnotes.graphPanel") === "0") { $("gpanel").hidden = true; $("panelBtn").setAttribute("aria-expanded", "false"); } } catch {}
}
async function openGraph() {
  if (!S.vault) return;
  await flush();
  $("graph").hidden = false;
  await graph.show();
}
function closeGraph() {
  if ($("graph").hidden) return;
  $("graph").hidden = true;
  graph.hide();
}

/* ---------------- global wiring ---------------- */
function bindGlobal() {
  $("wOpen").onclick = pickVault;
  $("wRecent").addEventListener("click", async (e) => {
    const b = e.target.closest("[data-vault]");
    if (!b) return;
    try { await enterVault(await api("open_vault", { path: b.dataset.vault })); }
    catch (err) { toast(errText(err), true); showWelcome(); }
  });
  $("vaultBtn").onclick = (e) => {
    const r = e.currentTarget.getBoundingClientRect();
    showMenu(r.left, r.bottom + 4, [
      { label: "Settings and add-ons…", run: openSettings },
      "-",
      { label: "Open another vault…", run: async () => { await flush(); await lockVault(true); pickVault(); } },
      { label: "Show vault in Explorer", run: () => reveal("") },
      { label: "Reload from disk", run: async () => { await flush(); await refresh(); toast("Reloaded"); } },
      "-",
      { label: "Close vault", run: async () => { await flush(); await api("close_vault"); S.vault = null; S.current = null; closeCanvas(); document.title = "Null Notes"; showWelcome(); } },
    ], S.vault ? S.vault.path : "");
  };
  $("settingsBtn").onclick = openSettings;
  $("newNoteBtn").onclick = () => newFile();
  $("newFolderBtn").onclick = () => newFolder();
  $("todayBtn").onclick = openToday;
  $("importBtn").onclick = (e) => {
    const r = e.currentTarget.getBoundingClientRect();
    const dest = S.selDir;
    showMenu(r.left, r.bottom + 4, [
      { label: "Import files…", run: () => importInto(dest, false) },
      { label: "Import a folder…", run: () => importInto(dest, true) },
    ], "Into " + (dest || "vault root"));
  };
  $("collapseBtn").onclick = () => { S.expanded.clear(); if (S.current) expandTo(S.current); drawTree(); savePrefs(); };
  $("blankNew").onclick = () => newFile();
  $("blankImport").onclick = () => importInto();
  $("renameBtn").onclick = () => S.current && renameItem(S.current);
  $("delBtn").onclick = () => S.current && deleteItem(S.current);
  $("mEdit").onclick = () => setMode("edit");
  $("mSplit").onclick = () => setMode("split");
  $("mRead").onclick = () => setMode("read");
  $("lightbox").onclick = () => ($("lightbox").hidden = true);
  $("lockedUnlock").onclick = () => unlockVault();
  $("lockBtn").onclick = () => (S.lock.unlocked ? lockVault() : unlockVault());

  document.addEventListener("click", (e) => {
    if (!e.target.closest("#ctx")) hideMenu();
    if (e.target.closest(".cv-stage") && !e.target.closest("[data-wl],[data-ext],[data-tag]")) return; // the canvas handles its own clicks
    const img = e.target.closest("img[data-img]");
    if (img && img.src && !e.target.closest(".cv-node")) { $("lbImg").src = img.src; $("lightbox").hidden = false; return; }
    const o = e.target.closest("[data-open]");
    if (o) { e.preventDefault(); open(o.dataset.open); return; }
    const w = e.target.closest("[data-wl]");
    if (w) { e.preventDefault(); followLink(w.dataset.wl); return; }
    const x = e.target.closest("[data-ext]");
    if (x) { e.preventDefault(); copyText(x.dataset.ext); return; }
    const t = e.target.closest("[data-tag],[data-tagf]");
    if (t) { S.tagFilter = S.tagFilter === (t.dataset.tag || t.dataset.tagf).toLowerCase() ? null : (t.dataset.tag || t.dataset.tagf).toLowerCase(); drawTree(); return; }
    const j = e.target.closest("[data-jump]");
    if (j) { const el = document.getElementById(j.dataset.jump); if (el) el.scrollIntoView({ behavior: "smooth", block: "start" }); }
  }, true);

  // App-like behaviour: no browser context menu outside text areas, no reload/print keys.
  document.addEventListener("contextmenu", (e) => {
    if (!e.target.closest("textarea, input, .doc, .plaintext")) e.preventDefault();
  });
  document.addEventListener("keydown", (e) => {
    const ctrl = e.ctrlKey || e.metaKey, k = e.key.toLowerCase();
    if (e.key === "Escape") { if (!$("ctx").hidden) hideMenu(); else { closeGraph(); $("lightbox").hidden = true; } }
    if (!$("modal").hidden) return;
    if (e.key === "F5" || (ctrl && (k === "r" || k === "p"))) { e.preventDefault(); return; }
    if (!S.vault) return;
    armAutoLock();
    if (ctrl && k === "k") { e.preventDefault(); $("q").focus(); $("q").select(); }
    else if (ctrl && e.shiftKey && k === "f") { e.preventDefault(); showTab("search"); }
    else if (ctrl && e.shiftKey && k === "n") { e.preventDefault(); newFolder(); }
    else if (ctrl && k === "n") { e.preventDefault(); newFile(); }
    else if (ctrl && k === "s") { e.preventDefault(); save(); }
    else if (ctrl && k === "g") { e.preventDefault(); $("graph").hidden ? openGraph() : closeGraph(); }
    else if (ctrl && k === "d" && S.settings.daily.enabled) { e.preventDefault(); openToday(); }
    else if (ctrl && (e.code === "Semicolon" || k === ";" || k === ":") && S.settings.daily.enabled && e.target === $("ed")) { e.preventDefault(); insertAtCursor(e.shiftKey ? fmtTime(new Date()) : fmtDate(new Date(), "YYYY-MM-DD")); }
    else if (ctrl && k === "e" && S.current && !isImg(S.current) && baseKind(S.current) !== "canvas") { e.preventDefault(); setMode(S.mode === "read" ? "split" : "read"); }
    else if (e.key === "F2" && S.current && !e.target.closest("#treeWrap, .cv-stage")) { e.preventDefault(); renameItem(S.current); }
  });
  addEventListener("pointerdown", () => armAutoLock(), { passive: true });

  // Pick up changes made in other apps when the window comes back into focus.
  addEventListener("focus", async () => { if (S.vault && !S.dirty && !S.saving) await refresh(); });
  addEventListener("beforeunload", () => { if (S.dirty) save(); });
}

/* ---------------- boot ---------------- */
/* ---------------- back up to Null Drive ---------------- */
let BK = null, lastBkError = "";
async function refreshBackup() {
  BK = await api("backup_status").catch(() => null);
  drawNet();
}
function agoText(iso) {
  if (!iso) return "";
  const s = Math.max(0, (Date.now() - new Date(iso).getTime()) / 1000);
  if (s < 60) return "just now";
  if (s < 3600) return Math.round(s / 60) + " min ago";
  if (s < 86400) return Math.round(s / 3600) + " h ago";
  return new Date(iso).toLocaleString();
}
function drawNet() {
  const el = $("netStat");
  if (!el) return;
  const on = BK && BK.enabled;
  let label = "LOCAL · NO NETWORK", title = "Notes are stored on this computer";
  if (on) {
    const two = BK.mode === "sync";
    const st = two
      ? { idle: BK.last_ok ? "SYNCED " + agoText(BK.last_ok).toUpperCase() : "SYNC ON", waiting: "SYNC PENDING", running: "SYNCING…", error: "SYNC ERROR" }[BK.state] || "SYNC ON"
      : { idle: BK.last_ok ? "BACKED UP " + agoText(BK.last_ok).toUpperCase() : "BACKUP ON", waiting: "BACKUP PENDING", running: "BACKING UP…", error: "BACKUP ERROR" }[BK.state] || "BACKUP ON";
    label = (two ? "LOCAL ⇄ DRIVE · " : "LOCAL + DRIVE · ") + st;
    title = (two ? `Synced both ways with ${BK.server} › ${BK.folder}` : `Backed up one way to ${BK.server} › ${BK.folder}`) + (BK.state === "error" ? "\n" + BK.message : "");
  }
  el.innerHTML = `<span class="dot${on && BK.state === "error" ? " bad" : ""}"></span>${esc(label)}`;
  el.title = title;
  el.classList.toggle("warn", !!(on && BK.state === "error"));
}
function drawBackupStatus(box, b) {
  if (!box || !b) return;
  const parts = [];
  const two = b.mode === "sync";
  if (b.state === "running") parts.push(two ? "Syncing…" : "Backing up…");
  else if (b.message) parts.push(b.message);
  if (b.last_ok && two) parts.push(`Last sync ${agoText(b.last_ok)}: ${b.uploaded} sent, ${b.downloaded} received${b.moved ? `, ${b.moved} renamed` : ""}${b.deleted ? `, ${b.deleted} deleted` : ""}.`);
  else if (b.last_ok) parts.push(`Last backup ${agoText(b.last_ok)}: ${b.uploaded} sent, ${b.deleted} to Drive's trash${b.moved ? `, ${b.moved} moved` : ""}, ${b.unchanged} unchanged.`);
  const conf = (b.conflicts || []).length ? `<div class="bkconf">Both versions kept: ${b.conflicts.slice(0, 8).map((c) => `<a href="#" data-open="${esc(c)}">${esc(fileName(c))}</a>`).join(", ")}</div>` : "";
  box.innerHTML = esc(parts.join(" ")) + conf + (b.problems && b.problems.length ? `<ul class="bkprob">${b.problems.slice(0, 8).map((p) => `<li>${esc(p)}</li>`).join("")}${b.problems.length > 8 ? `<li>…and ${b.problems.length - 8} more</li>` : ""}</ul>` : "");
  box.className = "setnote" + (b.state === "error" ? " warn" : "");
}
let lastConflicts = "";
function onBackupStatus(b) {
  BK = b;
  drawNet();
  drawBackupStatus(document.getElementById("sBkStatus"), b);
  const what = b.mode === "sync" ? "Sync" : "Backup";
  if (b.state === "error" && b.message !== lastBkError) toast(what + ": " + b.message, true);
  lastBkError = b.state === "error" ? b.message : "";
  const c = (b.conflicts || []).join("|");
  if (c && c !== lastConflicts && b.state === "idle") {
    toast(`Changed in two places, so both versions were kept: ${b.conflicts.slice(0, 3).map(fileName).join(", ")}${b.conflicts.length > 3 ? "…" : ""}`, true);
  }
  if (b.state === "idle") lastConflicts = c;
}
// conflict links in Settings open the copy
document.addEventListener("click", (e) => {
  const a = e.target.closest && e.target.closest("[data-open]");
  if (!a) return;
  e.preventDefault();
  const p = a.dataset.open;
  const m = document.getElementById("modal");
  if (m && !m.hidden) { const c = m.querySelector("[data-cancel]"); if (c) c.click(); }
  refresh().then(() => { if (S.byPath.has(p)) open(p); });
});

/* ---------------- nullnotes://open?id= links ---------------- */
let linkBusy = Promise.resolve();
function handleOpenLink() {
  // Serialise: two links clicked quickly must not switch vaults over each other.
  linkBusy = linkBusy.then(async () => {
    const req = await api("take_pending_open").catch(() => null);
    if (!req) return;
    if (req.error) { toast(req.error, true); return; }
    try {
      if (!S.vault || S.vault.path !== req.vault.path) {
        // Save and lock the current vault first, exactly like "Open another vault".
        await flush();
        await lockVault(true);
        await enterVault(await api("open_vault", { path: req.vault.path }));
      } else {
        await refresh(); // the note may be brand new
      }
      if (!S.byPath.has(req.path)) { toast("That note isn't in this vault anymore.", true); return; }
      closeGraph();
      showTab("files");
      await open(req.path);
    } catch (e) { toast(errText(e), true); }
  });
  return linkBusy;
}

async function boot() {
  bindGlobal();
  bindTree();
  bindEditor();
  bindSearch();
  bindFullSearch();
  bindGraph();
  bindFileDrop();
  if (!native) document.title = "Null Notes (browser demo)";
  onEvent("open-note", handleOpenLink);
  // Files changed on disk outside the editor (sync, Explorer, git): reload, at most once a second.
  let vcTimer = null;
  onEvent("vault-changed", () => { if (!vcTimer) vcTimer = setTimeout(() => { vcTimer = null; if (S.vault) refresh(); }, 1000); });
  onEvent("backup-status", (e) => onBackupStatus(e.payload));
  try {
    const v = await api("vault_info");
    if (v) await enterVault(v); else await showWelcome();
    await handleOpenLink(); // launched by a link
  } catch (e) {
    await showWelcome();
    toast(errText(e), true);
  }
}
boot();
