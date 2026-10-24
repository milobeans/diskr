import { test, expect } from "@playwright/test";

const pages = ["./", "docs/"];
const errors = new WeakMap();

test.beforeEach(async ({ page }) => {
  const problems = [];
  errors.set(page, problems);
  page.on("pageerror", (error) => problems.push(error.message));
  page.on("response", (response) => {
    if (response.url().startsWith("http://127.0.0.1:4173/") && response.status() >= 400) {
      problems.push(`${response.status()} ${response.url()}`);
    }
  });
  // Browser checks use system fallback fonts and never depend on a third-party CDN.
  await page.route("https://fonts.bunny.net/**", (route) => route.fulfill({ contentType: "text/css", body: "" }));
});

test.afterEach(async ({ page }) => {
  expect(errors.get(page), "Browser errors and missing deployment assets").toEqual([]);
});

for (const url of pages) {
  test(`${url} search contains focus, closes and restores its trigger`, async ({ page }) => {
    await page.goto(url);
    const trigger = page.getByRole("button", { name: "Search docs", exact: true });
    const dialog = page.getByRole("dialog", { name: "Search documentation" });
    await trigger.click();
    await expect(dialog).toBeVisible();
    const input = dialog.getByRole("searchbox", { name: "Search the docs" });
    await expect(input).toBeFocused();
    await input.fill("reclaim");
    await expect.poll(() => dialog.locator(".palette-results a").count()).toBeGreaterThan(0);
    const stops = await dialog.locator("input, button, a[href]").count();
    for (let i = 0; i < stops + 2; i++) {
      await page.keyboard.press("Tab");
      expect(await dialog.evaluate((node) => node.contains(document.activeElement)), "Tab must remain inside search").toBe(true);
    }
    for (let i = 0; i < stops + 2; i++) {
      await page.keyboard.press("Shift+Tab");
      expect(await dialog.evaluate((node) => node.contains(document.activeElement)), "Shift+Tab must remain inside search").toBe(true);
    }
    await input.focus();
    await expect(input).toHaveValue("reclaim");
    await page.keyboard.press("Escape");
    await expect(dialog).toBeHidden();
    await expect(trigger).toBeFocused();
    await trigger.click();
    await input.fill("no-matching-section-492827");
    await expect(dialog.locator(".palette-empty")).toBeVisible();
    await page.keyboard.press("Shift+Tab");
    expect(await dialog.evaluate((node) => node.contains(document.activeElement))).toBe(true);
    await dialog.getByRole("button", { name: "Close search", exact: true }).click();
    await expect(dialog).toBeHidden();
    await expect(trigger).toBeFocused();
  });

  test(`${url} Copy writes exactly the displayed command including comments`, async ({ page }) => {
    await page.addInitScript(() => {
      window.copiedPayloads = [];
      Object.defineProperty(navigator, "clipboard", {
        configurable: true,
        value: { writeText: async (value) => { window.copiedPayloads.push(value); } },
      });
    });
    await page.goto(url);
    for (const disclosure of await page.locator("details").all()) {
      if (await disclosure.getAttribute("open") === null) await disclosure.locator(":scope > summary").click();
    }
    const checked = new Set();
    let count = 0;
    const checkVisible = async () => {
      for (const button of await page.locator("button.copy:visible").all()) {
        const expected = await button.evaluate((node) => (node.dataset.copy ?? node.closest(".code")?.querySelector("pre")?.innerText ?? "").trim());
        if (checked.has(expected)) continue;
        checked.add(expected);
        const initialLabel = await button.getAttribute("aria-label");
        const mutation = await button.evaluate((node) => !!node.closest('[data-copy-kind="mutation"]'));
        if (!mutation) expect(expected).not.toMatch(/(?:^|\s)--yes(?:\s|$)/);
        await button.click();
        count++;
        await expect.poll(() => page.evaluate(() => window.copiedPayloads.length)).toBe(count);
        expect(await page.evaluate(() => window.copiedPayloads.at(-1))).toBe(expected);
        if (mutation) await expect(button).toHaveAttribute("aria-label", initialLabel, { timeout: 5000 });
      }
    };
    await checkVisible();
    for (const tab of await page.getByRole("tab").all()) {
      await tab.click();
      await checkVisible();
    }
    expect(count).toBeGreaterThan(2);
    if (url === "docs/") {
      expect(checked.has("# Preview only; no snapshots are removed\ndiskr --thin-snapshots 10G ~")).toBe(true);
      expect(checked.has("# Apply: request permanent local snapshot removal\ndiskr --thin-snapshots 10G --yes ~")).toBe(true);
    }
  });

  for (const theme of ["light", "dark"]) {
    test(`${url} secondary text meets normal-text contrast in ${theme} theme`, async ({ page }) => {
      await page.addInitScript((value) => localStorage.setItem("diskr-theme", value), theme);
      await page.goto(url);
      const measure = async (selector) => page.locator(selector).evaluateAll((nodes) => {
        const canvas = document.createElement("canvas");
        canvas.width = canvas.height = 1;
        const context = canvas.getContext("2d", { willReadFrequently: true });
        const color = (value) => {
          context.clearRect(0, 0, 1, 1);
          context.fillStyle = value;
          context.fillRect(0, 0, 1, 1);
          return [...context.getImageData(0, 0, 1, 1).data].map((n, index) => index === 3 ? n / 255 : n);
        };
        const over = (front, back) => front.slice(0, 3).map((n, index) => n * front[3] + back[index] * (1 - front[3]));
        const luminance = (rgb) => rgb.map((n) => n / 255).map((n) => n <= 0.04045 ? n / 12.92 : ((n + 0.055) / 1.055) ** 2.4).reduce((sum, n, index) => sum + n * [0.2126, 0.7152, 0.0722][index], 0);
        return nodes.filter((node) => node.getBoundingClientRect().width > 0 && node.getBoundingClientRect().height > 0 && getComputedStyle(node).visibility !== "hidden").map((node) => {
          const ancestors = [];
          for (let parent = node; parent; parent = parent.parentElement) ancestors.unshift(parent);
          let background = [255, 255, 255];
          for (const parent of ancestors) background = over(color(getComputedStyle(parent).backgroundColor), background);
          const foreground = over(color(getComputedStyle(node).color), background);
          const values = [luminance(foreground), luminance(background)].sort((a, b) => b - a);
          return { text: node.textContent.trim().slice(0, 70), ratio: (values[0] + 0.05) / (values[1] + 0.05) };
        });
      });
      const body = await measure(".caption, .facts, .label, .toc a");
      expect(body.length).toBeGreaterThan(0);
      for (const sample of body) expect(sample.ratio, `${sample.text} contrast in ${theme}`).toBeGreaterThanOrEqual(4.5);
      await page.getByRole("button", { name: "Search docs", exact: true }).click();
      await expect.poll(() => page.locator(".palette-results a").count()).toBeGreaterThan(0);
      await page.getByRole("searchbox", { name: "Search the docs" }).fill("reclaim");
      await expect.poll(() => page.locator(".palette-results mark").count()).toBeGreaterThan(0);
      const search = await measure(".palette-results .s, .palette-results .crumb, .palette-results mark");
      expect(search.length).toBeGreaterThan(0);
      for (const sample of search) expect(sample.ratio, `${sample.text} search contrast in ${theme}`).toBeGreaterThanOrEqual(4.5);
      await page.keyboard.press("Escape");
      await page.getByRole("button", { name: "Toggle color theme", exact: true }).click();
      await expect(page.locator("html")).toHaveAttribute("data-theme", theme === "light" ? "dark" : "light");
    });
  }

  test(`${url} narrow layout contains content and exposes terminal panning`, async ({ page }) => {
    await page.setViewportSize({ width: 390, height: 844 });
    await page.goto(url);
    await expect.poll(() => page.locator(".figure canvas").first().evaluate((canvas) => canvas.width)).toBeGreaterThan(300);
    expect(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth + 1)).toBe(true);
    await expect(page.locator(".pan-hint").first()).toBeVisible();
    const screen = page.locator(".figure .screen").first();
    await screen.scrollIntoViewIfNeeded();
    await expect(screen).toHaveAttribute("tabindex", "0");
    expect(await screen.evaluate((node) => node.scrollWidth > node.clientWidth)).toBe(true);
    await screen.focus();
    await page.keyboard.press("ArrowRight");
    await expect.poll(() => screen.evaluate((node) => node.scrollLeft)).toBeGreaterThan(0);
  });
}

