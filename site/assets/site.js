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
  const sidebar = $(".sidebar");
  const menu = $(".docs-menu");
  const sideLinks = new Map($$("a[href^='#']", sidebar).map((link) => [link.hash.slice(1), link]));
  const outline = $(".toc ul");
  const outlineLabel = $(".toc .label");
  const overview = { link: sideLinks.get("overview"), items: [...outline.children] };
  const headings = [{ heading: $("#overview", doc), section: overview }];
  let section = overview;

  for (const heading of $$("h2[id], h3[id]", doc)) {
    const title = heading.textContent.trim();
    const fragment = "#" + heading.id;
    if (heading.tagName === "H2") {
      section = { link: sideLinks.get(heading.id), items: [] };
    }
    const link = el("a", heading.tagName === "H3" ? "sub" : null, title);
    link.href = fragment;
    const item = el("li");
    item.append(link);
    section.items.push(item);
    headings.push({ heading, section, link });

    const anchor = el("a", "anchor", "#");
    anchor.href = fragment;
    anchor.setAttribute("aria-label", `Link to ${title}`);
    heading.append(anchor);
  }
  headings.forEach(({ heading }) => { heading.tabIndex = -1; });

  let active;
  const updateLocation = () => {
    // Match native fragment positioning, including the sticky header offset.
    const padding = parseFloat(getComputedStyle(document.documentElement).scrollPaddingTop) || 0;
    const atEnd = scrollY > 0 && scrollY + innerHeight >= document.documentElement.scrollHeight - 2;
    let current;
    if (atEnd) {
      // Near the footer a deep link can reach the scroll limit before its heading reaches the header.
      const target = document.querySelector(":target");
      current = headings.find(({ heading }) => heading === target && heading.getBoundingClientRect().top >= padding - 2) ?? headings.at(-1);
    } else {
      current = headings.findLast(({ heading }) => heading.getBoundingClientRect().top <= padding + 2) ?? headings[0];
    }
    if (current === active) return;
    if (current.section !== active?.section) {
      active?.section.link.removeAttribute("aria-current");
      current.section.link.setAttribute("aria-current", "location");
      outlineLabel.textContent = current.section === overview ? "Quick links" : "In this section";
      outline.replaceChildren(...current.section.items);
    }
    active?.link?.removeAttribute("aria-current");
    current.link?.setAttribute("aria-current", "location");
    active = current;
  };
  addEventListener("scroll", updateLocation, { passive: true });
  addEventListener("hashchange", updateLocation);
  new ResizeObserver(updateLocation).observe(doc);
  updateLocation();

  const narrow = matchMedia("(max-width: 900px)");
  const setMenuOpen = (open) => {
    sidebar.classList.toggle("open", open);
    sidebar.inert = narrow.matches && !open;
    menu.setAttribute("aria-expanded", String(open));
    menu.setAttribute("aria-label", open ? "Close navigation" : "Open navigation");
    if (open) {
      const link = active.section.link;
      link.focus({ preventScroll: true });
      if (link.offsetTop < sidebar.scrollTop || link.offsetTop + link.offsetHeight > sidebar.scrollTop + sidebar.clientHeight) {
        sidebar.scrollTop = link.offsetTop - sidebar.clientHeight / 2;
      }
    }
  };
  menu.addEventListener("click", () => setMenuOpen(!sidebar.classList.contains("open")));
  sidebar.addEventListener("click", (event) => {
    const link = event.target.closest("a[href^='#']");
    if (!sidebar.classList.contains("open") || !link || event.metaKey || event.ctrlKey || event.shiftKey || event.altKey) return;
    setMenuOpen(false);
    document.getElementById(link.hash.slice(1))?.focus({ preventScroll: true });
  });
  const dismissOutside = (event) => {
    if (sidebar.classList.contains("open") && !sidebar.contains(event.target) && !menu.contains(event.target)) {
      setMenuOpen(false);
    }
  };
  document.addEventListener("pointerdown", dismissOutside);
  document.addEventListener("focusin", dismissOutside);
  document.addEventListener("keydown", (event) => {
    if (event.key === "Escape" && sidebar.classList.contains("open")) {
      event.preventDefault();
      setMenuOpen(false);
      menu.focus();
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
    lastFocus = document.activeElement.closest(".sidebar.open") ? $(".docs-menu") : document.activeElement;
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
