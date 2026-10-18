// Bridge to the Rust side. Inside the desktop app this calls Tauri commands.
// Opened in a plain browser (for UI work), it falls back to an in-memory demo
// vault so the interface can be tried without building the app.

const T = window.__TAURI__;
export const native = !!(T && T.core);

export async function api(cmd, args = {}) {
  if (native) return T.core.invoke(cmd, args);
  return mock(cmd, args);
}

/** App events from the Rust side (no-op in the browser demo). */
export function onEvent(name, handler) {
  if (native) T.event.listen(name, handler);
}

/**
 * Files dragged in from Explorer. handler({type, paths, x, y}) where type is
 * enter | over | drop | leave and x/y are CSS pixels in the window.
 */
export function onFileDrop(handler) {
  if (native) {
    const dpr = () => window.devicePixelRatio || 1;
    const pos = (p) => (p && p.position ? { x: p.position.x / dpr(), y: p.position.y / dpr() } : { x: -1, y: -1 });
    T.event.listen("tauri://drag-enter", (e) => handler({ type: "enter", paths: e.payload.paths || [], ...pos(e.payload) }));
    T.event.listen("tauri://drag-over", (e) => handler({ type: "over", paths: [], ...pos(e.payload) }));
    T.event.listen("tauri://drag-drop", (e) => handler({ type: "drop", paths: e.payload.paths || [], ...pos(e.payload) }));
    T.event.listen("tauri://drag-leave", () => handler({ type: "leave", paths: [], x: -1, y: -1 }));
    return;
  }
  // Browser demo: read dropped files straight into the mock vault.
  let depth = 0;
  const has = (e) => e.dataTransfer && [...e.dataTransfer.types].includes("Files");
  addEventListener("dragenter", (e) => { if (!has(e)) return; e.preventDefault(); depth++; handler({ type: "enter", paths: [], x: e.clientX, y: e.clientY }); });
  addEventListener("dragover", (e) => { if (!has(e)) return; e.preventDefault(); handler({ type: "over", paths: [], x: e.clientX, y: e.clientY }); });
  addEventListener("dragleave", (e) => { if (!has(e)) return; depth = Math.max(0, depth - 1); if (!depth) handler({ type: "leave", paths: [], x: -1, y: -1 }); });
  addEventListener("drop", async (e) => {
    if (!has(e)) return;
    e.preventDefault(); depth = 0;
    const ids = await stash([...e.dataTransfer.files]);
    handler({ type: "drop", paths: ids, x: e.clientX, y: e.clientY });
  });
}

/* ---------------- browser demo vault ---------------- */

const M = { open: false, dirs: new Set(), files: new Map(), staged: new Map(), meta: new Map(), agent: { token: "demo-token-not-real", origin: "" }, backup: { enabled: false, server: "", folder: "", has_token: false } };
const KINDS = { md: "md", markdown: "md", txt: "txt", html: "html", htm: "html", canvas: "canvas", png: "img", jpg: "img", jpeg: "img" };
const extOf = (p) => (p.match(/\.([^./]+)$/) || [, ""])[1].toLowerCase();
const kindOf = (p) => (extOf(p) === "lock" ? (["md", "txt", "html", "canvas"].includes(KINDS[extOf(p.slice(0, -5))]) ? "locked" : null) : KINDS[extOf(p)] || null);
const TEXTK = ["md", "txt", "html", "canvas"];
const L = { pass: null, unlocked: false };
const needKey = () => { if (!L.unlocked) fail("The vault is locked. Unlock it first."); };
const parentOf = (p) => (p.includes("/") ? p.slice(0, p.lastIndexOf("/")) : "");
const fail = (msg) => { throw msg; };

