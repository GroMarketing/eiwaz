// README screenshot harness: the real frontend with a mocked Tauri backend, on a demo bakery project.
// Bench and Guide use real data (bench.json = real Model Bench results, guide-stripe.json = real
// Plado catalog entries). Work, Changes and Savings use demo activity. Ollama answers for real.
// Run `node tests/shots/take-shots.mjs` (it starts the dev servers itself).
import { mockIPC, mockWindows } from "@tauri-apps/api/mocks";
import { emit } from "@tauri-apps/api/event";
import bench from "./bench.json";
import guide from "./guide-stripe.json";

const PROJECT = "/Users/you/Sites/corner-bakery";
const SITE = "http://localhost:5173/";
mockWindows("main");
try { localStorage.setItem("project", PROJECT); } catch { /* ignore */ }

const now = Math.floor(Date.now() / 1000);
const enc = (s: string) => btoa(String.fromCharCode(...new TextEncoder().encode(s)));

// ---- Work: a Claude Code session showing the split line ----
const g = "\x1b[90m", b = "\x1b[1m", y = "\x1b[33m", c = "\x1b[36m", gr = "\x1b[32m", r = "\x1b[0m", o = "\x1b[38;5;209m";
const CLAUDE = [
  `${o}╭────────────────────────────────────────────╮${r}`,
  `${o}│${r} ${o}✻${r} ${b}Welcome to Claude Code${r}                     ${o}│${r}`,
  `${o}│${r}   ${g}cwd: ~/Sites/corner-bakery${r}               ${o}│${r}`,
  `${o}╰────────────────────────────────────────────╯${r}`,
  ``,
  `${g}>${r} Add weekend hours to the homepage and make the order button easier to tap on phones`,
  ``,
  `${c}Split · Mac: summarize index.html and style.css, digest the test run · Cloud: add the hours section, fix the button${r}`,
  ``,
  `${gr}⏺${r} ${b}eiwaz - read_and_summarize${r} (path: ".", focus: "homepage layout and button styles")`,
  `  ${g}⎿  index.html: header (logo, h1), main (h2, p, a.btn → #order). style.css: .btn 32px tall, no media queries.${r}`,
  ``,
  `${gr}⏺${r} ${b}Update${r}(index.html)`,
  `  ${g}⎿  Added 8 lines: <section class="hours"> with Sat–Sun 7 am – 2 pm${r}`,
  ``,
  `${gr}⏺${r} ${b}Update${r}(style.css)`,
  `  ${g}⎿  .btn min-height 48px, full width under 480px${r}`,
  ``,
  `${gr}⏺${r} ${b}Bash${r}(npm test)`,
  `  ${g}⎿  412 lines of output${r}`,
  ``,
  `${gr}⏺${r} ${b}eiwaz - digest_output${r} (412 lines)`,
  `  ${g}⎿  All 14 tests pass. 1 warning: img/logo.png has no width on small screens (style.css:3).${r}`,
  ``,
  `${gr}⏺${r} Weekend hours are on the homepage, and the order button is now 48px tall and full width on phones.`,
  ``,
  `${c}Split result · Mac: read 2 files, digested 412 lines of test output · Cloud: wrote the hours section and button fix${r}`,
  ``,
  `${g}>${r} `,
].join("\r\n");
let nextId = 1;
async function spawnSession(a: any) {
  const id = nextId++;
  const text = /claude/.test(String(a.startup)) ? CLAUDE : `${g}~/Sites/corner-bakery${r} $ `;
  setTimeout(() => emit("pty-output", { id, data: enc(text) }), 400);
  return id;
}

// ---- Bench: real results; the env lists the same models ----
const env = {
  on_battery: false, mem_total: 51539607552, mem_available: 30e9, gpu_limit: 38654705664, chip: "Apple M4 Pro", running: false,
  ollama_up: true, lmstudio_up: false,
  models: Object.values(bench as Record<string, any>).map((m) => ({ name: m.model, size: m.file_bytes, details: { parameter_size: m.params, quantization_level: m.quant, family: m.family } })),
};

// ---- Changes: two save points and the edit from the Work session ----
const DIFF_HTML = `diff --git a/index.html b/index.html
--- a/index.html
+++ b/index.html
@@ -8,6 +8,14 @@
     <h2>Fresh bread every morning</h2>
     <p>Sourdough, rye and baguettes, baked before 7 am.</p>
     <a class="btn" href="#order">Order for pickup</a>
+    <section class="hours">
+      <h3>Weekend hours</h3>
+      <ul>
+        <li>Saturday: 7 am – 2 pm</li>
+        <li>Sunday: 7 am – 2 pm</li>
+      </ul>
+      <p>Sold out early? Order ahead for pickup.</p>
+    </section>
   </main>
 </body>
 </html>
`;
const DIFF_CSS = `diff --git a/style.css b/style.css
--- a/style.css
+++ b/style.css
@@ -10,7 +10,13 @@
 .btn {
   display: inline-block;
-  padding: 6px 14px;
+  min-height: 48px;
+  padding: 12px 20px;
   border-radius: 8px;
   background: #8a4b2a;
   color: #fff;
 }
+@media (max-width: 480px) {
+  .btn { display: block; text-align: center; }
+}
+.hours { margin-top: 2rem; }
`;
const POINTS = [
  { id: "sp3", ts: now - 300, label: "Weekend hours + bigger order button", agent: "Claude Code", files: 2 },
  { id: "sp2", ts: now - 3600 * 3, label: "Add the logo and page title", agent: "Claude Code", files: 3 },
  { id: "sp1", ts: now - 86400, label: "Before agents started", agent: "Eiwaz", files: 5 },
];
const CHANGES = [
  { path: "index.html", status: "changed", added: 8, removed: 0, agents: ["Claude Code"] },
  { path: "style.css", status: "changed", added: 8, removed: 1, agents: ["Claude Code"] },
];

