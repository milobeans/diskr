import { createServer } from "node:http";
import { readFile, stat } from "node:fs/promises";
import path from "node:path";
import { site } from "./paths.mjs";

const types = { ".html": "text/html", ".css": "text/css", ".js": "text/javascript", ".json": "application/json", ".svg": "image/svg+xml", ".png": "image/png", ".webp": "image/webp" };
const port = Number(process.env.PORT || 4173);
createServer(async (request, response) => {
  try {
    const url = new URL(request.url, `http://127.0.0.1:${port}`);
    if (!url.pathname.startsWith("/diskr/")) throw new Error("outside site");
    let filename = path.resolve(site, decodeURIComponent(url.pathname.slice(7)));
    if (filename !== site && !filename.startsWith(site + path.sep)) throw new Error("outside site");
    if ((await stat(filename)).isDirectory()) filename = path.join(filename, "index.html");
    const body = await readFile(filename);
    response.writeHead(200, { "content-type": types[path.extname(filename)] || "application/octet-stream", "cache-control": "no-store" });
    response.end(body);
  } catch {
    response.writeHead(404, { "content-type": "text/plain" });
    response.end("Not found");
  }
}).listen(port, "127.0.0.1", () => console.log(`Website ready at http://127.0.0.1:${port}/diskr/`));