function clean(rel) {
  rel = String(rel).replace(/\\/g, "/").replace(/^\/+|\/+$/g, "");
  if (!rel) return "";
  for (const part of rel.split("/")) {
    if (!part.trim() || part === "." || part === "..") fail(`"${rel}" isn't a valid path inside the vault.`);
    if (/[<>:"|?*]/.test(part)) fail(`"${part}" contains a character Windows doesn't allow in names (< > : " | ? *).`);
  }
  return rel;
}
function ensureDirs(rel) {
  const parts = rel.split("/");
  for (let i = 1; i <= parts.length; i++) M.dirs.add(parts.slice(0, i).join("/"));
}
function exists(p) { return M.dirs.has(p) || M.files.has(p); }
function unique(dir, name) {
  const join = (n) => (dir ? dir + "/" + n : n);
  if (!exists(join(name))) return join(name);
  const i = name.lastIndexOf(".");
  const stem = i > 0 ? name.slice(0, i) : name, ext = i > 0 ? name.slice(i) : "";
  let n = 1;
  while (exists(join(`${stem} ${n}${ext}`))) n++;
  return join(`${stem} ${n}${ext}`);
}
function put(path, data) { if (parentOf(path)) ensureDirs(parentOf(path)); M.files.set(path, { data, modified: Date.now() }); }

async function stash(files) {
  const ids = [];
  for (const f of files) {
    const id = "demo-upload:" + Math.random().toString(36).slice(2) + "/" + f.name;
    M.staged.set(id, f);
    ids.push(id);
  }
  return ids;
}
const readAs = (f, data) => new Promise((res, rej) => {
  const r = new FileReader();
  r.onload = () => res(r.result); r.onerror = () => rej(r.error);
  data ? r.readAsDataURL(f) : r.readAsText(f);
});

function seed() {
  const c = document.createElement("canvas");
  c.width = 520; c.height = 200;
  const x = c.getContext("2d");
  x.fillStyle = "#0a0a0f"; x.fillRect(0, 0, 520, 200);
  [["#00e5ff", 30, "VAULT"], ["#ff2e88", 200, "NOTES"], ["#39ff88", 370, "DISK"]].forEach(([col, bx, t]) => {
    x.shadowColor = col; x.shadowBlur = 12; x.strokeStyle = col; x.lineWidth = 2; x.strokeRect(bx, 70, 120, 60);
    x.shadowBlur = 0; x.fillStyle = "#d9def0"; x.font = "bold 14px monospace"; x.textAlign = "center"; x.fillText(t, bx + 60, 105);
  });
  x.fillStyle = "#5a6288"; x.font = "11px monospace"; x.textAlign = "left"; x.fillText("// every note is a real file", 30, 40);
  put("Attachments/vault-map.png", c.toDataURL("image/png"));
  put("Welcome.md", `# Welcome to Null Notes

This is the browser demo. In the desktop app the vault is a real folder on your disk.

![[vault-map.png]]

## Try it
- [ ] Right-click a folder to make a subfolder
- [ ] Drag a note onto a folder to move it
- [ ] Import a file with the Import button
- [ ] Link to a note that doesn't exist yet, like [[My First Note]]

See [[Projects/Null Search|Null Search]] and [[todo.txt]]. #welcome`);
  put("Projects/Null Search.md", "# Null Search\n\nOne search bar for the vault and the web.\n\nBack to [[Welcome]]. #nullgrid");
  put("Projects/Null Notes/Plan.md", "# Plan\n\n- [x] Desktop app\n- [x] Folders and subfolders\n- [ ] Store submission\n\nRelated: [[Null Search]] #nullgrid #project");
  put("Projects/Null Notes/start.html", "<!doctype html>\n<html>\n<body style=\"background:#0a0a0f;color:#00e5ff;font-family:monospace;display:grid;place-items:center;height:90vh\">\n  <h1>// NULLGRID</h1>\n</body>\n</html>\n");
  put("Inbox/todo.txt", "[ ] order drives\n[x] buy nullgrid.dev\n");
  put("Homelab/Network.md", "# Network\n\n```mermaid\nflowchart LR\n  ISP[ISP router] --> FW{OPNsense}\n  FW -->|LAN| SW[Switch]\n  SW --> PVE[Proxmox]\n  SW --> PI[Pi 4]\n```\n\nSee [[Null Search]]. #homelab");
  put("Boards/Lab plan.canvas", JSON.stringify({ nodes: [
    { id: "a", type: "text", text: "# Lab plan\nWhat goes where", x: 0, y: 0, width: 260, height: 110, color: "5" },
    { id: "b", type: "file", file: "Projects/Null Search.md", x: 380, y: -40, width: 300, height: 180 },
    { id: "c", type: "text", text: "Buy 2x 16TB", x: 380, y: 200, width: 220, height: 70, color: "2" },
  ], edges: [{ id: "e1", fromNode: "a", toNode: "b", toEnd: "arrow" }, { id: "e2", fromNode: "a", toNode: "c", toEnd: "arrow", label: "next" }] }, null, "\t"));
  ensureDirs("Archive/2026");
}

async function mock(cmd, a) {
  switch (cmd) {
    case "vault_info": if (!M.files.size) seed(); M.open = true; return { path: "C:\\Users\\you\\Documents\\Null Notes", name: "Null Notes" };
    case "read_meta": return M.meta.get(a.name) ?? null;
    case "write_meta": M.meta.set(a.name, a.content); return null;
    case "recent_vaults": return [];
    case "open_vault":
    case "pick_vault": if (!M.files.size) seed(); M.open = true; return { path: "C:\\Users\\you\\Documents\\Null Notes", name: "Null Notes" };
    case "close_vault": M.open = false; return null;
    case "list_entries": {
      const out = [];
      M.dirs.forEach((d) => out.push({ path: d, name: d.split("/").pop(), dir: true, kind: "dir", size: 0, modified: 0 }));
      M.files.forEach((f, p) => out.push({ path: p, name: p.split("/").pop(), dir: false, kind: kindOf(p), size: f.data.length, modified: f.modified }));
      return out.sort((x, y) => x.path.toLowerCase().localeCompare(y.path.toLowerCase()));
    }
    case "read_all_text": {
      const o = {};
      M.files.forEach((f, p) => { if (TEXTK.includes(kindOf(p))) o[p] = f.data; });
      return o;
    }
    case "read_text": { const p = clean(a.path); if (kindOf(p) === "locked") fail("That note is locked. Unlock the vault to open it."); const f = M.files.get(p); if (!f) fail("That file doesn't exist anymore."); return f.data; }
    case "lock_status": return { configured: !!L.pass, unlocked: L.unlocked };
    case "lock_setup": if (L.pass) fail("This vault already has a passphrase."); if ((a.passphrase || "").length < 8) fail("Use a passphrase of at least 8 characters."); L.pass = a.passphrase; L.unlocked = true; return null;
    case "lock_unlock": if (a.passphrase !== L.pass) fail("Wrong passphrase."); L.unlocked = true; return null;
    case "lock_change": if (a.old !== L.pass) fail("Wrong passphrase."); L.pass = a.new; L.unlocked = true; return null;
    case "lock_lock": L.unlocked = false; return null;
    case "encrypt_path": { needKey(); const p = clean(a.path), np = p + ".lock"; if (exists(np)) fail(`${np} already exists.`); const f = M.files.get(p); M.files.delete(p); put(np, f.data); return np; }
    case "decrypt_path": { needKey(); const p = clean(a.path), np = p.replace(/\.lock$/, ""); if (exists(np)) fail(`${np} already exists.`); const f = M.files.get(p); M.files.delete(p); put(np, f.data); return np; }
    case "read_locked": { needKey(); const f = M.files.get(clean(a.path)); if (!f) fail("That file doesn't exist anymore."); return f.data; }
    case "write_locked": needKey(); put(clean(a.path), a.content); return null;
    case "create_locked": { needKey(); const p = clean(a.path); if (exists(p)) fail(`${p} already exists.`); put(p, a.content); return null; }
    case "read_all_locked": { needKey(); const o = {}; M.files.forEach((f, p) => { if (kindOf(p) === "locked") o[p] = f.data; }); return o; }
    case "read_image": { const f = M.files.get(clean(a.path)); if (!f) fail("That image doesn't exist anymore."); return f.data; }
    case "write_text": { const p = clean(a.path); if (!TEXTK.includes(kindOf(p))) fail("Only .md, .txt, .html and .canvas files can be edited as text."); put(p, a.content); return null; }
    case "write_image": { const p = clean(a.path); if (exists(p)) fail(`${p} already exists.`); put(p, a.data); return null; }
    case "create_file": { const p = clean(a.path); if (!TEXTK.includes(kindOf(p))) fail("Only .md, .txt, .html and .canvas files can be created."); if (exists(p)) fail(`${p} already exists.`); put(p, a.content); return null; }
    case "create_folder": { const p = clean(a.path); if (!p) fail("Give the folder a name."); if (exists(p)) fail(`${p} already exists.`); ensureDirs(p); return null; }
    case "rename_path": {
      const from = clean(a.from), to = clean(a.to);
      if (!exists(from)) fail(`${from} doesn't exist anymore.`);
      if (exists(to) && from.toLowerCase() !== to.toLowerCase()) fail(`${to} already exists.`);
      if (M.dirs.has(from)) {
        if (to === from || to.startsWith(from + "/")) fail("A folder can't be moved inside itself.");
        const move = (p) => (p === from ? to : to + p.slice(from.length));
        const dirs = [...M.dirs], files = [...M.files];
        dirs.forEach((d) => { if (d === from || d.startsWith(from + "/")) { M.dirs.delete(d); M.dirs.add(move(d)); } });
        files.forEach(([p, f]) => { if (p.startsWith(from + "/")) { M.files.delete(p); M.files.set(move(p), f); } });
        if (parentOf(to)) ensureDirs(parentOf(to));
      } else {
        if (!kindOf(to)) fail("Keep a .md, .txt, .html, .png or .jpg extension so the file stays in the vault.");
        const f = M.files.get(from); M.files.delete(from); put(to, f.data);
      }
      return null;
    }
    case "delete_path": {
      const p = clean(a.path);
      M.dirs.forEach((d) => { if (d === p || d.startsWith(p + "/")) M.dirs.delete(d); });
      [...M.files.keys()].forEach((f) => { if (f === p || f.startsWith(p + "/")) M.files.delete(f); });
      return null;
    }
    case "import_paths": return importStaged(a.paths, clean(a.dest));
    case "pick_import": {
      const files = await new Promise((res) => {
        const inp = document.createElement("input");
        inp.type = "file"; inp.multiple = true;
        if (a.folders) inp.webkitdirectory = true;
        else inp.accept = ".md,.markdown,.txt,.html,.htm,.png,.jpg,.jpeg";
        inp.onchange = () => res([...inp.files]);
        inp.click();
      });
      if (!files.length) return null;
      const ids = [];
      for (const f of files) {
        const rel = f.webkitRelativePath || f.name;
        const id = "demo-upload:x/" + rel;
        M.staged.set(id, f); ids.push(id);
      }
      return importStaged(ids, clean(a.dest));
    }
    case "reveal_path": fail("The browser demo has no file manager to open.");
    case "take_pending_open": return null;
    case "backup_status": return { ...M.backup, default_folder: "Null Notes/Null Notes", folder: M.backup.folder || "Null Notes/Null Notes", state: M.backup.enabled ? "idle" : "off", message: "", problems: [] };
    case "backup_configure": if (a.enabled && !a.server) fail("Enter the Null Drive address, like https://drive.example.com"); Object.assign(M.backup, { enabled: a.enabled, server: a.server, folder: a.folder, has_token: M.backup.has_token || !!a.token }); return mock("backup_status", {});
    case "backup_test": fail("The browser demo can't reach Null Drive.");
    case "backup_now": return null;
    case "agent_info": return { ...M.agent, address: "http://127.0.0.1:47821", status: "Not running in the browser demo", indexed: false };
    case "agent_set_origin": M.agent.origin = a.origin; return mock("agent_info", {});
    case "agent_new_token": M.agent.token = [...crypto.getRandomValues(new Uint8Array(32))].map((b) => b.toString(16).padStart(2, "0")).join(""); return mock("agent_info", {});
    default: fail("Unknown command " + cmd);
  }
}

async function importStaged(ids, dest) {
  if (dest) ensureDirs(dest);
  const added = [], skipped = [];
  const roots = new Map();
  for (const id of ids) {
    const f = M.staged.get(id); M.staged.delete(id);
    if (!f) continue;
    const rel = id.split("/").slice(1).join("/");
    const k = kindOf(rel);
    if (!k) { skipped.push(rel.split("/").pop()); continue; }
    let target;
    if (rel.includes("/")) {
      const top = rel.split("/")[0];
      if (!roots.has(top)) roots.set(top, unique(dest, top));
      target = roots.get(top) + rel.slice(top.length);
      if (exists(target)) target = unique(parentOf(target), target.split("/").pop());
    } else target = unique(dest, rel);
    put(target, await readAs(f, k === "img"));
    added.push(target);
  }
  return { added, skipped };
}
