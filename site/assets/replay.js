// Canvas replay of real diskr frames captured from a pseudo-terminal.
// Box-drawing and block glyphs are drawn as geometry so borders connect and
// bars fill cells exactly at any zoom level, matching the README renders.

const PALETTE = {
  "000000": "#181825", cd0000: "#f38ba8", "00cd00": "#a6e3a1", cdcd00: "#f9e2af",
  "0000ee": "#89b4fa", cd00cd: "#cba6f7", "00cdcd": "#94e2d5", e5e5e5: "#bac2de",
  "7f7f7f": "#6c7086", ff0000: "#f38ba8", "00ff00": "#a6e3a1", ffff00: "#f9e2af",
  "5c5cff": "#89b4fa", ff00ff: "#f5c2e7", "00ffff": "#89dceb", ffffff: "#e6e9f5",
  "949494": "#8c92ac", "262626": "#313244", bcbcbc: "#a6adc8", "303030": "#313244",
  "3a3a3a": "#45475a",
};
const BG = "#1e1e2e";
const FG = "#cdd6f4";
const BOX = {
  "─": "LR", "│": "UD", "┌": "RD", "┐": "LD", "└": "RU", "┘": "LU", "├": "UDR",
  "┤": "UDL", "┬": "LRD", "┴": "LRU", "┼": "LRUD", "╭": "RD", "╮": "LD", "╰": "RU",
  "╯": "LU", "╴": "L", "╶": "R", "╵": "U", "╷": "D",
};
const SHADES = { "░": 0.28, "▒": 0.5, "▓": 0.75 };
const FONT = '"JetBrains Mono", ui-monospace, "SF Mono", Menlo, monospace';
const LINE = 1.32;

function color(value, isBg) {
  if (value === "default" || (isBg && value === "000000")) return isBg ? BG : FG;
  if (PALETTE[value]) return PALETTE[value];
  if (/^[0-9a-f]{6}$/i.test(value)) return "#" + value;
  return FG;
}

function mix(a, b, t) {
  const p = (h) => [1, 3, 5].map((i) => parseInt(h.slice(i, i + 2), 16));
  const [x, y] = [p(a), p(b)];
  return `rgb(${x.map((v, i) => Math.round(v * t + y[i] * (1 - t))).join(",")})`;
}

function el(tag, className, text) {
  const node = document.createElement(tag);
  if (className) node.className = className;
  if (text != null) node.textContent = text;
  return node;
}

let dataPromise;
export function loadReplay(url) {
  dataPromise ??= fetch(url).then((r) => r.json());
  return dataPromise;
}

export function frameText(data, index) {
  return data.frames[index].l.map((l) => data.lines[l].map((r) => r[0]).join("").replaceAll("\u0000", " "));
}

export class TerminalView {
  constructor(canvas, data) {
    this.canvas = canvas;
    this.data = data;
    this.ctx = canvas.getContext("2d");
    this.index = 0;
    new ResizeObserver(() => this.resize()).observe(canvas.parentElement);
    canvas.parentElement.addEventListener("scroll", (e) => {
      e.currentTarget.closest(".window")?.classList.add("panned");
    }, { passive: true, once: true });
    this.resize();
  }

  resize() {
    const { cols, rows } = this.data;
    const width = this.canvas.clientWidth;
    if (width <= 0) return;
    const dpr = Math.min(window.devicePixelRatio || 1, 3);
    this.cw = width / cols;
    this.size = this.cw / 0.6;
    this.ch = Math.round(this.size * LINE * dpr) / dpr;
    const height = this.ch * rows;
    this.canvas.style.aspectRatio = `${width} / ${height}`;
    this.canvas.width = Math.round(width * dpr);
    this.canvas.height = Math.round(height * dpr);
    this.dpr = dpr;
    this.draw(this.index);
  }

