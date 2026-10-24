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
let selectedTheme;
try { selectedTheme = localStorage.getItem("diskr-theme"); } catch { /* storage may be disabled */ }
const applyTheme = () => {
  document.documentElement.dataset.theme = selectedTheme || (media.matches ? "dark" : "light");
};
media.addEventListener("change", applyTheme);
$$(".theme-toggle").forEach((b) => b.addEventListener("click", () => {
  const next = document.documentElement.dataset.theme === "dark" ? "light" : "dark";
  selectedTheme = next;
  try { localStorage.setItem("diskr-theme", next); } catch { /* keep this page usable without storage */ }
  applyTheme();
}));

/* Nav hairline once the page scrolls. */
const nav = $(".nav");
const onScroll = () => nav?.classList.toggle("scrolled", scrollY > 8);
addEventListener("scroll", onScroll, { passive: true });
onScroll();

/* Copy buttons: the text comes from data-copy or the sibling <pre>. */
$$(".copy").forEach((button) => {
  const label = button.getAttribute("aria-label") || "Copy";
  let feedbackTimer;
  button.addEventListener("click", async () => {
    const text = button.dataset.copy ?? button.closest(".code")?.querySelector("pre")?.innerText.trim();
    if (!text) return;
    try {
      await navigator.clipboard.writeText(text);
      clearTimeout(feedbackTimer);
      button.classList.add("copied");
      button.setAttribute("aria-label", "Copied");
      feedbackTimer = setTimeout(() => {
        button.classList.remove("copied");
        button.setAttribute("aria-label", label);
      }, 1600);
    } catch { /* clipboard unavailable; leave the text selectable */ }
  });
});