// ---- Savings: demo activity for the bakery project ----
const RUNS = [
  { ts: now - 600, project: PROJECT, task: "agent", model: "gemma4:latest", prompt_tokens: 18240, output_tokens: 310, seconds: 14.2 },
  { ts: now - 560, project: PROJECT, task: "agent", model: "gemma4:latest", prompt_tokens: 9620, output_tokens: 140, seconds: 6.8 },
  { ts: now - 86400, project: PROJECT, task: "write", model: "gemma4:latest", prompt_tokens: 2400, output_tokens: 1850, seconds: 31.5 },
  { ts: now - 86400 * 2, project: PROJECT, task: "summary", model: "gemma4:latest", prompt_tokens: 6100, output_tokens: 220, seconds: 5.9 },
];
const msg = (ts: number, preview: string, m: Partial<Record<string, number | boolean>>) => ({
  ts, preview, words: preview.split(" ").length, turns: 6, input: 4200, output: 1900, context_end: 38000, tool_tokens: 900, tool_calls: 7,
  local_calls: 2, local_read: 27860, subagents: 0, split_line: true, split_result: true, ...m,
});
const ROUTES = [
  { route: { ts: (now - 600) * 1000, note: "on", heavy: true, agent: "Claude Code", from: "message box" },
    msg: msg((now - 600) * 1000, "Add weekend hours to the homepage and make the order button easier to tap on phones", {}) },
  { route: { ts: (now - 86400) * 1000, note: "on", heavy: true, agent: "Claude Code", from: "message box" },
    msg: msg((now - 86400) * 1000, "Write an About page about the bakery's story and link it from the header", { local_calls: 1, local_read: 2400, turns: 4, input: 3100, output: 1200, tool_tokens: 400, context_end: 31000 }) },
  { route: { ts: (now - 86400 * 2) * 1000, split: "cloud", agent: "Claude Code", from: "message box" },
    msg: msg((now - 86400 * 2) * 1000, "Fix the typo in the footer", { local_calls: 0, local_read: 0, turns: 2, input: 1300, output: 220, tool_tokens: 150, tool_calls: 2, context_end: 22000, split_line: false, split_result: false }) },
];

const STORE: Record<string, unknown> = { bench, savings: RUNS };

mockIPC(async (cmd, a: any) => {
  switch (cmd) {
    case "sessions_sweep": return { unclean: false, ended: [], summary: "" };
    case "detect_tools": return { claude: "/usr/local/bin/claude" };
    case "ollama_status": return { up: true, loaded: [{ name: "gemma4:latest", size: 9.6e9 }], installed: env.models.map((m) => m.name) };
    case "system_stats": return { mem_used: 21e9, mem_total: env.mem_total, claude_mem: 0.6e9, codex_mem: 0, ollama_mem: 9.6e9 };
    case "store_read": return STORE[a.name] ?? null;
    case "store_write": STORE[a.name] = a.value; return null;
    case "spawn_session": return spawnSession(a);
    case "local_mcp_config": return { config: "/tmp/eiwaz-mcp.json", rules: "/tmp/eiwaz-rules.md" };
    case "claude_title": return "Weekend hours + order button";
    case "local_mcp_events": return { events: [], offset: 0 };
    case "chat_plan": return { max_ctx: 131072, safe_ctx: 131072, vision: true };
    case "chat_send": return chatSend(a);
    case "chat_stop": return null;
    case "bench_env": return env;
    case "guide_scan": return {
      signals: [{ integration: "stripe", evidence: "package.json: stripe" }], stack: ["JavaScript / Node", "Vite"],
      has_claude_md: false, has_agents_md: false, installed: [], skills: 0, subagents: 0, git_remote: "",
    };
    case "guide_catalog": return { picks: (guide as any).picks, built_at: (guide as any).builtAt, fetched_at: now - 3600, offline: false, error: "" };
    case "sp_list": return POINTS;
    case "sp_changes": return CHANGES;
    case "sp_diff": return a.path === "index.html" ? DIFF_HTML : a.path === "style.css" ? DIFF_CSS : DIFF_HTML + DIFF_CSS;
    case "sp_shot": return null;
    case "sp_unsaved": return [];
    case "route_report": return ROUTES;
    case "preview_serve": return "";
    case "preview_probe": return { servers: [{ port: 5173, url: SITE, command: "npm run dev", from_project: true }], start_command: "npm run dev", files: ["index.html", "PLAN.md", "README.md", "img/logo.png"] };
    case "preview_port_up": return true;
    case "estimate_request": return null;
    default: return null;
  }
}, { shouldMockEvents: true });

// Local chat answers through the real Ollama, so the reply in the screenshot is genuine.
async function chatSend(a: any) {
  await emit("chat-status", { id: a.id, text: "reading" });
  const res = await fetch("http://127.0.0.1:11434/api/chat", {
    method: "POST", body: JSON.stringify({ model: a.model, messages: a.messages, stream: false, think: false, options: { num_ctx: 8192 } }),
  });
  const v = await res.json();
  await emit("chat-delta", { id: a.id, content: v.message?.content ?? "", thinking: "" });
  await emit("chat-done", { id: a.id, ok: true, stopped: false, error: "", prompt_tokens: v.prompt_eval_count ?? 0, reply_tokens: v.eval_count ?? 0 });
  return null;
}
