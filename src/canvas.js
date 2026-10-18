// Canvas: a freeform board stored as JSON Canvas (.canvas), the open format
// Obsidian uses, so boards move between the two apps.
//
// Cards (text, note, image, link) and groups are DOM elements in a zoomable
// "world"; connections are drawn in an SVG layer underneath.

import { render, esc } from "./markdown.js";

const COLORS = { 1: "#ff4d6d", 2: "#ff9f43", 3: "#f5e663", 4: "#39ff88", 5: "#00e5ff", 6: "#a58bff" };
const COLOR_NAMES = { 1: "Red", 2: "Orange", 3: "Yellow", 4: "Green", 5: "Cyan", 6: "Purple" };
const colorOf = (c) => (!c ? null : COLORS[c] || (/^#[0-9a-f]{3,8}$/i.test(c) ? c : null));
const uid = () => Math.random().toString(16).slice(2, 10) + Math.random().toString(16).slice(2, 10);
const DIRS = { top: [0, -1], bottom: [0, 1], left: [-1, 0], right: [1, 0] };

export function createCanvas(host, ctx) {
  // ctx: { renderCtx, hydrate(el), loadImage(p), readText(p) -> Promise<string>, open(p), kindOf(p), display(p),
  //        files(), menu(x,y,items,header), askText(opts), askLine(opts), pickFile(title, filter), onChange(json),
  //        graphSnapshot() -> Promise<{nodes, links}>, toast(msg, warn) }
  host.innerHTML = `
    <div class="cv-wrap">
      <div class="cv-bar">
        <button class="btn" data-act="card" title="Text card (or double-click the board)">+ Card</button>
        <button class="btn" data-act="note" title="A note from the vault">+ Note</button>
        <button class="btn" data-act="image" title="An image from the vault">+ Image</button>
        <button class="btn" data-act="group" title="A box to group cards">+ Group</button>
        <span class="cv-sep"></span>
        <button class="btn" data-act="import">Import ▾</button>
        <span class="cv-sep"></span>
        <button class="btn icon" data-act="undo" title="Undo (Ctrl+Z)" aria-label="Undo">↶</button>
        <button class="btn icon" data-act="redo" title="Redo (Ctrl+Y)" aria-label="Redo">↷</button>
        <button class="btn" data-act="fit">Fit</button>
        <span class="cv-zoom" data-zoom>100%</span>
      </div>
      <div class="cv-stage" tabindex="0">
        <div class="cv-world">
          <svg class="cv-edges" width="1" height="1"></svg>
          <div class="cv-nodes"></div>
        </div>
        <div class="cv-marquee" hidden></div>
        <div class="cv-empty" hidden>
          <b>Empty canvas</b>
          <span>Double-click anywhere to add a card. Drag from the dot on a card's edge to connect it to another card.</span>
          <span>Or use Import to bring in your graph, an Obsidian canvas, a mermaid flowchart or a CSV list of links.</span>
        </div>
      </div>
    </div>`;
  const stage = host.querySelector(".cv-stage");
  const world = host.querySelector(".cv-world");
  const svg = host.querySelector(".cv-edges");
  const layer = host.querySelector(".cv-nodes");
  const marquee = host.querySelector(".cv-marquee");
  const zoomEl = host.querySelector("[data-zoom]");

  let data = { nodes: [], edges: [] };
  let view = { x: 80, y: 80, k: 1 };
  let sel = new Set();
  let editing = null;
  let undo = [], redo = [];
  let op = null; // current pointer operation

  /* ---------- model helpers ---------- */
  const byId = (id) => data.nodes.find((n) => n.id === id);
  const edgeById = (id) => data.edges.find((e) => e.id === id);
  function snapshot() { return JSON.stringify(data); }
  function commit(before) {
    if (before !== undefined && before !== snapshot()) { undo.push(before); if (undo.length > 100) undo.shift(); redo = []; }
    ctx.onChange(JSON.stringify(data, null, "\t"));
  }
  function mutate(fn) { const before = snapshot(); fn(); renderAll(); commit(before); }

  function sanitize(d) {
    const out = { nodes: [], edges: [] };
    const ids = new Set();
    for (const n of Array.isArray(d && d.nodes) ? d.nodes : []) {
      if (!n || typeof n !== "object" || !["text", "file", "link", "group"].includes(n.type)) continue;
      const id = typeof n.id === "string" && n.id && !ids.has(n.id) ? n.id : uid();
      ids.add(id);
      out.nodes.push({ ...n, id, x: +n.x || 0, y: +n.y || 0, width: Math.max(40, +n.width || 250), height: Math.max(30, +n.height || 120) });
    }
    for (const e of Array.isArray(d && d.edges) ? d.edges : []) {
      if (!e || !ids.has(e.fromNode) || !ids.has(e.toNode)) continue;
      out.edges.push({ ...e, id: typeof e.id === "string" && e.id ? e.id : uid() });
    }
    return out;
  }

  /* ---------- view ---------- */
  function applyView() {
    world.style.transform = `translate(${view.x}px, ${view.y}px) scale(${view.k})`;
    zoomEl.textContent = Math.round(view.k * 100) + "%";
    stage.style.backgroundPosition = `${view.x}px ${view.y}px`;
    let g = 24 * view.k;
    while (g < 14) g *= 4; // keep the dot grid readable when zoomed out
    stage.style.backgroundSize = `${g}px ${g}px`;
  }
  const toWorld = (cx, cy) => { const r = stage.getBoundingClientRect(); return { x: (cx - r.left - view.x) / view.k, y: (cy - r.top - view.y) / view.k }; };
  function bbox(nodes = data.nodes) {
    if (!nodes.length) return null;
    return {
      x1: Math.min(...nodes.map((n) => n.x)), y1: Math.min(...nodes.map((n) => n.y)),
      x2: Math.max(...nodes.map((n) => n.x + n.width)), y2: Math.max(...nodes.map((n) => n.y + n.height)),
    };
  }
  function fit() {
    const b = bbox();
    const W = stage.clientWidth, H = stage.clientHeight;
    if (!b) { view = { x: W / 2, y: H / 2, k: 1 }; return applyView(); }
    const k = Math.max(0.1, Math.min(1.2, Math.min((W - 80) / (b.x2 - b.x1 || 1), (H - 80) / (b.y2 - b.y1 || 1))));
    view = { k, x: W / 2 - ((b.x1 + b.x2) / 2) * k, y: H / 2 - ((b.y1 + b.y2) / 2) * k };
    applyView();
  }
  function centerWorld() { const r = stage.getBoundingClientRect(); return toWorld(r.left + r.width / 2, r.top + r.height / 2); }

  /* ---------- rendering ---------- */
  function nodeInner(n) {
    if (n.type === "text") return `<div class="cv-text doc">${render(n.text || "", ctx.renderCtx).html || '<p class="cv-ph">Double-click to write</p>'}</div>`;
    if (n.type === "group") return `<div class="cv-glabel">${esc(n.label || "Group")}</div>`;
    if (n.type === "link") return `<div class="cv-link"><span>LINK</span>${esc(n.url || "")}</div>`;
    const k = ctx.kindOf(n.file || "");
    if (!k) return `<div class="cv-file"><div class="cv-ftitle missing">missing: ${esc(n.file || "")}</div></div>`;
    if (k === "img") return `<div class="cv-file img"><img data-img="${esc(n.file)}" alt="${esc(ctx.display(n.file))}"></div>`;
    return `<div class="cv-file"><div class="cv-ftitle">${esc(ctx.display(n.file))}<span>${esc(k === "locked" ? "locked" : k)}</span></div><div class="cv-fbody doc" data-file="${esc(n.file)}"></div></div>`;
  }
  function placeNode(el, n) {
    el.style.left = n.x + "px"; el.style.top = n.y + "px";
    el.style.width = n.width + "px"; el.style.height = n.height + "px";
  }
  function renderNodes() {
    const order = [...data.nodes.filter((n) => n.type === "group"), ...data.nodes.filter((n) => n.type !== "group")];
    layer.innerHTML = order.map((n) => {
      const col = colorOf(n.color);
      return `<div class="cv-node t-${n.type}${sel.has(n.id) ? " sel" : ""}" data-id="${esc(n.id)}" style="${col ? `--c:${col};` : ""}">
        ${nodeInner(n)}
        ${n.type === "group" ? "" : ["top", "right", "bottom", "left"].map((s) => `<i class="cv-h s-${s}" data-side="${s}"></i>`).join("")}
        <i class="cv-rz" title="Resize"></i></div>`;
    }).join("");
    layer.querySelectorAll(".cv-node").forEach((el) => placeNode(el, byId(el.dataset.id)));
    ctx.hydrate(layer);
    layer.querySelectorAll(".cv-fbody[data-file]").forEach(async (b) => {
      const p = b.dataset.file;
      try {
        const t = await ctx.readText(p);
        if (t == null) { b.innerHTML = '<p class="cv-ph">Locked. Unlock the vault to see it.</p>'; return; }
        const k = ctx.kindOf(p);
        b.innerHTML = k === "md" || k === "locked" ? render(t, ctx.renderCtx).html : k === "canvas" ? '<p class="cv-ph">Canvas</p>' : `<pre class="plaintext">${esc(t.slice(0, 4000))}</pre>`;
        ctx.hydrate(b);
      } catch { b.innerHTML = '<p class="cv-ph">Couldn\'t read this file.</p>'; }
    });
    host.querySelector(".cv-empty").hidden = data.nodes.length > 0;
  }
  function anchor(n, side) {
    switch (side) {
      case "top": return { x: n.x + n.width / 2, y: n.y };
      case "bottom": return { x: n.x + n.width / 2, y: n.y + n.height };
      case "left": return { x: n.x, y: n.y + n.height / 2 };
      default: return { x: n.x + n.width, y: n.y + n.height / 2 };
    }
  }
  function autoSides(a, b) {
    const dx = b.x + b.width / 2 - (a.x + a.width / 2), dy = b.y + b.height / 2 - (a.y + a.height / 2);
    if (Math.abs(dx) * 0.8 > Math.abs(dy)) return dx > 0 ? ["right", "left"] : ["left", "right"];
    return dy > 0 ? ["bottom", "top"] : ["top", "bottom"];
  }
  function edgeGeom(e) {
    const a = byId(e.fromNode), b = byId(e.toNode);
    if (!a || !b) return null;
    const [sa, sb] = autoSides(a, b);
    const s1 = e.fromSide || sa, s2 = e.toSide || sb;
    const p1 = anchor(a, s1), p2 = anchor(b, s2);
    const d = Math.max(40, Math.hypot(p2.x - p1.x, p2.y - p1.y) * 0.4);
    const c1 = { x: p1.x + DIRS[s1][0] * d, y: p1.y + DIRS[s1][1] * d };
    const c2 = { x: p2.x + DIRS[s2][0] * d, y: p2.y + DIRS[s2][1] * d };
    return { p1, p2, c1, c2 };
  }
  function bez(g, t) {
    const u = 1 - t;
    return {
      x: u * u * u * g.p1.x + 3 * u * u * t * g.c1.x + 3 * u * t * t * g.c2.x + t * t * t * g.p2.x,
      y: u * u * u * g.p1.y + 3 * u * u * t * g.c1.y + 3 * u * t * t * g.c2.y + t * t * t * g.p2.y,
    };
  }
  function arrowHead(at, from, size) {
    const dx = at.x - from.x, dy = at.y - from.y, len = Math.hypot(dx, dy) || 1, ux = dx / len, uy = dy / len;
    return `${at.x},${at.y} ${at.x - ux * size - uy * size * 0.55},${at.y - uy * size + ux * size * 0.55} ${at.x - ux * size + uy * size * 0.55},${at.y - uy * size - ux * size * 0.55}`;
  }
  function renderEdges(temp) {
    let out = "";
    for (const e of data.edges) {
      const g = edgeGeom(e);
      if (!g) continue;
      const col = colorOf(e.color) || "#5a6a9a", on = sel.has(e.id);
      const stroke = on ? "#00e5ff" : col;
      const path = `M${g.p1.x},${g.p1.y} C${g.c1.x},${g.c1.y} ${g.c2.x},${g.c2.y} ${g.p2.x},${g.p2.y}`;
      out += `<g class="cv-edge${on ? " sel" : ""}" data-edge="${esc(e.id)}"><path d="${path}" class="hit"/><path d="${path}" stroke="${stroke}" class="line"/>`;
      if ((e.toEnd || "arrow") === "arrow") out += `<polygon points="${arrowHead(g.p2, bez(g, 0.96), 11)}" fill="${stroke}"/>`;
      if (e.fromEnd === "arrow") out += `<polygon points="${arrowHead(g.p1, bez(g, 0.04), 11)}" fill="${stroke}"/>`;
      if (e.label) { const m = bez(g, 0.5); out += `<text x="${m.x}" y="${m.y}" class="cv-elabel">${esc(e.label)}</text>`; }
      out += "</g>";
    }
    if (temp) out += `<path d="M${temp.p1.x},${temp.p1.y} L${temp.p2.x},${temp.p2.y}" class="cv-temp"/>`;
    svg.innerHTML = out;
  }
  function renderAll() { renderNodes(); renderEdges(); }

  /* ---------- adding things ---------- */
  function addNode(n, edit) {
    const before = snapshot();
    const node = { id: uid(), x: 0, y: 0, width: 250, height: 120, ...n };
    data.nodes.push(node);
    sel = new Set([node.id]);
    renderAll();
    commit(before);
    if (edit) startEdit(node.id);
    return node;
  }
  function addTextAt(p, edit = true) { return addNode({ type: "text", text: "", x: Math.round(p.x - 125), y: Math.round(p.y - 30), width: 250, height: 120 }, edit); }
  async function addFile(kindFilter, at) {
    const p = await ctx.pickFile(kindFilter === "img" ? "Add an image" : "Add a note", (path) => (kindFilter === "img" ? ctx.kindOf(path) === "img" : ctx.kindOf(path) !== "img"));
    if (!p) return;
    const c = at || centerWorld();
    const img = ctx.kindOf(p) === "img";
    addNode({ type: "file", file: p, x: Math.round(c.x - (img ? 150 : 200)), y: Math.round(c.y - (img ? 110 : 150)), width: img ? 300 : 400, height: img ? 220 : 300 });
  }
  function addGroup(at) {
    const c = at || centerWorld();
    const chosen = data.nodes.filter((n) => sel.has(n.id) && n.type !== "group");
    const b = chosen.length ? bbox(chosen) : null;
    const g = b
      ? { type: "group", label: "Group", x: b.x1 - 40, y: b.y1 - 60, width: b.x2 - b.x1 + 80, height: b.y2 - b.y1 + 100 }
      : { type: "group", label: "Group", x: Math.round(c.x - 250), y: Math.round(c.y - 180), width: 500, height: 360 };
    const n = addNode(g);
    renameGroup(n.id);
  }
  async function renameGroup(id) {
    const n = byId(id);
    const v = await ctx.askLine({ title: "Group name", label: "Name", value: n.label || "", ok: "Save" });
    if (v === null) return;
    mutate(() => (n.label = v));
  }

  /* ---------- editing text cards ---------- */
  function startEdit(id) {
    const n = byId(id);
    if (!n || n.type !== "text") return;
    stopEdit();
    const el = layer.querySelector(`.cv-node[data-id="${CSS.escape(id)}"]`);
    if (!el) return;
    editing = { id, before: snapshot() };
    const box = el.querySelector(".cv-text");
    box.innerHTML = "";
    const ta = document.createElement("textarea");
    ta.className = "cv-ta";
    ta.value = n.text || "";
    ta.spellcheck = false;
    box.appendChild(ta);
    el.classList.add("editing");
    ta.addEventListener("keydown", (e) => {
      e.stopPropagation();
      if (e.key === "Escape") { e.preventDefault(); stopEdit(); stage.focus(); }
    });
    ta.addEventListener("pointerdown", (e) => e.stopPropagation());
    ta.addEventListener("blur", () => setTimeout(stopEdit, 0));
    setTimeout(() => { ta.focus(); ta.setSelectionRange(ta.value.length, ta.value.length); }, 0);
  }
  function stopEdit() {
    if (!editing) return;
    const { id, before } = editing;
    editing = null;
    const el = layer.querySelector(`.cv-node[data-id="${CSS.escape(id)}"]`);
    const ta = el && el.querySelector("textarea");
    const n = byId(id);
    if (n && ta) n.text = ta.value;
    renderAll();
    commit(before);
  }

  /* ---------- deleting, colours, order ---------- */
  function deleteSelected() {
    if (!sel.size) return;
    mutate(() => {
      data.nodes = data.nodes.filter((n) => !sel.has(n.id));
      const live = new Set(data.nodes.map((n) => n.id));
      data.edges = data.edges.filter((e) => !sel.has(e.id) && live.has(e.fromNode) && live.has(e.toNode));
      sel.clear();
    });
  }
  function colorItems(targets) {
    return [...Object.keys(COLORS).map((k) => ({ label: "Color: " + COLOR_NAMES[k], run: () => mutate(() => targets.forEach((t) => (t.color = k))) })),
      { label: "Color: none", run: () => mutate(() => targets.forEach((t) => delete t.color)) }];
  }

  /* ---------- imports ---------- */
  function placeBesides(nodes) {
    const b = bbox(), nb = bbox(nodes);
    if (!nb) return;
    const dx = b ? b.x2 + 200 - nb.x1 : -nb.x1, dy = b ? b.y1 - nb.y1 : -nb.y1;
    nodes.forEach((n) => { n.x = Math.round(n.x + dx); n.y = Math.round(n.y + dy); });
  }
  function merge(incoming, label) {
    const inc = sanitize(incoming);
    if (!inc.nodes.length) return ctx.toast("Nothing to import there.", true);
    const remap = new Map(inc.nodes.map((n) => [n.id, uid()]));
    inc.nodes.forEach((n) => (n.id = remap.get(n.id)));
    inc.edges.forEach((e) => { e.id = uid(); e.fromNode = remap.get(e.fromNode); e.toNode = remap.get(e.toNode); });
    placeBesides(inc.nodes);
    mutate(() => { data.nodes.push(...inc.nodes); data.edges.push(...inc.edges); sel = new Set(inc.nodes.map((n) => n.id)); });
    fit();
    ctx.toast(`Imported ${inc.nodes.length} card${inc.nodes.length === 1 ? "" : "s"} and ${inc.edges.length} connection${inc.edges.length === 1 ? "" : "s"} from ${label}`);
  }
  async function importGraph() {
    const g = await ctx.graphSnapshot();
    if (!g.nodes.length) return ctx.toast("The graph is empty.", true);
    const S = 2.4;
    const nodes = g.nodes.map((n) => {
      if (n.ghost || n.kind === "tag") return { id: n.id, type: "text", text: n.kind === "tag" ? n.label : `[[${n.label}]]`, x: n.x * S - 90, y: n.y * S - 30, width: 180, height: 60, color: n.kind === "tag" ? "1" : "2" };
      const img = n.kind === "img";
      return { id: n.id, type: "file", file: n.id, x: n.x * S - (img ? 100 : 130), y: n.y * S - (img ? 80 : 60), width: img ? 200 : 260, height: img ? 160 : 120 };
    });
    const edges = g.links.map((l) => ({ id: uid(), fromNode: l.source, toNode: l.target, toEnd: "arrow" }));
    merge({ nodes, edges }, "the graph");
  }
  async function importCanvasFile() {
    const p = await ctx.pickFile("Insert a canvas", (path) => ctx.kindOf(path) === "canvas");
    if (!p) return;
    try { merge(JSON.parse(await ctx.readText(p)), ctx.display(p)); }
    catch { ctx.toast("That canvas file couldn't be read.", true); }
  }
  async function importMermaid() {
    const src = await ctx.askText({
      title: "Import a mermaid flowchart",
      label: "Paste the flowchart",
      placeholder: "flowchart LR\n  ISP[ISP router] --> FW{OPNsense}\n  FW -->|LAN| SW[Switch]\n  SW --> PVE[Proxmox]\n  SW --> PI[Pi 4]",
      ok: "Import",
    });
    if (!src) return;
    const r = parseMermaid(src);
    if (!r.nodes.length) return ctx.toast("No flowchart nodes found. Start with a line like: A --> B", true);
    merge(r, "the flowchart");
  }
  async function importCSV() {
    const src = await ctx.askText({
      title: "Import a list of links",
      label: "One connection per line: from, to, optional label",
      placeholder: "source,target,label\nfirewall,switch,trunk\nswitch,proxmox,10G\nswitch,pi4\nproxmox,Null Search",
      ok: "Import",
    });
    if (!src) return;
    const r = parseEdgeList(src, ctx);
    if (!r.nodes.length) return ctx.toast("No connections found. Use lines like: switch,proxmox", true);
    merge(r, "the list");
  }
  function importMenu(x, y) {
    ctx.menu(x, y, [
      { label: "Graph view (with your pinned layout)", run: importGraph },
      { label: "Another canvas file…", run: importCanvasFile },
      { label: "Mermaid flowchart…", run: importMermaid },
      { label: "CSV list of links…", run: importCSV },
    ], "Import into this canvas");
  }

  /* ---------- toolbar ---------- */
  host.querySelector(".cv-bar").addEventListener("click", (e) => {
    const b = e.target.closest("[data-act]");
    if (!b) return;
    const act = b.dataset.act;
    if (act === "card") addTextAt(centerWorld());
    else if (act === "note") addFile("note");
    else if (act === "image") addFile("img");
    else if (act === "group") addGroup();
    else if (act === "import") { const r = b.getBoundingClientRect(); importMenu(r.left, r.bottom + 4); }
    else if (act === "undo") doUndo();
    else if (act === "redo") doRedo();
    else if (act === "fit") fit();
  });
  function doUndo() { stopEdit(); if (!undo.length) return; redo.push(snapshot()); data = JSON.parse(undo.pop()); sel.clear(); renderAll(); commit(); }
  function doRedo() { stopEdit(); if (!redo.length) return; undo.push(snapshot()); data = JSON.parse(redo.pop()); sel.clear(); renderAll(); commit(); }

  /* ---------- pointer interaction ---------- */
  const nodeEl = (t) => t.closest && t.closest(".cv-node");
  stage.addEventListener("pointerdown", (e) => {
    if (e.button !== 0 || e.target.closest("textarea")) return;
    stage.focus({ preventScroll: true });
    const w = toWorld(e.clientX, e.clientY);
    const handle = e.target.closest(".cv-h"), rz = e.target.closest(".cv-rz"), nel = nodeEl(e.target), edg = e.target.closest("[data-edge]");
    if (editing && (!nel || nel.dataset.id !== editing.id)) stopEdit();
    stage.setPointerCapture(e.pointerId);
    if (handle) {
      const n = byId(nel.dataset.id);
      op = { type: "connect", from: n, side: handle.dataset.side };
      return;
    }
    if (rz) {
      const n = byId(nel.dataset.id);
      op = { type: "resize", n, sx: w.x, sy: w.y, w: n.width, h: n.height, before: snapshot() };
      return;
    }
    if (nel) {
      const id = nel.dataset.id;
      if (e.shiftKey) { sel.has(id) ? sel.delete(id) : sel.add(id); }
      else if (!sel.has(id)) sel = new Set([id]);
      const moving = new Set([...sel].filter((s) => byId(s)));
      // Dragging a group carries everything inside it.
      [...moving].forEach((s) => {
        const g = byId(s);
        if (g.type !== "group") return;
        data.nodes.forEach((n) => { if (n !== g && n.x >= g.x && n.y >= g.y && n.x + n.width <= g.x + g.width && n.y + n.height <= g.y + g.height) moving.add(n.id); });
      });
      op = { type: "move", sx: w.x, sy: w.y, start: [...moving].map((s) => { const n = byId(s); return [n, n.x, n.y]; }), moved: false, before: snapshot() };
      renderSelection();
      return;
    }
    if (edg) {
      const id = edg.dataset.edge;
      if (e.shiftKey) sel.has(id) ? sel.delete(id) : sel.add(id); else sel = new Set([id]);
      renderSelection();
      op = null;
      return;
    }
    if (e.shiftKey) { op = { type: "marquee", sx: e.clientX, sy: e.clientY }; return; }
    op = { type: "pan", sx: e.clientX, sy: e.clientY, vx: view.x, vy: view.y, moved: false };
  });
  stage.addEventListener("pointermove", (e) => {
    if (!op) return;
    const w = toWorld(e.clientX, e.clientY);
    if (op.type === "pan") {
      if (Math.abs(e.clientX - op.sx) + Math.abs(e.clientY - op.sy) > 3) op.moved = true;
      view.x = op.vx + e.clientX - op.sx; view.y = op.vy + e.clientY - op.sy; applyView();
    } else if (op.type === "move") {
      const dx = w.x - op.sx, dy = w.y - op.sy;
      if (!op.moved && Math.abs(dx) + Math.abs(dy) < 2 / view.k) return;
      op.moved = true;
      for (const [n, x, y] of op.start) {
        n.x = Math.round(x + dx); n.y = Math.round(y + dy);
        const el = layer.querySelector(`.cv-node[data-id="${CSS.escape(n.id)}"]`);
        if (el) placeNode(el, n);
      }
      renderEdges();
    } else if (op.type === "resize") {
      op.n.width = Math.max(80, Math.round(op.w + w.x - op.sx));
      op.n.height = Math.max(40, Math.round(op.h + w.y - op.sy));
      const el = layer.querySelector(`.cv-node[data-id="${CSS.escape(op.n.id)}"]`);
      if (el) placeNode(el, op.n);
      renderEdges();
    } else if (op.type === "connect") {
      renderEdges({ p1: anchor(op.from, op.side), p2: w });
      layer.querySelectorAll(".cv-node.target").forEach((n) => n.classList.remove("target"));
      const t = nodeEl(document.elementFromPoint(e.clientX, e.clientY) || stage);
      if (t && t.dataset.id !== op.from.id) t.classList.add("target");
    } else if (op.type === "marquee") {
      const r = stage.getBoundingClientRect();
      const x1 = Math.min(op.sx, e.clientX) - r.left, y1 = Math.min(op.sy, e.clientY) - r.top;
      Object.assign(marquee.style, { left: x1 + "px", top: y1 + "px", width: Math.abs(e.clientX - op.sx) + "px", height: Math.abs(e.clientY - op.sy) + "px" });
      marquee.hidden = false;
    }
  });
  stage.addEventListener("pointerup", (e) => {
    const o = op;
    op = null;
    if (!o) return;
    if (o.type === "pan" && !o.moved) { if (sel.size) { sel.clear(); renderSelection(); } }
    else if (o.type === "move") { if (o.moved) commit(o.before); }
    else if (o.type === "resize") { renderAll(); commit(o.before); }
    else if (o.type === "connect") {
      layer.querySelectorAll(".cv-node.target").forEach((n) => n.classList.remove("target"));
      const t = nodeEl(document.elementFromPoint(e.clientX, e.clientY) || stage);
      if (t && t.dataset.id !== o.from.id) {
        const to = byId(t.dataset.id), w = toWorld(e.clientX, e.clientY);
        const side = ["top", "right", "bottom", "left"].map((s) => [s, Math.hypot(anchor(to, s).x - w.x, anchor(to, s).y - w.y)]).sort((a, b) => a[1] - b[1])[0][0];
        mutate(() => data.edges.push({ id: uid(), fromNode: o.from.id, fromSide: o.side, toNode: to.id, toSide: side, toEnd: "arrow" }));
      } else if (!t) {
        // Dropped on empty board: make a new card there and connect it, like Obsidian.
        const w = toWorld(e.clientX, e.clientY);
        const before = snapshot();
        const n = { id: uid(), type: "text", text: "", x: Math.round(w.x - 125), y: Math.round(w.y - 40), width: 250, height: 120 };
        data.nodes.push(n);
        data.edges.push({ id: uid(), fromNode: o.from.id, fromSide: o.side, toNode: n.id, toEnd: "arrow" });
        sel = new Set([n.id]);
        renderAll(); commit(before); startEdit(n.id);
      } else renderEdges();
    } else if (o.type === "marquee") {
      marquee.hidden = true;
      const a = toWorld(o.sx, o.sy), b = toWorld(e.clientX, e.clientY);
      const x1 = Math.min(a.x, b.x), x2 = Math.max(a.x, b.x), y1 = Math.min(a.y, b.y), y2 = Math.max(a.y, b.y);
      data.nodes.forEach((n) => { if (n.x >= x1 && n.y >= y1 && n.x + n.width <= x2 && n.y + n.height <= y2) sel.add(n.id); });
      renderSelection();
    }
  });
  function renderSelection() {
    layer.querySelectorAll(".cv-node").forEach((el) => el.classList.toggle("sel", sel.has(el.dataset.id)));
    renderEdges();
  }
  stage.addEventListener("dblclick", (e) => {
    if (e.target.closest("textarea")) return;
    const nel = nodeEl(e.target), edg = e.target.closest("[data-edge]");
    if (nel) {
      const n = byId(nel.dataset.id);
      if (n.type === "text") startEdit(n.id);
      else if (n.type === "group") renameGroup(n.id);
      else if (n.type === "file" && ctx.kindOf(n.file)) ctx.open(n.file);
      return;
    }
    if (edg) return editEdgeLabel(edg.dataset.edge);
    addTextAt(toWorld(e.clientX, e.clientY));
  });
  async function editEdgeLabel(id) {
    const ed = edgeById(id);
    const v = await ctx.askLine({ title: "Connection label", label: "Label (leave empty for none)", value: ed.label || "", ok: "Save", allowEmpty: true });
    if (v === null) return;
    mutate(() => { if (v) ed.label = v; else delete ed.label; });
  }
  stage.addEventListener("wheel", (e) => {
    e.preventDefault();
    if (e.ctrlKey || !e.shiftKey) {
      const r = stage.getBoundingClientRect(), px = e.clientX - r.left, py = e.clientY - r.top;
      const k = Math.max(0.1, Math.min(3, view.k * Math.exp(-e.deltaY * 0.0015)));
      view.x = px - ((px - view.x) / view.k) * k; view.y = py - ((py - view.y) / view.k) * k; view.k = k;
    } else { view.x -= e.deltaY; }
    applyView();
  }, { passive: false });
  stage.addEventListener("contextmenu", (e) => {
    e.preventDefault();
    if (e.target.closest("textarea")) return;
    const nel = nodeEl(e.target), edg = e.target.closest("[data-edge]");
    const w = toWorld(e.clientX, e.clientY);
    if (nel) {
      const id = nel.dataset.id;
      if (!sel.has(id)) { sel = new Set([id]); renderSelection(); }
      const n = byId(id), targets = data.nodes.filter((x) => sel.has(x.id));
      const items = [];
      if (n.type === "text") items.push({ label: "Edit", run: () => startEdit(id) });
      if (n.type === "group") items.push({ label: "Rename group", run: () => renameGroup(id) });
      if (n.type === "file" && ctx.kindOf(n.file)) items.push({ label: "Open file", run: () => ctx.open(n.file) });
      if (n.type !== "group") items.push({ label: "Bring to front", run: () => mutate(() => { data.nodes = [...data.nodes.filter((x) => !sel.has(x.id)), ...targets]; }) });
      if (targets.filter((t) => t.type !== "group").length > 1) items.push({ label: "Group selected cards", run: () => addGroup() });
      items.push("-", ...colorItems(targets), "-", { label: sel.size > 1 ? `Delete ${sel.size} items` : "Delete", key: "Del", danger: true, run: deleteSelected });
      return ctx.menu(e.clientX, e.clientY, items, n.type === "file" ? ctx.display(n.file) : n.type === "group" ? n.label || "Group" : "Card");
    }
    if (edg) {
      const ed = edgeById(edg.dataset.edge);
      sel = new Set([ed.id]); renderSelection();
      return ctx.menu(e.clientX, e.clientY, [
        { label: "Edit label", run: () => editEdgeLabel(ed.id) },
        { label: "Reverse direction", run: () => mutate(() => { [ed.fromNode, ed.toNode] = [ed.toNode, ed.fromNode]; [ed.fromSide, ed.toSide] = [ed.toSide, ed.fromSide]; }) },
        { label: (ed.toEnd || "arrow") === "arrow" ? "Hide arrow" : "Show arrow", run: () => mutate(() => (ed.toEnd = (ed.toEnd || "arrow") === "arrow" ? "none" : "arrow")) },
        "-", ...colorItems([ed]), "-",
        { label: "Delete connection", danger: true, run: deleteSelected },
      ], "Connection");
    }
    ctx.menu(e.clientX, e.clientY, [
      { label: "Add card here", run: () => addTextAt(w) },
      { label: "Add note here…", run: () => addFile("note", w) },
      { label: "Add image here…", run: () => addFile("img", w) },
      { label: "Add group here", run: () => addGroup(w) },
      "-",
      { label: "Import…", run: () => importMenu(e.clientX, e.clientY) },
      { label: "Fit to screen", run: fit },
    ], "Canvas");
  });
  stage.addEventListener("keydown", (e) => {
    if (e.target.closest("textarea, input")) return;
    const ctrl = e.ctrlKey || e.metaKey, k = e.key.toLowerCase();
    if (e.key === "Delete" || e.key === "Backspace") { e.preventDefault(); e.stopPropagation(); deleteSelected(); }
    else if (ctrl && k === "z" && !e.shiftKey) { e.preventDefault(); e.stopPropagation(); doUndo(); }
    else if (ctrl && (k === "y" || (k === "z" && e.shiftKey))) { e.preventDefault(); e.stopPropagation(); doRedo(); }
    else if (ctrl && k === "a") { e.preventDefault(); sel = new Set(data.nodes.map((n) => n.id)); renderSelection(); }
    else if (e.key === "Escape") { if (sel.size) { e.stopPropagation(); sel.clear(); renderSelection(); } }
    else if (e.key === "Enter" && sel.size === 1) { const n = byId([...sel][0]); if (n && n.type === "text") { e.preventDefault(); startEdit(n.id); } }
  });
  stage.addEventListener("paste", (e) => {
    if (e.target.closest("textarea, input")) return;
    const t = e.clipboardData && e.clipboardData.getData("text/plain");
    if (!t) return;
    e.preventDefault();
    const c = centerWorld();
    if (/^https?:\/\/\S+$/.test(t.trim())) addNode({ type: "link", url: t.trim(), x: Math.round(c.x - 150), y: Math.round(c.y - 40), width: 300, height: 80 });
    else addNode({ type: "text", text: t, x: Math.round(c.x - 150), y: Math.round(c.y - 80), width: 300, height: 160 });
  });
  const ro = new ResizeObserver(() => applyView());
  ro.observe(stage);

  return {
    load(text) {
      stopEdit();
      let parsed = { nodes: [], edges: [] };
      try { if (text && text.trim()) parsed = JSON.parse(text); } catch { ctx.toast("This canvas file isn't valid JSON, so it opened empty. Nothing is saved until you change something.", true); }
      data = sanitize(parsed);
      undo = []; redo = []; sel.clear();
      renderAll();
      requestAnimationFrame(fit);
    },
    refresh() { if (!editing && !op) renderAll(); },
    destroy() { stopEdit(); ro.disconnect(); host.innerHTML = ""; },
    fit,
  };
}

/* ---------- parsers for imports ---------- */

export function parseMermaid(src) {
  const lines = src.replace(/\r/g, "").split(/\n|;/).map((l) => l.trim()).filter(Boolean);
  let dir = "TD";
  const nodes = new Map(), edges = [];
  const shapeColor = { "{": "2", "((": "5", "([": "4", "[(": "6" };
  const node = (tok) => {
    tok = tok.trim();
    const m = tok.match(/^([A-Za-z0-9_\-.]+)\s*(\[\[|\[\(|\(\[|\(\(|\{\{|\[|\(|\{|>)?\s*(.*?)\s*(\]\]|\)\]|\]\)|\)\)|\}\}|\]|\)|\})?\s*$/);
    if (!m) return null;
    const [, id, open, label] = m;
    const clean = (label || "").replace(/^["']|["']$/g, "").replace(/<br\s*\/?>/gi, "\n");
    if (!nodes.has(id)) nodes.set(id, { id, label: clean || id, color: shapeColor[open] || (open === "{{" ? "3" : undefined) });
    else if (clean) nodes.get(id).label = clean;
    return id;
  };
  for (let line of lines) {
    if (/^%%/.test(line) || /^(subgraph|end|classDef|class|style|click|linkStyle|direction)\b/.test(line)) continue;
    const h = line.match(/^(graph|flowchart)\s*(TD|TB|BT|LR|RL)?/i);
    if (h) { dir = (h[2] || "TD").toUpperCase(); continue; }
    line = line.replace(/--\s+([^->]+?)\s+-->/g, "-->|$1|").replace(/==\s+([^=>]+?)\s+==>/g, "==>|$1|");
    const parts = line.split(/\s*(<-->|-->|---|-\.->|-\.-|==>|===|--x|--o)\s*/);
    if (parts.length === 1) { parts[0].split(/\s*&\s*/).forEach(node); continue; }
    let prev = null;
    for (let i = 0; i < parts.length; i += 2) {
      let tok = parts[i], label = null;
      const lm = tok.match(/^\|([^|]*)\|\s*(.*)$/);
      if (lm) { label = lm[1].trim(); tok = lm[2]; }
      const ids = tok.split(/\s*&\s*/).map(node).filter(Boolean);
      if (prev && ids.length) {
        const arrow = parts[i - 1];
        for (const a of prev) for (const b of ids) edges.push({ from: a, to: b, label, arrow: !/^(---|-\.-|===)$/.test(arrow), both: arrow === "<-->" });
      }
      if (ids.length) prev = ids;
    }
  }
  // Layered layout: each node sits one level below its deepest parent.
  const level = new Map([...nodes.keys()].map((k) => [k, 0]));
  for (let pass = 0; pass < nodes.size; pass++) {
    let changed = false;
    for (const e of edges) if (e.from !== e.to && level.get(e.to) < level.get(e.from) + 1 && level.get(e.from) + 1 < nodes.size) { level.set(e.to, level.get(e.from) + 1); changed = true; }
    if (!changed) break;
  }
  const rows = new Map();
  for (const [id, lv] of level) { if (!rows.has(lv)) rows.set(lv, []); rows.get(lv).push(id); }
  const horiz = dir === "LR" || dir === "RL", flip = dir === "BT" || dir === "RL";
  const maxLv = Math.max(0, ...level.values());
  const out = { nodes: [], edges: [] };
  for (const [lv, ids] of rows) ids.forEach((id, i) => {
    const n = nodes.get(id), L = flip ? maxLv - lv : lv, off = i - (ids.length - 1) / 2;
    out.nodes.push({ id, type: "text", text: n.label, color: n.color, width: 200, height: 70, x: horiz ? L * 320 : off * 260, y: horiz ? off * 130 : L * 170 });
  });
  edges.forEach((e, i) => out.edges.push({ id: "e" + i, fromNode: e.from, toNode: e.to, toEnd: e.arrow ? "arrow" : "none", fromEnd: e.both ? "arrow" : undefined, label: e.label || undefined }));
  return out;
}

export function parseEdgeList(src, ctx) {
  const rows = src.replace(/\r/g, "").split("\n").map((l) => l.trim()).filter((l) => l && !l.startsWith("#"));
  if (rows.length && /^(source|from)\s*[,\t;]\s*(target|to)/i.test(rows[0])) rows.shift();
  const names = new Map(), edges = [];
  const id = (name) => { name = name.trim().replace(/^["']|["']$/g, ""); if (!name) return null; if (!names.has(name)) names.set(name, "n" + names.size); return names.get(name); };
  for (const r of rows) {
    const parts = r.includes("->") ? r.split(/\s*->\s*/) : r.split(/\s*[,\t;]\s*/);
    if (parts.length < 2) { id(parts[0]); continue; }
    const a = id(parts[0]), b = id(parts[1]);
    if (a && b) edges.push({ fromNode: a, toNode: b, label: parts[2] ? parts.slice(2).join(", ").trim() : undefined });
  }
  const nodes = [...names].map(([name, nid]) => {
    const file = ctx && ctx.resolveName ? ctx.resolveName(name) : null;
    return file ? { id: nid, type: "file", file, width: 240, height: 110 } : { id: nid, type: "text", text: name, width: 180, height: 60 };
  });
  // Lay it out with the same force engine as the graph view.
  const d3 = window.d3;
  const sim = nodes.map((n) => ({ id: n.id }));
  if (d3 && sim.length) {
    const s = d3.forceSimulation(sim)
      .force("link", d3.forceLink(edges.map((e) => ({ source: e.fromNode, target: e.toNode }))).id((d) => d.id).distance(240))
      .force("charge", d3.forceManyBody().strength(-900))
      .force("collide", d3.forceCollide(150))
      .force("x", d3.forceX(0).strength(0.05)).force("y", d3.forceY(0).strength(0.05))
      .stop();
    for (let i = 0; i < 300; i++) s.tick();
  }
  nodes.forEach((n, i) => { n.x = Math.round((sim[i].x || i * 220) - n.width / 2); n.y = Math.round((sim[i].y || 0) - n.height / 2); });
  return { nodes, edges: edges.map((e, i) => ({ id: "e" + i, ...e, toEnd: "arrow" })) };
}
