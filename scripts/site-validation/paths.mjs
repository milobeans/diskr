import { fileURLToPath } from "node:url";
import path from "node:path";

export const repository = fileURLToPath(new URL("../../", import.meta.url));
export const output = path.join(repository, "target", "site-validation");
export const site = path.join(output, "site");
export const origin = "https://milobeans.github.io";
export const base = `${origin}/diskr/`;
