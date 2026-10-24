import { cp, mkdir, readdir, rm } from "node:fs/promises";
import path from "node:path";
import { repository, site } from "./paths.mjs";

await rm(site, { recursive: true, force: true });
await cp(path.join(repository, "site"), site, {
  recursive: true,
  filter: (source) => source !== path.join(repository, "site", "assets", "img"),
});
const images = path.join(site, "assets", "img");
await mkdir(images, { recursive: true });
for (const name of await readdir(path.join(repository, ".github", "assets"))) {
  if (/\.(png|webp)$/.test(name)) {
    await cp(path.join(repository, ".github", "assets", name), path.join(images, name));
  }
}
console.log(`Assembled website in ${path.relative(repository, site)}`);
