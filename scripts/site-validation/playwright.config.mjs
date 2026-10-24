import { defineConfig } from "@playwright/test";
import path from "node:path";
import { output } from "./paths.mjs";

export default defineConfig({
  testDir: ".",
  testMatch: "browser.spec.mjs",
  fullyParallel: false,
  workers: 1,
  forbidOnly: !!process.env.CI,
  retries: 0,
  timeout: 45_000,
  globalTimeout: 10 * 60_000,
  outputDir: path.join(output, "results"),
  reporter: [["list"], ["html", { outputFolder: path.join(output, "report"), open: "never" }]],
  use: {
    browserName: "chromium",
    baseURL: "http://127.0.0.1:4173/diskr/",
    viewport: { width: 1440, height: 1000 },
    reducedMotion: "reduce",
    trace: "retain-on-failure",
    screenshot: "only-on-failure",
  },
  webServer: {
    command: "node serve.mjs",
    url: "http://127.0.0.1:4173/diskr/",
    reuseExistingServer: false,
    timeout: 15_000,
  },
});
