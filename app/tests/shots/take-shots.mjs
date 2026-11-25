// Takes the README screenshots into ../docs/images/ (run from app/: `node tests/shots/take-shots.mjs`).
// Needs Ollama running with gemma4:latest for the local-chat pane. Set PLAYWRIGHT to a path
// if playwright isn't installed in this folder. Only the views named on the command line are taken.
import { spawn } from "node:child_process";
import { fileURLToPath } from "node:url";
import path from "node:path";

const { webkit } = await import(process.env.PLAYWRIGHT ?? "playwright");
const here = path.dirname(fileURLToPath(import.meta.url));
const app = path.resolve(here, "../..");
const out = path.resolve(app, "../docs/images");
const vite = path.join(app, "node_modules/.bin/vite");

const servers = [
  spawn(vite, ["--port", "1420", "--strictPort"], { cwd: app, stdio: "ignore" }),
  spawn(vite, ["--port", "5173", "--strictPort"], { cwd: path.join(app, "src-tauri/fixtures/demo"), stdio: "ignore" }),
];
const stop = () => servers.forEach((s) => s.kill());
process.on("exit", stop);
for (let i = 0; i < 60; i++) {
  try { await fetch("http://localhost:1420/tests/shots/shots.html"); await fetch("http://localhost:5173/"); break; } catch { await new Promise((r) => setTimeout(r, 250)); }
}

const browser = await webkit.launch();
const page = await browser.newPage({ viewport: { width: 1440, height: 900 }, deviceScaleFactor: 2, colorScheme: "dark" });
page.on("pageerror", (e) => console.error("page error:", e.message));
await page.goto("http://localhost:1420/tests/shots/shots.html");
await page.waitForTimeout(1200);

const wait = (ms) => page.waitForTimeout(ms);
const view = async (name) => { await page.click(`.rail button[data-view="${name}"]`); await wait(1200); };
const shot = async (name) => { await page.screenshot({ path: path.join(out, `${name}.png`) }); console.log("saved", name); };
const want = new Set(process.argv.slice(2));
const take = (n) => !want.size || want.has(n);

// Work: Claude Code beside a local model chat.
await page.keyboard.press("Meta+t"); await wait(1500);
if (take("main-window")) {
  await page.click("#new-btn"); await wait(300);
  await page.locator("#new-menu button", { hasText: "gemma4:latest" }).first().click(); await wait(800);
  await page.fill("#msg", "In two sentences: what should a bakery homepage show first on a phone?");
  await page.keyboard.press("Enter");
  await page.waitForSelector("text=your turn", { timeout: 120000 }).catch(() => {});
  await wait(1500);
  // Put Claude Code on the left of the chat.
  const tabs = page.locator("#tabs .tab:not(.add-tab)");
  const grid = await page.locator("#grid").boundingBox();
  const t = await tabs.nth(0).boundingBox();
  await page.mouse.move(t.x + t.width / 2, t.y + t.height / 2); await page.mouse.down();
  await page.mouse.move(t.x + t.width / 2 + 20, t.y + 30, { steps: 5 });
  await page.mouse.move(grid.x + 30, grid.y + grid.height / 2, { steps: 12 }); await wait(200); await page.mouse.up(); await wait(1500);
  await shot("main-window");
}
for (const [v, file] of [["changes", "changes"], ["guide", "guide"], ["bench", "bench"], ["preview", "preview"], ["savings", "savings"]]) {
  if (!take(file)) continue;
  await view(v);
  if (v === "preview") { await page.locator("text=http://localhost:5173/").first().click(); await wait(2500); }
  if (v === "changes") { await page.locator("text=index.html").first().click().catch(() => {}); await wait(800); }
  await shot(file);
}
await browser.close();
stop();
process.exit(0);