  draw(index) {
    this.index = index;
    const { ctx, data, dpr } = this;
    if (!dpr) return;
    const frame = data.frames[index];
    const cw = this.cw * dpr;
    const ch = this.ch * dpr;
    const size = this.size * dpr;
    ctx.setTransform(1, 0, 0, 1, 0, 0);
    ctx.fillStyle = BG;
    ctx.fillRect(0, 0, this.canvas.width, this.canvas.height);
    ctx.textBaseline = "middle";
    const t = Math.max(1, Math.round(dpr * 1.1));
    const inset = Math.round(ch * 0.18);
    frame.l.forEach((lineId, y) => {
      const top = Math.round(y * ch);
      const bottom = Math.round((y + 1) * ch);
      let x = 0;
      for (const [text, styleId] of data.lines[lineId]) {
        const [fg, bg, bold, reverse] = data.styles[styleId];
        let fgc = color(fg, false);
        let bgc = color(bg, true);
        if (reverse) [fgc, bgc] = [bgc, fgc];
        ctx.font = `${bold ? 600 : 400} ${size}px ${FONT}`;
        for (const glyph of text) {
          const x0 = Math.round(x * cw);
          const x1 = Math.round((x + 1) * cw);
          x += 1;
          if (bgc !== BG) {
            ctx.fillStyle = bgc;
            ctx.fillRect(x0, top, x1 - x0, bottom - top);
          }
          if (glyph === " " || glyph === "\u0000") continue;
          ctx.fillStyle = fgc;
          const segs = BOX[glyph];
          if (segs) {
            const cx = Math.round((x0 + x1) / 2 - t / 2);
            const cy = Math.round((top + bottom) / 2 - t / 2);
            if (segs.includes("L")) ctx.fillRect(x0, cy, cx - x0 + t, t);
            if (segs.includes("R")) ctx.fillRect(cx, cy, x1 - cx, t);
            if (segs.includes("U")) ctx.fillRect(cx, top, t, cy - top + t);
            if (segs.includes("D")) ctx.fillRect(cx, cy, t, bottom - cy);
          } else if (glyph === "█" || SHADES[glyph]) {
            ctx.fillStyle = glyph === "█" ? fgc : mix(fgc, bgc, SHADES[glyph]);
            ctx.fillRect(x0, top + inset, x1 - x0, bottom - top - 2 * inset);
          } else {
            ctx.fillText(glyph, x0, (top + bottom) / 2 + size * 0.04);
          }
        }
      }
    });
  }
}

function fmt(ms) {
  const s = Math.floor(ms / 1000);
  return `${Math.floor(s / 60)}:${String(s % 60).padStart(2, "0")}`;
}

export class Player {
  constructor(root, data) {
    this.root = root;
    this.data = data;
    this.view = new TerminalView(root.querySelector("canvas"), data);
    this.times = [];
    let acc = 0;
    for (const f of data.frames) { this.times.push(acc); acc += f.d; }
    this.total = acc;
    this.position = 0;
    this.playing = false;
    this.player = root.querySelector(".player");
    this.button = root.querySelector(".play");
    this.clock = root.querySelector(".clock");
    this.buildTrack();
    this.bind();
    this.seek(this.times[Math.min(4, this.times.length - 1)]);
  }

  buildTrack() {
    const track = this.root.querySelector(".track");
    const chapters = this.data.chapters;
    this.segments = chapters.map((c, i) => {
      const start = this.times[c.frame];
      const end = i + 1 < chapters.length ? this.times[chapters[i + 1].frame] : this.total;
      const button = el("button", "segment");
      button.type = "button";
      button.style.setProperty("--w", end - start);
      button.setAttribute("aria-label", `Jump to ${c.name}`);
      const bar = el("span", "bar");
      bar.append(el("span", "fill"));
      button.append(bar, el("span", "name", `${i + 1} · ${c.name}`));
      track.append(button);
      return { el: button, start, end };
    });
  }

  bind() {
    this.button.addEventListener("click", () => this.toggle());
    const track = this.root.querySelector(".track");
    const seekFromPointer = (e) => {
      for (const s of this.segments) {
        const r = s.el.getBoundingClientRect();
        if (e.clientX <= r.right || s === this.segments.at(-1)) {
          const p = Math.min(1, Math.max(0, (e.clientX - r.left) / r.width));
          this.seek(s.start + p * (s.end - s.start));
          return;
        }
      }
    };
    track.addEventListener("pointerdown", (e) => {
      this.scrubbing = true;
      this.wasPlaying = this.playing;
      this.pause();
      track.setPointerCapture(e.pointerId);
      seekFromPointer(e);
    });
    track.addEventListener("pointermove", (e) => this.scrubbing && seekFromPointer(e));
    track.addEventListener("pointerup", () => {
      this.scrubbing = false;
      if (this.wasPlaying) this.play();
    });
    this.root.addEventListener("keydown", (e) => {
      const idx = this.frameAt(this.position);
      if (e.key === " " || e.key === "k") { e.preventDefault(); this.toggle(); }
      else if (e.key === "ArrowRight" || e.key === "l") { e.preventDefault(); this.pause(); this.seek(this.times[Math.min(idx + 1, this.times.length - 1)]); }
      else if (e.key === "ArrowLeft" || e.key === "j") { e.preventDefault(); this.pause(); this.seek(this.times[Math.max(idx - 1, 0)]); }
      else if (/^[1-9]$/.test(e.key) && this.segments[+e.key - 1]) { this.seek(this.segments[+e.key - 1].start); }
    });
    const reduce = matchMedia("(prefers-reduced-motion: reduce)").matches;
    new IntersectionObserver(([entry]) => {
      if (entry.isIntersecting && !reduce && !this.userPaused) this.play();
      else if (!entry.isIntersecting) this.pause();
    }, { threshold: 0.35 }).observe(this.root);
    document.addEventListener("visibilitychange", () => document.hidden && this.pause());
  }