test("demo fetches real frames and responds to playback and frame-step controls", async ({ page }) => {
  await page.goto("./");
  const demo = page.locator("[data-replay]");
  await expect.poll(() => demo.locator(".segment").count()).toBeGreaterThan(0);
  const canvas = demo.locator("canvas");
  expect(await canvas.evaluate((node) => node.width > 0 && node.height > 0)).toBe(true);
  expect(await canvas.evaluate((node) => {
    const pixels = node.getContext("2d").getImageData(0, 0, node.width, node.height).data;
    for (let i = 4; i < pixels.length; i += 4) if (pixels[i] !== pixels[0] || pixels[i + 1] !== pixels[1] || pixels[i + 2] !== pixels[2]) return true;
    return false;
  }), "Rendered frame must contain terminal content").toBe(true);
  await demo.scrollIntoViewIfNeeded();
  await demo.focus();
  const progress = () => demo.locator(".segment").first().evaluate((node) => Number(node.style.getPropertyValue("--p")));
  const before = await progress();
  await page.keyboard.press("ArrowRight");
  await expect.poll(progress).toBeGreaterThan(before);
  await page.keyboard.press("Space");
  await expect(demo.getByRole("button", { name: "Pause demo", exact: true })).toBeVisible();
  await page.keyboard.press("Space");
  await expect(demo.getByRole("button", { name: "Play demo", exact: true })).toBeVisible();
  const first = demo.locator(".segment").first();
  const second = demo.locator(".segment").nth(1);
  await second.focus();
  await page.keyboard.press("Enter");
  await expect(second).toHaveClass(/active/);
  await first.focus();
  await page.keyboard.press("Space");
  await expect(first).toHaveClass(/active/);
  const play = demo.locator("button.play");
  await play.focus();
  await page.keyboard.press("Space");
  await expect(play).toHaveAttribute("aria-label", "Pause demo");
  await page.keyboard.press("Space");
  await expect(play).toHaveAttribute("aria-label", "Play demo");
});