/* Let an overflowing terminal keep the browser's horizontal keyboard scroll. */
$$(".screen").forEach((screen) => screen.addEventListener("keydown", (e) => {
  if ((e.key === "ArrowLeft" || e.key === "ArrowRight") && screen.scrollWidth > screen.clientWidth) {
    e.stopPropagation();
  }
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
      const next = d ? buttons[(i + d + buttons.length) % buttons.length]
        : e.key === "Home" ? buttons[0] : e.key === "End" ? buttons.at(-1) : null;
      if (next) { e.preventDefault(); select(next); next.focus(); }
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
  const menu = $(".docs-menu");
  const narrow = matchMedia("(max-width: 900px)");
  const setMenuOpen = (open) => {
    sidebar?.classList.toggle("open", open);
    if (sidebar) sidebar.inert = narrow.matches && !open;
    menu?.setAttribute("aria-expanded", String(open));
    menu?.setAttribute("aria-label", open ? "Close navigation" : "Open navigation");
  };
  menu?.addEventListener("click", () => setMenuOpen(!sidebar.classList.contains("open")));
  sidebar?.addEventListener("click", (e) => {
    if (e.target.closest("a")) setMenuOpen(false);
  });
  sidebar?.addEventListener("keydown", (e) => {
    if (e.key === "Escape" && narrow.matches) {
      e.preventDefault();
      setMenuOpen(false);
      menu?.focus();
    }
  });
  narrow.addEventListener("change", () => setMenuOpen(false));
  setMenuOpen(false);
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
let indexPromise;
function buildIndex() {
  if (!indexPromise) indexPromise = (async () => {
    let page = document;
    if (!doc) {
      const response = await fetch(root + "docs/");
      if (!response.ok) throw new Error("Documentation could not be loaded.");
      page = new DOMParser().parseFromString(await response.text(), "text/html");
    }
    const index = $$(".doc h2[id], .doc h3[id]", page).map((h) => {
      let text = "";
      for (let n = h.nextElementSibling; n && !/^H[23]$/.test(n.tagName); n = n.nextElementSibling) if (!n.matches(".figure, .term-block")) text += " " + n.textContent;
      const parent = h.tagName === "H3" ? (() => { let p = h.previousElementSibling; while (p && p.tagName !== "H2") p = p.previousElementSibling; return p; })() : null;
      return {
        title: h.firstChild.textContent.trim(),
        crumb: parent ? parent.firstChild.textContent.trim() : "Docs",
        text: text.replace(/\s+/g, " ").trim(),
        href: (doc ? "" : root + "docs/") + "#" + h.id,
      };
    });
    if (!index.length) throw new Error("Documentation has no searchable sections.");
    return index;
  })().catch((error) => { indexPromise = undefined; throw error; });
  return indexPromise;
}

function highlight(target, text, q) {
  const i = q ? text.toLowerCase().indexOf(q) : -1;
  if (i < 0) { target.textContent = text; return; }
  target.append(text.slice(0, i), el("mark", null, text.slice(i, i + q.length)), text.slice(i + q.length));
}

if (palette) {
  const input = $("input", palette);
  const list = $(".palette-results", palette);
  const status = $(".palette-status", palette);
  let selected = 0;
  let results = [];
  let searchVersion = 0;
  let lastFocus;

  const setSelected = (next, focus = false) => {
    selected = Math.max(0, Math.min(next, results.length - 1));
    [...list.children].forEach((li, i) => {
      li.classList.toggle("sel", i === selected);
      const link = $("a", li);
      if (i === selected) link?.setAttribute("aria-current", "true");
      else link?.removeAttribute("aria-current");
    });
    const current = list.children[selected];
    current?.scrollIntoView({ block: "nearest" });
    if (status && results[selected]) status.textContent = `${results[selected].item.title}, result ${selected + 1} of ${results.length}.`;
    if (focus) $("a", current)?.focus();
  };

  const render = () => {
    list.replaceChildren();
    if (!results.length) {
      list.append(el("li", "palette-empty", "No matches. Try “reclaim”, “json” or a key like “S”."));
      if (status) status.textContent = "No matching sections.";
      return;
    }
    results.forEach((r, i) => {
      const li = el("li");
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
      a.addEventListener("focus", () => setSelected(i));
      a.addEventListener("keydown", (e) => {
        if (e.key === "ArrowDown" || e.key === "ArrowUp") {
          e.preventDefault();
          setSelected(selected + (e.key === "ArrowDown" ? 1 : -1), true);
        }
      });
      li.append(a);
      list.append(li);
    });
    setSelected(0);
  };

  const search = async () => {
    const version = ++searchVersion;
    const q = input.value.trim().toLowerCase();
    list.setAttribute("aria-busy", "true");
    try {
      const items = await buildIndex();
      if (version !== searchVersion || !palette.open) return;
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
      render();
    } catch {
      if (version !== searchVersion || !palette.open) return;
      results = [];
      const li = el("li", "palette-empty", "Search is unavailable. ");
      const link = el("a", null, "Browse the documentation");
      link.href = root + "docs/";
      link.addEventListener("click", close);
      li.append(link, ", or type again to retry.");
      list.replaceChildren(li);
      if (status) status.textContent = "Search could not load the documentation.";
    } finally {
      if (version === searchVersion) list.removeAttribute("aria-busy");
    }
  };

  const open = () => {
    if (palette.open) return;
    lastFocus = document.activeElement;
    input.value = "";
    results = [];
    list.replaceChildren();
    if (status) status.textContent = "Loading documentation…";
    palette.showModal();
    input.focus();
    search();
  };
  function close() {
    if (!palette.open) return;
    searchVersion++;
    list.removeAttribute("aria-busy");
    palette.close();
  }
  palette.addEventListener("close", () => {
    if (!palette.open && lastFocus?.isConnected) lastFocus.focus({ preventScroll: true });
  });
  palette.addEventListener("cancel", (e) => { e.preventDefault(); close(); });
  $(".palette-close", palette)?.addEventListener("click", close);
  $$(".search-trigger").forEach((b) => b.addEventListener("click", open));
  input.addEventListener("input", search);
  input.addEventListener("keydown", (e) => {
    if ((e.key === "ArrowDown" || e.key === "ArrowUp") && results.length) {
      e.preventDefault();
      setSelected(selected + (e.key === "ArrowDown" ? 1 : -1));
    } else if (e.key === "Enter" && results[selected]) {
      e.preventDefault();
      const href = results[selected].item.href;
      close();
      location.href = href;
    }
  });
  palette.addEventListener("click", (e) => { if (e.target === palette) close(); });
  palette.addEventListener("keydown", (e) => {
    if (e.key === "Escape") {
      e.preventDefault();
      e.stopPropagation();
      close();
      return;
    }
    if (e.key !== "Tab") return;
    const controls = $$("a[href], button:not([disabled]), input:not([disabled]), [tabindex='0']", palette)
      .filter((node) => node.getClientRects().length);
    const first = controls[0];
    const last = controls.at(-1);
    if (e.shiftKey && document.activeElement === first) { e.preventDefault(); last?.focus(); }
    else if (!e.shiftKey && document.activeElement === last) { e.preventDefault(); first?.focus(); }
  });
  addEventListener("keydown", (e) => {
    const typing = e.target.closest?.("input, textarea, [contenteditable]");
    if ((e.key === "k" && (e.metaKey || e.ctrlKey)) || (e.key === "/" && !typing)) {
      e.preventDefault();
      palette.open ? close() : open();
    }
  });
}

/* Footer year */
$$("[data-year]").forEach((n) => (n.textContent = new Date().getFullYear()));

mountReplays(root + "assets/replay.json");