  frameAt(ms) {
    let lo = 0, hi = this.times.length - 1;
    while (lo < hi) {
      const mid = (lo + hi + 1) >> 1;
      if (this.times[mid] <= ms) lo = mid; else hi = mid - 1;
    }
    return lo;
  }

  seek(ms) {
    this.position = Math.max(0, Math.min(ms, this.total - 1));
    const idx = this.frameAt(this.position);
    if (idx !== this.view.index || !this.drawn) { this.view.draw(idx); this.drawn = true; }
    for (const s of this.segments) {
      const p = Math.min(1, Math.max(0, (this.position - s.start) / (s.end - s.start)));
      s.el.style.setProperty("--p", p);
      s.el.classList.toggle("active", this.position >= s.start && this.position < s.end);
      s.el.classList.toggle("past", this.position >= s.end);
    }
    if (this.clock) this.clock.textContent = `${fmt(this.position)} / ${fmt(this.total)}`;
  }

  toggle() {
    if (this.playing) { this.pause(); this.userPaused = true; }
    else { this.userPaused = false; this.play(); }
  }

  play() {
    if (this.playing) return;
    this.playing = true;
    this.player.classList.add("playing");
    this.button.setAttribute("aria-label", "Pause demo");
    let last = performance.now();
    const tick = (now) => {
      if (!this.playing) return;
      let next = this.position + (now - last);
      last = now;
      if (next >= this.total) next = 0;
      this.seek(next);
      this.raf = requestAnimationFrame(tick);
    };
    this.raf = requestAnimationFrame(tick);
  }

  pause() {
    this.playing = false;
    cancelAnimationFrame(this.raf);
    this.player.classList.remove("playing");
    this.button.setAttribute("aria-label", "Play demo");
  }
}

// A still frame with annotation pins located by searching the frame's text,
// so pins follow the UI when the frames are regenerated.
export function still(figure, data) {
  const name = figure.dataset.frame;
  const index = data.marks?.[name] ?? +name;
  new TerminalView(figure.querySelector("canvas"), data).draw(index);
  const notes = JSON.parse(figure.dataset.pins || "[]");
  const text = frameText(data, index);
  const screen = figure.querySelector(".screen");
  notes.forEach((note, n) => {
    const row = text.findIndex((line) => line.includes(note.find));
    if (row < 0) return;
    const col = text[row].indexOf(note.find);
    const width = [...note.find].length;
    const pin = el("div", "pin");
    // Screen padding is 12px top / 16px sides; cells divide the remaining box.
    pin.style.left = `calc(16px + (100% - 32px) * ${col / data.cols} - 3px)`;
    pin.style.width = `calc((100% - 32px) * ${width / data.cols} + 6px)`;
    pin.style.top = `calc(12px + (100% - 28px) * ${row / data.rows} - 2px)`;
    pin.style.height = `calc((100% - 28px) / ${data.rows} + 4px)`;
    const button = el("button", null, String(n + 1));
    button.type = "button";
    button.setAttribute("aria-label", note.label);
    const tip = el("span", "tip", note.label);
    tip.setAttribute("role", "tooltip");
    button.addEventListener("click", () => pin.classList.toggle("open"));
    pin.append(button, tip);
    screen.append(pin);
  });
}

export async function mountReplays(url) {
  const nodes = [...document.querySelectorAll("[data-replay], [data-frame]")];
  if (!nodes.length) return;
  await document.fonts?.load(`16px ${FONT}`).catch(() => {});
  const data = await loadReplay(url);
  for (const node of nodes) {
    if (node.hasAttribute("data-replay")) new Player(node, data);
    else still(node, data);
  }
}
