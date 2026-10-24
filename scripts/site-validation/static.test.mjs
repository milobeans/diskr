import assert from "node:assert/strict";
import { readFile, readdir, stat } from "node:fs/promises";
import path from "node:path";
import test from "node:test";
import { parse } from "parse5";
import { site, base, origin } from "./paths.mjs";

const attr = (node, name) => node.attrs?.find((a) => a.name === name)?.value;
const hasClass = (node, name) => (attr(node, "class") || "").split(/\s+/).includes(name);
const descendants = (node) => [node, ...(node.childNodes || []).flatMap(descendants)];
const text = (node) => node.nodeName === "#text" ? node.value : (node.childNodes || []).map(text).join("");
function ancestor(node, predicate) {
  for (; node; node = node.parentNode) if (predicate(node)) return node;
}
async function files(directory) {
  const entries = await readdir(directory, { withFileTypes: true });
  return (await Promise.all(entries.map((entry) => entry.isDirectory() ? files(path.join(directory, entry.name)) : path.join(directory, entry.name)))).flat();
}
const allFiles = await files(site);
const documents = new Map(await Promise.all(allFiles.filter((f) => f.endsWith(".html")).map(async (filename) => {
  const nodes = descendants(parse(await readFile(filename, "utf8")));
  return [filename, { nodes, ids: nodes.map((n) => attr(n, "id")).filter(Boolean) }];
})));

async function checkReference(reference, from) {
  if (!reference || reference.startsWith("data:")) return;
  const url = new URL(reference, new URL(path.relative(site, from), base));
  if (url.origin !== origin || !url.pathname.startsWith("/diskr/")) return;
  let target = path.resolve(site, decodeURIComponent(url.pathname.slice(7)));
  assert.ok(target === site || target.startsWith(site + path.sep), `Path escapes site: ${reference}`);
  let info;
  try { info = await stat(target); } catch { assert.fail(`${path.relative(site, from)} links to missing ${reference}`); }
  if (info.isDirectory()) target = path.join(target, "index.html");
  assert.ok((await stat(target)).isFile(), `${reference} is not a file`);
  if (url.hash && documents.has(target)) {
    const id = decodeURIComponent(url.hash.slice(1));
    assert.ok(documents.get(target).ids.includes(id), `${path.relative(site, from)} links to missing fragment ${reference}`);
  }
}

test("every local page, fragment and asset reference resolves in the deployment artifact", async () => {
  assert.ok(documents.size >= 2, "Landing page and documentation must be assembled");
  for (const [filename, { nodes, ids }] of documents) {
    assert.equal(new Set(ids).size, ids.length, `Duplicate ids in ${filename}`);
    for (const node of nodes) {
      for (const key of ["href", "src", "poster"]) await checkReference(attr(node, key), filename);
      if (node.tagName === "meta" && attr(node, "property") === "og:image") await checkReference(attr(node, "content"), filename);
    }
  }
  for (const filename of allFiles.filter((f) => /\.(css|js)$/.test(f))) {
    const source = await readFile(filename, "utf8");
    const references = filename.endsWith(".css")
      ? [...source.matchAll(/url\(\s*["']?([^"')]+)["']?\s*\)/g)].map((m) => m[1])
      : [...source.matchAll(/(?:from\s+|import\s*)["'](\.\.?\/[^"']+)["']/g)].map((m) => m[1]);
    for (const reference of references) await checkReference(reference, filename);
  }
});

test("confirmed mutations cannot enter a generic copy block", () => {
  let mutationCount = 0;
  for (const [filename, { nodes }] of documents) {
    for (const button of nodes.filter((n) => n.tagName === "button" && hasClass(n, "copy"))) {
      const block = ancestor(button, (n) => hasClass(n, "code"));
      const pre = block && descendants(block).find((n) => n.tagName === "pre");
      const payload = (attr(button, "data-copy") ?? (pre && text(pre)) ?? "").trim();
      assert.ok(payload, `Copy button without payload in ${filename}`);
      const commands = payload.split("\n").map((line) => line.trim()).filter((line) => line && !line.startsWith("#"));
      const mutation = commands.some((line) => /(?:^|\s)--yes(?:\s|$)/.test(line));
      if (mutation) {
        mutationCount++;
        assert.equal(attr(block, "data-copy-kind"), "mutation", `Generic copy includes --yes: ${payload}`);
        const disclosure = ancestor(block, (n) => n.tagName === "details");
        assert.ok(disclosure && attr(disclosure, "open") === undefined, "Mutation requires a closed disclosure");
        assert.equal(commands.length, 1, "Mutation must be its own command");
        assert.match(attr(button, "aria-label") || "", /apply|prun|remov|thin|execute/i, "Mutation Copy needs an explicit accessible label");
      }
    }
  }
  assert.ok(mutationCount > 0, "Explicit mutation examples should remain available");
});

test("replay frames, chapters, named stills and line references are valid", async () => {
  const data = JSON.parse(await readFile(path.join(site, "assets", "replay.json"), "utf8"));
  assert.ok(Number.isInteger(data.cols) && data.cols > 0);
  assert.ok(Number.isInteger(data.rows) && data.rows > 0);
  assert.ok(data.frames.length > 1 && data.lines.length > 0 && data.styles.length > 0);
  const validFrame = (index) => Number.isInteger(index) && index >= 0 && index < data.frames.length;
  const play = data.play ?? data.frames.length;
  assert.ok(Number.isInteger(play) && play > 0 && play <= data.frames.length);
  for (const [index, frame] of data.frames.entries()) {
    assert.ok(Number.isFinite(frame.d));
    if (index < play) assert.ok(frame.d > 0, "Playable frames need positive duration");
    else assert.equal(frame.d, 0, "Appended still frames do not contribute playback time");
    assert.equal(frame.l.length, data.rows);
    assert.ok(frame.l.every((line) => Number.isInteger(line) && line >= 0 && line < data.lines.length));
  }
  for (const [name, index] of Object.entries(data.marks)) assert.ok(validFrame(index), `Invalid mark ${name}`);
  assert.ok(data.chapters.length > 0);
  assert.equal(data.chapters[0].frame, 0);
  data.chapters.forEach((chapter, index) => {
    assert.ok(validFrame(chapter.frame) && chapter.frame < play);
    if (index) assert.ok(chapter.frame > data.chapters[index - 1].frame);
  });
  for (const { nodes } of documents.values()) {
    for (const node of nodes.filter((n) => attr(n, "data-frame") !== undefined)) {
      const name = attr(node, "data-frame");
      assert.ok(validFrame(data.marks[name] ?? Number(name)), `Unknown still frame ${name}`);
    }
  }
});
