// Small markdown renderer for Null Notes. Covers what notes actually use:
// headings, lists, tasks, quotes, code, tables, links, [[wikilinks]],
// ![[embeds]], #tags. Output is built from escaped text only.

export const esc = (s) =>
  String(s).replace(/[&<>"]/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;" })[c]);

/** Every link target in a note: [[wikilinks]], ![[embeds]] and ![](local images). */
export function linkTargets(text) {
  const out = [];
  const re = /\[\[([^\]|#]+)(?:#[^\]|]*)?(?:\|[^\]]*)?\]\]/g;
  let m;
  while ((m = re.exec(text))) out.push(m[1].trim());
  const re2 = /!\[[^\]]*\]\(([^)\s]+)\)/g;
  while ((m = re2.exec(text))) {
    if (!/^[a-z]+:/i.test(m[1])) {
      try { out.push(decodeURIComponent(m[1])); } catch { out.push(m[1]); }
    }
  }
  return out;
}

export function tagsIn(text) {
  const set = new Set();
  const clean = text.replace(/```[\s\S]*?```/g, "").replace(/`[^`]*`/g, "");
  const re = /(^|\s)#([a-zA-Z][\w\-/]*)/g;
  let m;
  while ((m = re.exec(clean))) set.add(m[2].toLowerCase());
  return [...set];
}

/**
 * ctx.resolve(name) -> vault path or null
 * ctx.display(path) -> label for a path
 * ctx.isImg(path)   -> true for images
 */
export function render(md, ctx) {
  const inline = (s) => {
    const keep = [];
    const hold = (h) => { keep.push(h); return "\u0001" + (keep.length - 1) + "\u0001"; };
    s = s.replace(/`([^`]+)`/g, (_, c) => hold("<code>" + esc(c) + "</code>"));
    s = s.replace(/!\[\[([^\]|]+?)(?:\|([^\]]+))?\]\]/g, (_, t, opt) => {
      const target = t.trim(), hit = ctx.resolve(target);
      if (hit && ctx.isImg(hit)) {
        const w = /^\d+$/.test((opt || "").trim()) ? ` style="width:${+opt}px"` : "";
        return hold(`<img class="embed" data-img="${esc(hit)}" alt="${esc(ctx.display(hit))}"${w}>`);
      }
      if (hit) return hold(`<a href="#" class="wl" data-wl="${esc(target)}">${esc(opt || ctx.display(hit))}</a>`);
      return hold(`<span class="missingimg">missing: ${esc(target)}</span>`);
    });
    s = s.replace(/!\[([^\]]*)\]\(([^)\s]+)\)/g, (_, alt, src) => {
      if (/^[a-z]+:/i.test(src)) return hold(`<a href="#" data-ext="${esc(src)}">image: ${esc(alt || src)}</a>`);
      let name = src; try { name = decodeURIComponent(src); } catch {}
      const hit = ctx.resolve(name);
      if (hit && ctx.isImg(hit)) return hold(`<img class="embed" data-img="${esc(hit)}" alt="${esc(alt)}">`);
      return hold(`<span class="missingimg">missing: ${esc(src)}</span>`);
    });
    s = esc(s);
    s = s.replace(/\[\[([^\]|]+?)(?:\|([^\]]+))?\]\]/g, (_, t, a) => {
      const target = t.split("#")[0].trim();
      const hit = ctx.resolve(target.replace(/&amp;/g, "&").replace(/&quot;/g, '"'));
      return hold(`<a href="#" class="wl${hit ? "" : " missing"}" data-wl="${target}" title="${hit ? "Open" : "Create"} ${target}">${a || (hit ? esc(ctx.display(hit)) : t)}</a>`);
    });
    s = s.replace(/\[([^\]]+)\]\(((?:https?|mailto):[^)\s]+)\)/g, (_, t, u) => hold(`<a href="#" data-ext="${u}" title="${u}">${t}</a>`));
    s = s.replace(/\*\*([^*]+)\*\*/g, "<strong>$1</strong>").replace(/(^|[^*])\*([^*]+)\*/g, "$1<em>$2</em>");
    s = s.replace(/~~([^~]+)~~/g, "<del>$1</del>");
    s = s.replace(/(^|\s)#([a-zA-Z][\w\-/]*)/g, '$1<span class="tg" data-tag="$2">#$2</span>');
    return s.replace(/\u0001(\d+)\u0001/g, (_, i) => keep[i]);
  };

  const lines = md.replace(/\r/g, "").split("\n");
  const heads = [];
  let html = "", i = 0, hId = 0;
  while (i < lines.length) {
    const l = lines[i];
    let m;
    if (/^```/.test(l)) {
      const lang = l.slice(3).trim().toLowerCase();
      const buf = []; i++;
      while (i < lines.length && !/^```/.test(lines[i])) buf.push(lines[i++]);
      i++;
      if (lang === "mermaid") html += `<div class="mermaid-block"><pre class="mermaid-src" hidden>${esc(buf.join("\n"))}</pre><div class="mermaid-out">Drawing diagram…</div></div>`;
      else html += `<pre${lang ? ` data-lang="${esc(lang)}"` : ""}><code>` + esc(buf.join("\n")) + "</code></pre>";
      continue;
    }
    if ((m = l.match(/^(#{1,6})\s+(.*)/))) {
      const lv = Math.min(3, m[1].length), id = "h" + hId++;
      heads.push({ lv, text: m[2], id });
      html += `<h${lv} id="${id}">${inline(m[2])}</h${lv}>`;
      i++; continue;
    }
    if (/^(-{3,}|\*{3,}|_{3,})$/.test(l.trim())) { html += "<hr>"; i++; continue; }
    if (/^>\s?/.test(l)) {
      const buf = [];
      while (i < lines.length && /^>\s?/.test(lines[i])) buf.push(lines[i++].replace(/^>\s?/, ""));
      html += "<blockquote><p>" + inline(buf.join(" ")) + "</p></blockquote>";
      continue;
    }
    if (/^\|.*\|\s*$/.test(l) && i + 1 < lines.length && /^\|[\s:|-]+\|\s*$/.test(lines[i + 1])) {
      const cells = (r) => r.trim().replace(/^\||\|$/g, "").split("|").map((c) => c.trim());
      const head = cells(l); i += 2;
      let body = "";
      while (i < lines.length && /^\|.*\|\s*$/.test(lines[i])) body += "<tr>" + cells(lines[i++]).map((c) => "<td>" + inline(c) + "</td>").join("") + "</tr>";
      html += '<div class="tablewrap"><table><thead><tr>' + head.map((c) => "<th>" + inline(c) + "</th>").join("") + "</tr></thead><tbody>" + body + "</tbody></table></div>";
      continue;
    }
    if (/^\s*([-*+]|\d+\.)\s+/.test(l)) {
      const tag = /^\s*\d+\./.test(l) ? "ol" : "ul";
      let items = "";
      while (i < lines.length && /^\s*([-*+]|\d+\.)\s+/.test(lines[i])) {
        const raw = lines[i].replace(/^\s*([-*+]|\d+\.)\s+/, "");
        const depth = Math.min(4, Math.floor((lines[i].match(/^\s*/)[0].length) / 2));
        const pad = depth ? ` style="margin-left:${depth * 1.2}em"` : "";
        const t = raw.match(/^\[( |x|X)\]\s+(.*)/);
        if (t) {
          const done = t[1] !== " ";
          items += `<li class="task${done ? " done" : ""}"${pad}><input type="checkbox" data-line="${i}"${done ? " checked" : ""} aria-label="Toggle task"><span>${inline(t[2])}</span></li>`;
        } else items += `<li${pad}>${inline(raw)}</li>`;
        i++;
      }
      html += `<${tag}>${items}</${tag}>`;
      continue;
    }
    if (!l.trim()) { i++; continue; }
    const buf = [];
    while (i < lines.length && lines[i].trim() && !/^(#{1,6}\s|```|>|\s*([-*+]|\d+\.)\s|\|)/.test(lines[i])) buf.push(lines[i++]);
    if (!buf.length) buf.push(lines[i++]);
    html += "<p>" + inline(buf.join(" ")) + "</p>";
  }
  return { html, heads };
}
