// Graph view: d3-force physics (the same engine Obsidian's graph uses), with
// pinning and an Obsidian-style settings panel: Filters, Groups, Display,
// Forces and Pinning.
//
// Everything (pins, settings, groups) is saved inside the vault in
// .nullnotes/graph.json, so the layout travels with the vault folder.

const d3 = window.d3;
const KCOL = { md: "#00e5ff", txt: "#8a92b4", html: "#ffb000", img: "#a58bff", canvas: "#39ff88", locked: "#f5f06b", tag: "#ff2e88", ghost: "#ffb000" };
const GROUP_COLORS = ["#39ff88", "#ff2e88", "#ffb000", "#a58bff", "#00e5ff", "#ff6b3d", "#f5f06b"];

const DEFAULTS = {
  filters: { search: "", tags: false, attachments: true, existingOnly: false, orphans: true },
  groups: [],
  display: { arrows: false, textFade: 0, nodeSize: 1, linkWidth: 1, glow: true },
  forces: { center: 0.5, repel: 10, linkStrength: 1, linkDistance: 120 },
  pinOnDrop: false,
  open: { filters: true, groups: false, display: false, forces: true, pinning: true },
};
const clone = (o) => JSON.parse(JSON.stringify(o));
const esc = (s) => String(s).replace(/[&<>"]/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;" })[c]);

