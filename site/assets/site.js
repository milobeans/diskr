import { mountReplays } from "./replay.js";

const root = document.documentElement.dataset.root || "./";
const $ = (sel, scope = document) => scope.querySelector(sel);
const $$ = (sel, scope = document) => [...scope.querySelectorAll(sel)];

function el(tag, className, text) {
  const node = document.createElement(tag);
  if (className) node.className = className;
  if (text != null) node.textContent = text;
  return node;
}

/* Theme: follow the system until the reader picks one. */
const media = matchMedia("(prefers-color-scheme: dark)");
const applyTheme = () => {
  const saved = localStorage.getItem("diskr-theme");
  document.documentElement.dataset.theme = saved || (media.matches ? "dark" : "light");
};
media.addEventListener("change", applyTheme);
$$(".theme-toggle").forEach((b) => b.addEventListener("click", () => {
  const next = document.documentElement.dataset.theme === "dark" ? "light" : "dark";
  localStorage.setItem("diskr-theme", next);
  applyTheme();
}));

/* Nav hairline once the page scrolls. */
const nav = $(".nav");
const onScroll = () => nav?.classList.toggle("scrolled", scrollY > 8);
addEventListener("scroll", onScroll, { passive: true });
onScroll();

/* Copy buttons: the text comes from data-copy or the sibling <pre>. */
$$(".copy").forEach((button) => button.addEventListener("click", async () => {
  const text = button.dataset.copy ?? button.closest(".code")?.querySelector("pre")?.innerText.replace(/\s+#.*$/gm, "").trim();
  if (!text) return;
  try {
    await navigator.clipboard.writeText(text);
    button.classList.add("copied");
    button.setAttribute("aria-label", "Copied");
    setTimeout(() => { button.classList.remove("copied"); button.setAttribute("aria-label", "Copy"); }, 1600);
  } catch { /* clipboard unavailable; leave the text selectable */ }
}));

/* Tabs */
$$(".tabs").forEach((tabs) => {
  const buttons = $$("[role=tab]", tabs);
  const select = (button) => {
    buttons.forEach((b) => {
      const on = b === button;
      b.setAttribute("aria-selected", on);
      b.tabIndex = on ? 0 : -1;
      $("#" + b.getAttribute("aria-controls")).hidden = !on;
    });
  };
  buttons.forEach((b, i) => {
    b.addEventListener("click", () => select(b));
    b.addEventListener("keydown", (e) => {
      const d = e.key === "ArrowRight" ? 1 : e.key === "ArrowLeft" ? -1 : 0;
      if (d) { const n = buttons[(i + d + buttons.length) % buttons.length]; select(n); n.focus(); }
    });
  });
});

/* Docs: heading anchors, generated outline, scrollspy, mobile drawer. */
const doc = $(".doc");
if (doc) {
  const headings = $$("h2[id], h3[id]", doc);
  headings.forEach((h) => {
    const a = el("a", "anchor", "#");
    a.href = "#" + h.id;
    a.setAttribute("aria-label", `Link to ${h.textContent}`);
    h.append(a);
  });

  const toc = $(".toc ul");
  const tocLinks = new Map();
  if (toc) headings.forEach((h) => {
    const li = el("li");
    const a = el("a", h.tagName === "H3" ? "sub" : null, h.firstChild.textContent.trim());
    a.href = "#" + h.id;
    li.append(a);
    toc.append(li);
    tocLinks.set(h.id, a);
  });

  const sideLinks = new Map($$(".sidebar a[href^='#']").map((a) => [a.getAttribute("href").slice(1), a]));
  const sections = headings.filter((h) => h.tagName === "H2");
  const spy = () => {
    const y = scrollY + innerHeight * 0.3;
    let current = headings[0];
    for (const h of headings) if (h.offsetTop <= y) current = h;
    let section = sections[0];
    for (const h of sections) if (h.offsetTop <= y) section = h;
    tocLinks.forEach((a, id) => a.classList.toggle("active", id === current?.id));
    sideLinks.forEach((a, id) => a.classList.toggle("active", id === section?.id));
  };
  addEventListener("scroll", spy, { passive: true });
  spy();

  const sidebar = $(".sidebar");
  $(".docs-menu")?.addEventListener("click", (e) => {
    const open = sidebar.classList.toggle("open");
    e.currentTarget.setAttribute("aria-expanded", open);
  });
  sidebar?.addEventListener("click", (e) => e.target.closest("a") && sidebar.classList.remove("open"));
}

/* Key reference filter */
const keyInput = $("#key-filter");
if (keyInput) {
  const rows = $$("#keys-table tbody tr");
  const empty = $(".key-empty");
  keyInput.addEventListener("input", () => {
    const q = keyInput.value.trim().toLowerCase();
    let shown = 0;
    rows.forEach((r) => {
      const hit = !q || r.textContent.toLowerCase().includes(q) || r.dataset.key.toLowerCase() === q;
      r.hidden = !hit;
      shown += hit;
    });
    empty.classList.toggle("show", shown === 0);
  });
}

/* Command palette: indexes every docs section, from either page. */
const palette = $(".palette");
let index;
async function buildIndex() {
  if (index) return index;
  let page = document;
  if (!doc) {
    const html = await fetch(root + "docs/").then((r) => r.text());
    page = new DOMParser().parseFromString(html, "text/html");
  }
  index = [];
  $$(".doc h2[id], .doc h3[id]", page).forEach((h) => {
    let text = "";
    for (let n = h.nextElementSibling; n && !/^H[23]$/.test(n.tagName); n = n.nextElementSibling) if (!n.matches(".figure, .term-block")) text += " " + n.textContent;
    const parent = h.tagName === "H3" ? (() => { let p = h.previousElementSibling; while (p && p.tagName !== "H2") p = p.previousElementSibling; return p; })() : null;
    index.push({
      title: h.firstChild.textContent.trim(),
      crumb: parent ? parent.firstChild.textContent.trim() : "Docs",
      text: text.replace(/\s+/g, " ").trim(),
      href: (doc ? "" : root + "docs/") + "#" + h.id,
    });
  });
  return index;
}

function highlight(target, text, q) {
  const i = q ? text.toLowerCase().indexOf(q) : -1;
  if (i < 0) { target.textContent = text; return; }
  target.append(text.slice(0, i), el("mark", null, text.slice(i, i + q.length)), text.slice(i + q.length));
}

if (palette) {
  const input = $("input", palette);
  const list = $(".palette-results", palette);
  let selected = 0;
  let results = [];

  const render = () => {
    list.replaceChildren();
    if (!results.length) { list.append(el("li", "palette-empty", "No matches. Try “reclaim”, “json” or a key like “S”.")); return; }
    results.forEach((r, i) => {
      const li = el("li", i === selected ? "sel" : null);
      li.setAttribute("role", "option");
      li.setAttribute("aria-selected", i === selected);
      const a = el("a");
      a.href = r.item.href;
      const t = el("span", "t");
      highlight(t, r.item.title, r.q);
      const snippet = r.item.text;
      const at = r.q ? snippet.toLowerCase().indexOf(r.q) : -1;
      const s = el("span", "s");
      highlight(s, at > 40 ? "…" + snippet.slice(at - 30, at + 110) : snippet.slice(0, 140), r.q);
      a.append(el("span", "crumb", r.item.crumb), t, s);
      a.addEventListener("click", close);
      li.append(a);
      list.append(li);
    });
    list.children[selected]?.scrollIntoView({ block: "nearest" });
  };

  const search = async () => {
    const items = await buildIndex();
    const q = input.value.trim().toLowerCase();
    results = items
      .map((item) => {
        if (!q) return { item, q, score: 1 };
        const title = item.title.toLowerCase();
        const score = title === q ? 100 : title.startsWith(q) ? 60 : title.includes(q) ? 40 : item.text.toLowerCase().includes(q) ? 10 : 0;
        return { item, q, score };
      })
      .filter((r) => r.score > 0)
      .sort((a, b) => b.score - a.score)
      .slice(0, 12);
    selected = 0;
    render();
  };

  let lastFocus;
  const open = () => {
    lastFocus = document.activeElement;
    palette.classList.add("open");
    input.value = "";
    input.focus();
    search();
  };
  function close() {
    palette.classList.remove("open");
    lastFocus?.focus?.();
  }

  $$(".search-trigger").forEach((b) => b.addEventListener("click", open));
  input.addEventListener("input", search);
  palette.addEventListener("click", (e) => e.target === palette && close());
  palette.addEventListener("keydown", (e) => {
    if (e.key === "Escape") { e.preventDefault(); close(); }
    else if (e.key === "ArrowDown") { e.preventDefault(); selected = Math.min(selected + 1, results.length - 1); render(); }
    else if (e.key === "ArrowUp") { e.preventDefault(); selected = Math.max(selected - 1, 0); render(); }
    else if (e.key === "Enter" && results[selected]) { e.preventDefault(); location.href = results[selected].item.href; close(); }
  });
  addEventListener("keydown", (e) => {
    const typing = e.target.closest?.("input, textarea, [contenteditable]");
    if ((e.key === "k" && (e.metaKey || e.ctrlKey)) || (e.key === "/" && !typing)) {
      e.preventDefault();
      palette.classList.contains("open") ? close() : open();
    }
  });
}

/* Footer year */
$$("[data-year]").forEach((n) => (n.textContent = new Date().getFullYear()));

mountReplays(root + "assets/replay.json");