export function createGraph(ctx) {
  // ctx: { canvas, panel, files(), linksOf(p), tagsOf(p), resolve(l), display(p), kindOf(p), current(),
  //        onOpen(p), onCreate(label), menu(x, y, items, header), loadMeta(), saveMeta(json) }
  const cv = ctx.canvas, panel = ctx.panel;
  const c = cv.getContext("2d");
  let nodes = [], links = [], byId = new Map(), neighbors = new Map();
  let sim = null, open = false, frame = 0;
  let view = { k: 1, x: 0, y: 0 };
  let hover = null, drag = null, pan = null;
  let S = clone(DEFAULTS);
  let pins = {};
  const remembered = new Map();
  let saveTimer, pendingFit = false, loaded = false;

  /* ---------- persistence ---------- */
  async function load() {
    try {
      const raw = await ctx.loadMeta();
      const m = raw ? JSON.parse(raw) : {};
      S = clone(DEFAULTS);
      for (const k of ["filters", "display", "forces", "open"]) Object.assign(S[k], m[k] || {});
      if (m.forces && "pinOnDrop" in m.forces) delete S.forces.pinOnDrop; // from 0.1
      S.groups = Array.isArray(m.groups) ? m.groups.filter((g) => g && typeof g.query === "string") : [];
      S.pinOnDrop = !!m.pinOnDrop;
      pins = m.pins && typeof m.pins === "object" ? m.pins : {};
    } catch { S = clone(DEFAULTS); pins = {}; }
    loaded = true;
  }
  function save() {
    clearTimeout(saveTimer);
    saveTimer = setTimeout(() => {
      ctx.saveMeta(JSON.stringify({ version: 2, pins, ...S }, null, 1)).catch(() => {});
    }, 400);
  }

  /* ---------- groups ---------- */
  function matches(n, q) {
    q = q.trim().toLowerCase();
    if (!q) return false;
    if (q.startsWith("tag:")) {
      const t = q.slice(4).replace(/^#/, "");
      return n.kind === "tag" ? n.label.slice(1).toLowerCase() === t : !n.ghost && ctx.tagsOf(n.id).includes(t);
    }
    if (q.startsWith("path:")) { const p = q.slice(5).replace(/^\/+/, ""); return !n.ghost && n.kind !== "tag" && n.id.toLowerCase().startsWith(p); }
    if (q.startsWith("file:")) return n.label.toLowerCase().includes(q.slice(5));
    return n.label.toLowerCase().includes(q) || (!n.ghost && n.kind !== "tag" && n.id.toLowerCase().includes(q));
  }
  function colorOf(n) {
    for (const g of S.groups) if (matches(n, g.query)) return g.color;
    return KCOL[n.kind] || "#8a92b4";
  }

  /* ---------- data ---------- */
  function startPos(id, i, total) {
    const pin = pins[id], prev = remembered.get(id) || (byId.get(id) && { x: byId.get(id).x, y: byId.get(id).y });
    if (pin) return { x: pin.x, y: pin.y };
    if (prev) return prev;
    const a = (i / Math.max(1, total)) * Math.PI * 2, rad = 60 + Math.sqrt(total) * 40;
    return { x: Math.cos(a) * rad + Math.random() * 10, y: Math.sin(a) * rad + Math.random() * 10 };
  }
  function build() {
    const f = S.filters, q = f.search.trim().toLowerCase();
    const old = byId;
    let files = ctx.files().filter((p) => (f.attachments || ctx.kindOf(p) !== "img"));
    if (q) files = files.filter((p) => p.toLowerCase().includes(q) || ctx.display(p).toLowerCase().includes(q));
    const keep = new Set(files);
    const N = [], L = [], ids = new Map(), nb = new Map();
    const add = (n) => { const prev = old.get(n.id); if (prev) { n.x = prev.x; n.y = prev.y; n.vx = prev.vx; n.vy = prev.vy; } N.push(n); ids.set(n.id, n); nb.set(n.id, new Set()); return n; };
    const pinIt = (n) => { const pin = pins[n.id]; if (pin) { n.fx = pin.x; n.fy = pin.y; n.pinned = true; } return n; };
    files.forEach((p, i) => pinIt(add({ id: p, label: ctx.display(p), kind: ctx.kindOf(p), ghost: false, ...startPos(p, i, files.length) })));
    const seen = new Set();
    const link = (a, b) => {
      const key = a < b ? a + "\u0000" + b : b + "\u0000" + a;
      if (a === b || seen.has(key)) return;
      seen.add(key); L.push({ source: a, target: b }); nb.get(a).add(b); nb.get(b).add(a);
    };
    files.forEach((p) => ctx.linksOf(p).forEach((l) => {
      let t = ctx.resolve(l);
      if (t && !keep.has(t)) return; // linked file is filtered out
      if (!t) {
        if (f.existingOnly || q) return;
        t = "?" + l.toLowerCase();
        if (!ids.has(t)) { const s = ids.get(p); pinIt(add({ id: t, label: l, kind: "ghost", ghost: true, ...(pins[t] || remembered.get(t) || { x: s.x + Math.random() * 40 - 20, y: s.y + Math.random() * 40 - 20 }) })); }
      }
      link(p, t);
    }));
    if (f.tags) files.forEach((p) => ctx.tagsOf(p).forEach((t) => {
      const id = "#" + t;
      if (!ids.has(id)) { const s = ids.get(p); pinIt(add({ id, label: "#" + t, kind: "tag", ghost: false, ...(pins[id] || remembered.get(id) || { x: s.x + Math.random() * 60 - 30, y: s.y + Math.random() * 60 - 30 }) })); }
      link(p, id);
    }));
    let finalNodes = N;
    if (!f.orphans) finalNodes = N.filter((n) => nb.get(n.id).size > 0);
    nodes = finalNodes; links = L; byId = new Map(finalNodes.map((n) => [n.id, n])); neighbors = nb;
    nodes.forEach((n) => (n.r = (4 + Math.min(10, Math.sqrt(neighbors.get(n.id).size) * 2.4)) * (n.kind === "tag" ? 0.8 : 1)));
  }

  function applyForces() {
    if (!sim) return;
    const F = S.forces;
    sim.force("link").distance(F.linkDistance).strength(F.linkStrength * 0.7);
    sim.force("charge").strength(-F.repel * 30);
    sim.force("x").strength(F.center * 0.08);
    sim.force("y").strength(F.center * 0.08);
    sim.force("collide").radius((d) => d.r * S.display.nodeSize + 6);
    sim.alpha(Math.max(sim.alpha(), 0.5)).restart();
  }
  function startSim(alpha) {
    if (sim) sim.stop();
    sim = d3.forceSimulation(nodes)
      .force("link", d3.forceLink(links).id((d) => d.id))
      .force("charge", d3.forceManyBody().distanceMax(900).theta(0.9))
      .force("x", d3.forceX(0))
      .force("y", d3.forceY(0))
      .force("collide", d3.forceCollide().strength(0.7))
      .velocityDecay(0.4)
      .alphaDecay(0.02)
      .on("tick", () => {
        if (pendingFit && sim.alpha() < 0.12) { pendingFit = false; fit(); }
        requestDraw();
      });
    applyForces();
    sim.alpha(alpha).restart();
  }
  function rebuild() {
    nodes.forEach((n) => remembered.set(n.id, { x: n.x, y: n.y }));
    build();
    startSim(0.35);
    hover = null;
    renderStats();
  }

  /* ---------- coordinates ---------- */
  const toWorld = (sx, sy) => ({ x: (sx - view.x) / view.k, y: (sy - view.y) / view.k });
  function nodeAt(sx, sy) {
    const w = toWorld(sx, sy);
    let best = null, bd = Infinity;
    for (const n of nodes) {
      const r = n.r * S.display.nodeSize;
      const d = (n.x - w.x) ** 2 + (n.y - w.y) ** 2, hit = (r + 5 / view.k) ** 2;
      if (d < hit && d < bd) { best = n; bd = d; }
    }
    return best;
  }
  const local = (e) => { const r = cv.getBoundingClientRect(); return { x: e.clientX - r.left, y: e.clientY - r.top }; };

  /* ---------- drawing ---------- */
  function requestDraw() { if (!frame) frame = requestAnimationFrame(draw); }
  function draw() {
    frame = 0;
    if (!open) return;
    const W = cv.clientWidth, H = cv.clientHeight, dpr = devicePixelRatio || 1;
    if (cv.width !== Math.round(W * dpr) || cv.height !== Math.round(H * dpr)) { cv.width = Math.round(W * dpr); cv.height = Math.round(H * dpr); }
    const D = S.display;
    c.setTransform(dpr, 0, 0, dpr, 0, 0);
    c.clearRect(0, 0, W, H);
    c.save();
    c.translate(view.x, view.y);
    c.scale(view.k, view.k);

    const focus = drag ? drag.node : hover;
    const near = focus ? neighbors.get(focus.id) : null;
    const lit = (n) => !focus || n === focus || near.has(n.id);

    for (const l of links) {
      const on = focus && (l.source === focus || l.target === focus);
      const col = on ? "rgba(0,229,255,.8)" : focus ? "rgba(0,229,255,.06)" : "rgba(0,229,255,.22)";
      c.strokeStyle = col;
      c.lineWidth = ((on ? 1.6 : 1) * D.linkWidth) / view.k;
      c.beginPath(); c.moveTo(l.source.x, l.source.y); c.lineTo(l.target.x, l.target.y); c.stroke();
      if (D.arrows) {
        const dx = l.target.x - l.source.x, dy = l.target.y - l.source.y, len = Math.hypot(dx, dy) || 1;
        const tr = l.target.r * D.nodeSize + 2, ux = dx / len, uy = dy / len;
        const tx = l.target.x - ux * tr, ty = l.target.y - uy * tr, s = (6 * Math.max(1, D.linkWidth)) / Math.sqrt(view.k);
        c.fillStyle = col;
        c.beginPath(); c.moveTo(tx, ty); c.lineTo(tx - ux * s - uy * s * 0.55, ty - uy * s + ux * s * 0.55); c.lineTo(tx - ux * s + uy * s * 0.55, ty - uy * s - ux * s * 0.55); c.fill();
      }
    }

    const cur = ctx.current();
    for (const n of nodes) {
      const isCur = n.id === cur, col = isCur ? "#ff2e88" : colorOf(n), r = n.r * D.nodeSize;
      c.globalAlpha = (n.ghost ? 0.55 : 1) * (lit(n) ? 1 : 0.16);
      if (D.glow) { c.shadowColor = col; c.shadowBlur = (n === focus || isCur ? 18 : 7) * view.k; }
      c.fillStyle = col;
      c.beginPath();
      if (n.kind === "img") c.rect(n.x - r, n.y - r, r * 2, r * 2);
      else if (n.kind === "tag") { c.moveTo(n.x, n.y - r * 1.2); c.lineTo(n.x + r * 1.2, n.y); c.lineTo(n.x, n.y + r * 1.2); c.lineTo(n.x - r * 1.2, n.y); c.closePath(); }
      else c.arc(n.x, n.y, r, 0, Math.PI * 2);
      c.fill();
      c.shadowBlur = 0;
      if (n.pinned) {
        c.strokeStyle = "#39ff88";
        c.lineWidth = 1.6 / view.k;
        c.beginPath(); c.arc(n.x, n.y, r + 4 / view.k + 1.5, 0, Math.PI * 2); c.stroke();
        c.fillStyle = "#39ff88";
        c.beginPath(); c.arc(n.x + r * 0.75 + 3, n.y - r * 0.75 - 3, 2.6 / Math.sqrt(view.k), 0, Math.PI * 2); c.fill();
      }
    }
    const base = Math.max(0, Math.min(1, (view.k - (0.45 + D.textFade)) / 0.4));
    c.textAlign = "center";
    c.font = '12px "IBM Plex Mono", monospace';
    for (const n of nodes) {
      const always = n === focus || n.id === cur || (near && near.has(n.id));
      const a = always ? 1 : focus ? base * 0.25 : base;
      if (a < 0.03) continue;
      c.globalAlpha = a;
      c.fillStyle = n === focus || n.id === cur ? "#d9def0" : n.kind === "tag" ? "#ff6fae" : "#8a92b4";
      c.fillText(n.label, n.x, n.y + n.r * D.nodeSize + 14);
    }
    c.restore();
    c.globalAlpha = 1;
  }

  /* ---------- pinning ---------- */
  function pin(n, on) {
    n.pinned = on;
    if (on) { n.fx = n.x; n.fy = n.y; pins[n.id] = { x: Math.round(n.x), y: Math.round(n.y) }; }
    else { n.fx = null; n.fy = null; delete pins[n.id]; sim && sim.alpha(Math.max(sim.alpha(), 0.3)).restart(); }
    save(); renderStats(); requestDraw();
  }
  function unpinAll() {
    nodes.forEach((n) => { n.pinned = false; n.fx = null; n.fy = null; });
    pins = {};
    save(); renderStats();
    sim && sim.alpha(0.6).restart();
  }
  function pinAllVisible() { nodes.forEach((n) => { n.pinned = true; n.fx = n.x; n.fy = n.y; pins[n.id] = { x: Math.round(n.x), y: Math.round(n.y) }; }); save(); renderStats(); requestDraw(); }

  /* ---------- input ---------- */
  cv.addEventListener("pointerdown", (e) => {
    if (e.button !== 0) return;
    const p = local(e), n = nodeAt(p.x, p.y);
    cv.setPointerCapture(e.pointerId);
    if (n) { drag = { node: n, moved: false, sx: p.x, sy: p.y, wasPinned: !!n.pinned }; n.fx = n.x; n.fy = n.y; }
    else pan = { sx: p.x, sy: p.y, vx: view.x, vy: view.y };
  });
  cv.addEventListener("pointermove", (e) => {
    const p = local(e);
    if (drag) {
      if (!drag.moved && Math.abs(p.x - drag.sx) + Math.abs(p.y - drag.sy) > 3) { drag.moved = true; sim.alphaTarget(0.3).restart(); }
      if (drag.moved) { const w = toWorld(p.x, p.y); drag.node.fx = w.x; drag.node.fy = w.y; }
      return;
    }
    if (pan) { view.x = pan.vx + (p.x - pan.sx); view.y = pan.vy + (p.y - pan.sy); requestDraw(); return; }
    const h = nodeAt(p.x, p.y);
    if (h !== hover) { hover = h; cv.style.cursor = h ? "pointer" : "grab"; requestDraw(); }
  });
  cv.addEventListener("pointerup", (e) => {
    if (drag) {
      const d = drag;
      drag = null;
      sim.alphaTarget(0);
      if (!d.moved) {
        if (!d.wasPinned) { d.node.fx = null; d.node.fy = null; }
        if (e.shiftKey) pin(d.node, !d.wasPinned);
        else if (d.node.ghost) ctx.onCreate(d.node.label);
        else if (d.node.kind === "tag") addGroup("tag:" + d.node.label);
        else ctx.onOpen(d.node.id);
        return;
      }
      if (d.wasPinned || S.pinOnDrop || e.shiftKey) pin(d.node, true);
      else { d.node.fx = null; d.node.fy = null; }
      return;
    }
    pan = null;
  });
  cv.addEventListener("pointerleave", () => { if (hover && !drag) { hover = null; requestDraw(); } });
  cv.addEventListener("wheel", (e) => {
    e.preventDefault();
    const p = local(e), k = Math.max(0.08, Math.min(4, view.k * Math.exp(-e.deltaY * 0.0015)));
    view.x = p.x - ((p.x - view.x) / view.k) * k;
    view.y = p.y - ((p.y - view.y) / view.k) * k;
    view.k = k;
    requestDraw();
  }, { passive: false });
  cv.addEventListener("contextmenu", (e) => {
    e.preventDefault();
    const p = local(e), n = nodeAt(p.x, p.y);
    if (!n) {
      return ctx.menu(e.clientX, e.clientY, [
        { label: "Fit graph to screen", run: fit },
        { label: S.pinOnDrop ? "Turn off pin on drop" : "Pin notes where I drop them", run: () => { S.pinOnDrop = !S.pinOnDrop; save(); renderPanel(); } },
        { label: "Pin everything in place", run: pinAllVisible },
        { label: "Unpin all", run: unpinAll },
      ], "Graph");
    }
    const items = [];
    if (n.ghost) items.push({ label: "Create this note", run: () => ctx.onCreate(n.label) });
    else if (n.kind === "tag") items.push({ label: "Color this tag as a group", run: () => addGroup("tag:" + n.label) });
    else items.push({ label: "Open", run: () => ctx.onOpen(n.id) });
    items.push(n.pinned ? { label: "Unpin", key: "Shift+click", run: () => pin(n, false) } : { label: "Pin here", key: "Shift+click", run: () => pin(n, true) });
    if (neighbors.get(n.id).size) items.push({ label: "Pin it and its links", run: () => { pin(n, true); neighbors.get(n.id).forEach((id) => { const m = byId.get(id); if (m) pin(m, true); }); } });
    if (!n.ghost && n.kind !== "tag" && n.id.includes("/")) {
      const folder = n.id.slice(0, n.id.lastIndexOf("/"));
      items.push({ label: `Color folder ${folder.split("/").pop()} as a group`, run: () => addGroup("path:" + folder) });
    }
    ctx.menu(e.clientX, e.clientY, items, n.label);
  });

  function fit() {
    if (!nodes.length) return;
    const W = cv.clientWidth, H = cv.clientHeight;
    const xs = nodes.map((n) => n.x), ys = nodes.map((n) => n.y);
    const minX = Math.min(...xs), maxX = Math.max(...xs), minY = Math.min(...ys), maxY = Math.max(...ys);
    const k = Math.max(0.08, Math.min(1.3, Math.min((W - 120) / Math.max(1, maxX - minX), (H - 140) / Math.max(1, maxY - minY))));
    view.k = k;
    view.x = W / 2 - ((minX + maxX) / 2) * k;
    view.y = H / 2 - ((minY + maxY) / 2) * k;
    requestDraw();
  }
  addEventListener("resize", requestDraw);

  /* ---------- settings panel ---------- */
  function addGroup(query) {
    if (S.groups.some((g) => g.query.toLowerCase() === query.toLowerCase())) { S.open.groups = true; renderPanel(); return; }
    S.groups.push({ query, color: GROUP_COLORS[S.groups.length % GROUP_COLORS.length] });
    S.open.groups = true;
    save(); renderPanel(); requestDraw();
  }
  const sec = (key, title, body) =>
    `<section class="gsec${S.open[key] ? " open" : ""}" data-sec="${key}"><button class="gsec-h" data-toggle="${key}" aria-expanded="${!!S.open[key]}"><span class="car">▶</span>${title}</button><div class="gsec-b">${body}</div></section>`;
  const toggle = (id, label, on, hint = "") => `<label class="gtog" title="${esc(hint)}"><span>${label}</span><input type="checkbox" id="${id}" ${on ? "checked" : ""}><i></i></label>`;
  const slider = (id, label, v, min, max, step, fmt = (x) => x) => `<label class="gsl" for="${id}"><span>${label}</span><output id="${id}-o">${fmt(v)}</output></label><input type="range" id="${id}" min="${min}" max="${max}" step="${step}" value="${v}">`;

  function renderPanel() {
    if (!panel) return;
    const F = S.filters, D = S.display, P = S.forces;
    panel.innerHTML =
      sec("filters", "Filters",
        `<input type="search" id="g-search" class="gsearch" placeholder="Search files…" value="${esc(F.search)}" autocomplete="off" spellcheck="false">` +
        toggle("g-tags", "Tags", F.tags, "Show each #tag as its own node") +
        toggle("g-att", "Attachments", F.attachments, "Show images") +
        toggle("g-exist", "Existing files only", F.existingOnly, "Hide links to notes you haven't made yet") +
        toggle("g-orph", "Orphans", F.orphans, "Show notes with no links")) +
      sec("groups", "Groups",
        `<div class="ggroups">${S.groups.map((g, i) => `<div class="ggroup"><input type="color" value="${esc(g.color)}" data-gcolor="${i}" aria-label="Group color"><input type="text" value="${esc(g.query)}" data-gquery="${i}" placeholder="tag:#idea  path:Projects  or text" spellcheck="false"><button class="gx" data-gdel="${i}" aria-label="Remove group">×</button></div>`).join("") || '<div class="ghint">Color notes by tag, folder or name.</div>'}</div>
         <button class="btn gadd" id="g-addgroup">New group</button>
         <div class="ghint">Examples: <code>tag:#project</code> · <code>path:Homelab</code> · <code>plan</code></div>`) +
      sec("display", "Display",
        toggle("g-arrows", "Arrows", D.arrows, "Show which way each link points") +
        toggle("g-glow", "Neon glow", D.glow) +
        slider("g-fade", "Text fade threshold", D.textFade, -0.4, 1.2, 0.05, (x) => (+x).toFixed(2)) +
        slider("g-size", "Node size", D.nodeSize, 0.4, 3, 0.1, (x) => (+x).toFixed(1)) +
        slider("g-lw", "Link thickness", D.linkWidth, 0.3, 4, 0.1, (x) => (+x).toFixed(1))) +
      sec("forces", "Forces",
        slider("g-center", "Center force", P.center, 0, 1, 0.05) +
        slider("g-repel", "Repel force", P.repel, 0, 20, 0.5) +
        slider("g-lstr", "Link force", P.linkStrength, 0, 1, 0.05) +
        slider("g-ldist", "Link distance", P.linkDistance, 30, 500, 10) +
        `<button class="btn" id="g-resetf">Reset forces</button>`) +
      sec("pinning", "Pinning",
        toggle("g-pod", "Pin on drop", S.pinOnDrop, "Notes stay wherever you drop them") +
        `<div class="gstat" id="g-pinstat"></div>
         <div class="grow"><button class="btn" id="g-pinall">Pin all</button><button class="btn" id="g-unpin">Unpin all</button></div>
         <div class="ghint">Shift+click a note to pin or unpin it. Right-click for more.</div>`) +
      `<div class="gstat gfoot" id="g-count"></div>`;
    renderStats();
  }
  function renderStats() {
    if (!panel) return;
    const ps = panel.querySelector("#g-pinstat"), cn = panel.querySelector("#g-count");
    const np = Object.keys(pins).length;
    if (ps) ps.textContent = np ? `${np} pinned` : "Nothing pinned";
    if (cn) cn.textContent = `${nodes.length} nodes · ${links.length} links`;
    const un = panel.querySelector("#g-unpin");
    if (un) un.disabled = !np;
  }
  if (panel) {
    let searchTimer;
    panel.addEventListener("click", (e) => {
      const t = e.target.closest("[data-toggle]");
      if (t) { const k = t.dataset.toggle; S.open[k] = !S.open[k]; t.closest(".gsec").classList.toggle("open", S.open[k]); t.setAttribute("aria-expanded", S.open[k]); save(); return; }
      const del = e.target.closest("[data-gdel]");
      if (del) { S.groups.splice(+del.dataset.gdel, 1); save(); renderPanel(); requestDraw(); return; }
      if (e.target.id === "g-addgroup") { S.groups.push({ query: "", color: GROUP_COLORS[S.groups.length % GROUP_COLORS.length] }); save(); renderPanel(); const ins = panel.querySelectorAll("[data-gquery]"); ins[ins.length - 1]?.focus(); return; }
      if (e.target.id === "g-resetf") { S.forces = clone(DEFAULTS.forces); applyForces(); save(); renderPanel(); return; }
      if (e.target.id === "g-unpin") return unpinAll();
      if (e.target.id === "g-pinall") return pinAllVisible();
    });
    panel.addEventListener("input", (e) => {
      const id = e.target.id, v = e.target.value;
      const out = panel.querySelector(`#${CSS.escape(id)}-o`);
      const num = +v;
      switch (id) {
        case "g-search": S.filters.search = v; clearTimeout(searchTimer); searchTimer = setTimeout(() => { save(); rebuild(); }, 200); return;
        case "g-fade": S.display.textFade = num; out.textContent = num.toFixed(2); break;
        case "g-size": S.display.nodeSize = num; out.textContent = num.toFixed(1); applyForces(); break;
        case "g-lw": S.display.linkWidth = num; out.textContent = num.toFixed(1); break;
        case "g-center": S.forces.center = num; out.textContent = num; applyForces(); break;
        case "g-repel": S.forces.repel = num; out.textContent = num; applyForces(); break;
        case "g-lstr": S.forces.linkStrength = num; out.textContent = num; applyForces(); break;
        case "g-ldist": S.forces.linkDistance = num; out.textContent = num; applyForces(); break;
        default:
          if (e.target.dataset.gquery !== undefined) S.groups[+e.target.dataset.gquery].query = v;
          else if (e.target.dataset.gcolor !== undefined) S.groups[+e.target.dataset.gcolor].color = v;
          else return;
      }
      save(); requestDraw();
    });
    panel.addEventListener("change", (e) => {
      const on = e.target.checked;
      const map = { "g-tags": ["filters", "tags"], "g-att": ["filters", "attachments"], "g-exist": ["filters", "existingOnly"], "g-orph": ["filters", "orphans"], "g-arrows": ["display", "arrows"], "g-glow": ["display", "glow"] };
      if (map[e.target.id]) {
        const [grp, key] = map[e.target.id];
        S[grp][key] = on;
        save();
        if (grp === "filters") rebuild(); else requestDraw();
      } else if (e.target.id === "g-pod") { S.pinOnDrop = on; save(); }
    });
    panel.addEventListener("keydown", (e) => { if (e.key !== "Escape") e.stopPropagation(); }); // typing in the panel shouldn't trigger app shortcuts
  }

  return {
    async show() {
      await load();
      open = true;
      build();
      renderPanel();
      const W = cv.clientWidth, H = cv.clientHeight;
      if (!remembered.size) view = { k: 1, x: W / 2, y: H / 2 };
      pendingFit = !remembered.size;
      startSim(remembered.size ? 0.3 : 1);
      requestDraw();
    },
    hide() {
      open = false;
      if (sim) sim.stop();
      nodes.forEach((n) => remembered.set(n.id, { x: n.x, y: n.y }));
    },
    fit,
    /** Keep pins attached when a file or folder is renamed or moved. */
    async remap(from, to) {
      if (!loaded) await load();
      let changed = false;
      for (const p of Object.keys(pins)) {
        if (p === from || p.startsWith(from + "/")) { pins[to + p.slice(from.length)] = pins[p]; delete pins[p]; changed = true; }
      }
      for (const [p, v] of [...remembered]) {
        if (p === from || p.startsWith(from + "/")) { remembered.delete(p); remembered.set(to + p.slice(from.length), v); }
      }
      if (changed) save();
    },
    /** Node positions for importing the graph into a canvas. Uses the live or
     *  remembered layout (pins included); runs the physics offline if the graph
     *  hasn't been opened yet. */
    async snapshot() {
      await load();
      const wasOpen = open;
      build();
      const fresh = nodes.some((n) => !remembered.has(n.id) && !pins[n.id] && !n.ghost);
      if (fresh && !wasOpen) {
        const s = d3.forceSimulation(nodes)
          .force("link", d3.forceLink(links).id((d) => d.id).distance(S.forces.linkDistance).strength(S.forces.linkStrength * 0.7))
          .force("charge", d3.forceManyBody().strength(-S.forces.repel * 30).distanceMax(900))
          .force("x", d3.forceX(0).strength(S.forces.center * 0.08))
          .force("y", d3.forceY(0).strength(S.forces.center * 0.08))
          .force("collide", d3.forceCollide((d) => d.r + 6))
          .stop();
        for (let i = 0; i < 300; i++) s.tick();
      }
      const out = {
        nodes: nodes.map((n) => ({ id: n.id, label: n.label, kind: n.kind, ghost: n.ghost, x: n.x, y: n.y })),
        links: links.map((l) => ({ source: typeof l.source === "object" ? l.source.id : l.source, target: typeof l.target === "object" ? l.target.id : l.target })),
      };
      if (!wasOpen) nodes.forEach((n) => remembered.set(n.id, { x: n.x, y: n.y }));
      return out;
    },
    /** Forget positions when another vault opens. */
    reset() { remembered.clear(); nodes = []; links = []; byId = new Map(); loaded = false; },
  };
}
