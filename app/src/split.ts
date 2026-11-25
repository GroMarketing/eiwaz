// Splitting token-heavy requests between Claude Code (cloud) and this Mac.
//
// - Bench labels for local models in the "+ New" menu, and a chat-only notice.
// - Claude Code starts with Eiwaz's read-only "do it on this Mac" tools.
// - One quiet line above the message box estimates what a request sends to the
//   cloud; for heavy ones it offers a split plan (a small popover).
// - Steps done on this Mac show in the status bar and count in Savings.
import { invoke } from "@tauri-apps/api/core";
import { db, el, esc, kfmt, local, toast } from "./core";
import * as savings from "./views/savings";

export type Label = { cls: "ok" | "warn" | "bad"; text: string; why: string };
interface Step { what: string; where: "mac" | "cloud"; why: string }
interface Estimate { message: number; files: { path: string; tokens: number }[]; total: number }

const st = {
  labels: {} as Record<string, Label>,
  bench: {} as Record<string, any>,
  est: null as Estimate | null,
  estFor: "",
  mode: (local.get("split:mode") as "split" | "cloud") || "split",
  plan: null as Step[] | null,
  planFor: "",
  planning: false,
  open: false,
  offset: -1,
  saved: 0,
  steps: 0,
  last: "",
};

export const threshold = () => Number(local.get("split:threshold")) || 20000;

// ---------- Bench labels ----------

export async function refreshLabels() {
  st.bench = await db.read<Record<string, any>>("bench", {});
  const out: Record<string, Label> = {};
  for (const [name, r] of Object.entries(st.bench)) {
    const inj = (r.injection ?? []) as { followed: boolean }[];
    const bad = inj.filter((c) => c.followed).length;
    const fileBad = (r.checks ?? []).some((c: any) => c.status === "bad");
    if (fileBad) out[name] = { cls: "bad", text: "Don't use · failed file check", why: "Bench found a problem with the model file" };
    else if (bad) out[name] = { cls: "bad", text: "Chat only · failed security test", why: `Obeyed instructions planted in files ${bad} of ${inj.length} times in Bench` };
    else if (r.verdict === "good") out[name] = { cls: "ok", text: "✓ Good for small jobs", why: `Passed Bench${r.scores?.compatibility != null ? ` (compatibility ${r.scores.compatibility}/100)` : ""}, ignored all ${inj.length} planted instructions it was tested with` };
    else out[name] = { cls: "warn", text: "Chat only", why: verdictWhy(r.verdict) };
  }
  st.labels = out;
}

function verdictWhy(v: string) {
  return ({ forgets: "Forgets long instructions", tools: "Weak at tool calls", notools: "Can't use tools", slow: "Slow on big prompts", toobig: "Too big for this Mac", failed: "Bench didn't finish" } as Record<string, string>)[v] ?? "See Bench";
}

export const label = (model: string): Label => st.labels[model] ?? { cls: "warn", text: "Not tested", why: "Test it in Bench first" };

/** Shown in a local model's terminal before it starts: Eiwaz runs local models as chat only. */
export function chatOnlyNotice(model: string) {
  const l = label(model);
  const color = l.cls === "bad" ? "31" : l.cls === "ok" ? "32" : "33";
  const head = l.cls === "ok" ? "Chat only, like every local model in Eiwaz." : `Chat only. ${l.why}.`;
  return `\x1b[${color}m● ${head}\x1b[0m\x1b[90m Eiwaz doesn't give local models tools, so it can't run commands or change files. Use Claude Code for that.\x1b[0m\r\n\r\n`;
}

const ACTION_RE = /\b(run|fix|edit|change|update|delete|remove|rename|install|commit|push|deploy|refactor|implement|create (a |the )?(file|folder|component)|npm|pnpm|yarn|git|rm|mkdir)\b/i;
export const looksLikeAction = (text: string) => ACTION_RE.test(text);

// ---------- Claude Code with local tools ----------

// The rules Claude Code starts with. Only tools a local model passed Bench for are offered.
function rulesFor(cats: Set<string>): string {
  const tools = [cats.has("summarize") && "read_and_summarize (files and folders)", cats.has("write") && "write_draft (writes prose into a file)", cats.has("digest") && "digest_output (long command output)"].filter(Boolean).join(", ");
  const mac = [cats.has("summarize") && "reading, searching, summarizing", cats.has("digest") && "digesting output", cats.has("write") && "writing prose"].filter(Boolean).join(", ");
  return [
    `Eiwaz splits work between you (cloud) and local models on this Mac, which cost nothing. They are behind the eiwaz tools: ${tools}. Each tool runs on the model that scored best for that kind of job in Eiwaz's reliability tests. Work the tools don't cover stays with you. These rules apply to every task, and to any subagents you start:`,
    `1. At the start of any task with more than one step, write one line before anything else, in this form: 'Split · Mac: <steps the local models will do> · Cloud: <steps you will do>'. Mac steps are only: ${mac}. Code, commands, browser actions, and decisions are Cloud steps.`,
    cats.has("write") ? "W. Writing: when a task needs prose longer than about 150 words (emails and email sequences, newsletters, landing-page or ad copy, posts, docs, READMEs, plans), do not write it yourself. Call write_draft with a precise brief (audience, goal, tone, length, structure, and the exact facts, names, links, prices and offers to include) and reference_paths to files that show the voice and facts. Do not read the result back in full; if you need to check it, call read_and_summarize on it with a focus on accuracy and tone, then make small fixes with Edit. If the copy must live inside code (a template in .ts or .js), have write_draft write it to an .html or .md file next to it and load it from there, rather than pasting it into the code. Short text (commit messages, a sentence or two, Linear comments) you write yourself." : "",
    cats.has("summarize") ? "2. Before reading any file over about 300 lines, more than 3 files, or a whole folder, call read_and_summarize with a clear focus and work from its brief. Read exact lines yourself only when you need to edit them or the brief is not enough." : "",
    cats.has("digest") ? "3. Pass test, build, and log output longer than about 100 lines to digest_output instead of reading all of it." : "",
    "4. When you finish a task, end with one line: 'Split result · Mac: <what the local model did, or none> · Cloud: <what you did>'.",
    "5. If the message says Eiwaz split is off for this request, skip the eiwaz tools and the Split lines.",
  ].filter(Boolean).join("\n");
}

/** The command that starts Claude Code, with the local tools when a model passed Bench. */
export async function claudeStartup(project: string, sessionId: string, resume = false): Promise<{ cmd: string; local: string }> {
  // Eiwaz names the session so it can find Claude Code's log for it (Savings → Messages).
  // After a restart, the same id resumes the conversation.
  const base = `clear && claude ${resume ? "--resume" : "--session-id"} ${sessionId}`;
  if (savings.neverLocal(project)) return { cmd: base, local: "" };
  const models = await savings.bestModels();
  const cats = new Set(Object.keys(models));
  if (!cats.size) return { cmd: base, local: "" };
  const spec = Object.entries(models).map(([c, m]) => `${c}=${m}`).join(",");
  try {
    const cfg = await invoke<{ config: string; rules: string }>("local_mcp_config", { project, model: spec, rules: rulesFor(cats) });
    const q = (s: string) => `'${s.replace(/'/g, `'\\''`)}'`;
    // Keep this line short: a terminal blocks on a line over about 1,024 characters.
    return { cmd: `${base} --mcp-config ${q(cfg.config)} --allowedTools mcp__eiwaz --append-system-prompt-file ${q(cfg.rules)}`, local: [...new Set(Object.values(models))].join(" + ") };
  } catch {
    return { cmd: base, local: "" };
  }
}

/** Make sure the estimate matches the text about to be sent (a fast Enter can beat the typing delay). */
export async function ensureEstimate() {
  if (!ctx) return;
  const text = ctx.text();
  const t = ctx.target();
  if (!t || t.kind !== "claude" || st.estFor === text) return;
  clearTimeout(timer);
  try { st.est = await invoke<Estimate>("estimate_request", { project: ctx.project(), text, attachments: ctx.attachments() }); st.estFor = text; } catch { /* keep going without it */ }
}

/** What Eiwaz decided for the message being sent (logged with it; never the text). */
export function routeInfo(text: string) {
  const t = ctx?.target();
  const suffix = sendSuffix();
  return {
    est_tokens: st.est?.total ?? null,
    est_files: st.est?.files.length ?? 0,
    heavy: heavy(),
    split: !t?.localModel ? "unavailable" : st.mode,
    note: suffix ? (suffix.includes("split plan") ? "plan" : suffix.includes("off") ? "off" : "on") : "none",
    plan_mac: st.plan?.filter((p) => p.where === "mac").length ?? 0,
    plan_cloud: st.plan?.filter((p) => p.where === "cloud").length ?? 0,
    words: text.trim().split(/\s+/).filter(Boolean).length,
  };
}

// ---------- the quiet line above the message box ----------

let timer = 0;
let ctx: { project: () => string; text: () => string; attachments: () => string[]; target: () => { kind: string; localModel?: string } | undefined } | null = null;

export function initStrip(c: typeof ctx) {
  ctx = c;
  document.addEventListener("click", (e) => {
    const pop = document.getElementById("split-pop");
    if (st.open && pop && !pop.contains(e.target as Node) && !(e.target as HTMLElement).closest("#split-strip")) { st.open = false; draw(); }
  });
}

/** Call when the message, attachments or target change. */
export function update() {
  clearTimeout(timer);
  timer = window.setTimeout(async () => {
    if (!ctx) return;
    const text = ctx.text();
    const t = ctx.target();
    if (!t || t.kind !== "claude" || (!text.trim() && !ctx.attachments().length)) { st.est = null; st.open = false; draw(); return; }
    try { st.est = await invoke<Estimate>("estimate_request", { project: ctx.project(), text, attachments: ctx.attachments() }); } catch { st.est = null; }
    st.estFor = text;
    if (st.planFor && st.planFor !== text) { st.plan = null; st.planFor = ""; }
    draw();
  }, 350);
}

function heavy() { return !!st.est && st.est.total >= threshold(); }

function draw() {
  const strip = document.getElementById("split-strip");
  if (!strip || !ctx) return;
  const t = ctx.target();
  const e = st.est;
  if (!e || !t || t.kind !== "claude") { strip.hidden = true; drawPop(); return; }
  strip.hidden = false;
  const isHeavy = heavy();
  strip.className = `split-strip${isHeavy ? " heavy" : ""}`;
  const canSplit = !!t.localModel;
  const planNote = st.plan ? `${st.plan.filter((s) => s.where === "mac").length} of ${st.plan.length} steps on this Mac` : "bulky reading can run on this Mac";
  strip.innerHTML = `<span class="sic" aria-hidden="true">${isHeavy ? "◆" : "·"}</span>
    <span class="stxt"><b>≈${kfmt(e.total)} tokens</b> in your message${isHeavy ? (canSplit ? ` · <span class="lc">${planNote}</span>` : ` · <span class="hint">restart Claude Code to use this Mac for the reading</span>`) : canSplit ? (st.mode === "split" ? ` · <span class="lc">split on: Claude will say which steps run on this Mac</span>` : ` · all cloud`) : ` · <span class="hint">this Claude Code started without the local tools; start a new one to split</span>`}</span>
    <span class="grow"></span>
    ${canSplit ? `<button class="linkbtn" id="split-review" aria-expanded="${st.open}">${st.open ? "Close" : "Review split"}</button>
      <span class="seg2" role="group" aria-label="Where this request runs"><button class="${st.mode === "split" ? "on" : ""}" data-mode="split" title="Do the bulky reading on this Mac">Split</button><button class="${st.mode === "cloud" ? "on" : ""}" data-mode="cloud" title="Send everything to Claude Code">All cloud</button></span>` : ""}`;
  strip.title = `Counts the message, attachments and the project files it names (${e.files.map((f) => `${f.path} ≈${kfmt(f.tokens)}`).join(", ") || "none named"}). The agent usually reads more on its own, so this is a floor.`;
  document.getElementById("split-review")?.addEventListener("click", (ev) => { ev.stopPropagation(); st.open = !st.open; if (st.open && !st.plan) makePlan(); draw(); });
  strip.querySelectorAll<HTMLButtonElement>("[data-mode]").forEach((b) => (b.onclick = () => { st.mode = b.dataset.mode as any; local.set("split:mode", st.mode); draw(); }));
  drawPop();
}

function drawPop() {
  let pop = document.getElementById("split-pop");
  if (!st.open || !st.est) { pop?.remove(); return; }
  if (!pop) {
    pop = document.createElement("div");
    pop.id = "split-pop";
    pop.className = "split-pop";
    el("split-strip").before(pop);
  }
  const steps = st.plan;
  pop.innerHTML = `<div class="pophead"><b>Split plan</b><span class="hint">${st.planning ? "planning on this Mac…" : steps ? `about ${kfmt(st.est.total)} tokens → much less sent to the cloud` : ""}</span></div>
    ${st.planning ? `<p class="hint"><span class="spinner"></span> The local model is listing the steps.</p>` : ""}
    ${steps ? `<ol class="psteps">${steps.map((s, i) => `<li><button class="where ${s.where}" data-i="${i}" title="${s.where === "mac" ? "Runs on this Mac. Click to send it to the cloud." : canMove(s) ? "Runs in the cloud. Click to run it on this Mac." : "Only reading and prose-writing steps can run on this Mac."}">${s.where === "mac" ? "Mac" : "Cloud"}</button><span><b>${esc(s.what)}</b><span class="hint"> · ${esc(s.why)}</span></span></li>`).join("")}</ol>` : ""}
    <div class="popfoot"><label class="hint">Show this for requests over <input id="split-th" type="number" min="1000" step="1000" value="${threshold()}"> tokens</label><span class="grow"></span>${steps ? `<button class="linkbtn" id="split-replan">Plan again</button>` : ""}</div>
    <p class="hint">Mac steps read, summarize, and write prose (emails, copy, docs) into text files. Code, commands and browser work stay with Claude Code, which asks you first.</p>`;
  pop.querySelectorAll<HTMLButtonElement>("button.where").forEach((b) => (b.onclick = (ev) => {
    ev.stopPropagation();
    const s = st.plan![Number(b.dataset.i)];
    if (s.where === "cloud" && !canMove(s)) return toast("Only reading, summarizing and writing prose can run on this Mac. This one stays with Claude Code.");
    s.where = s.where === "mac" ? "cloud" : "mac";
    draw();
  }));
  const th = pop.querySelector<HTMLInputElement>("#split-th")!;
  th.onchange = () => { local.set("split:threshold", String(Math.max(1000, Number(th.value) || 20000))); draw(); };
  pop.querySelector("#split-replan")?.addEventListener("click", (ev) => { ev.stopPropagation(); st.plan = null; makePlan(); });
}

const ACTS = /\b(edit|change|write|modify|update|fix|create|delete|remove|rename|install|run|execute|commit|push|deploy|apply|refactor|implement|add)\b/i;
// Judge a step by its opening verb, so file names (like GTM-PLAN.md) can't trip the rules.
const verb = (s: Step) => (s.what.trim().match(/^[A-Za-z]+/)?.[0] ?? "").toLowerCase();
const words = (s: Step) => s.what.split(/\s+/).filter((w) => !/[./]/.test(w)).join(" ");
// Only reading-type steps may run on this Mac, and only if nothing in them acts.
const PROSE_VERB = /^(write|draft|compose|rewrite|redraft|polish|outline)$/;
const PROSE_NOUN = /\b(emails?|sequences?|newsletters?|copy|copywriting|subject lines?|posts?|articles?|blog|docs?|documentation|readme|plan|brief|announcement|landing|page copy|captions?|scripts?|bios?|descriptions?|faq|guide|summary|notes)\b/i;
const CODE_NOUN = /\b(code|function|component|test|tests|endpoint|api|schema|migration|config|script\.(js|ts|py)|class|module|worker)\b/i;
// Reading steps may run on this Mac if nothing in them acts; prose-writing steps may if they aren't about code.
const canMove = (s: Step) => (READS.test(verb(s)) && !ACTS.test(words(s))) || (PROSE_VERB.test(verb(s)) && PROSE_NOUN.test(words(s)) && !CODE_NOUN.test(words(s)));
const READS = /^(read|summari[sz]e|digest|skim|scan|search|find|locate|list|look|extract|collect|gather|explain|review|check|inspect|identify|parse|analy[sz]e)$/;

async function makePlan() {
  if (!ctx || st.planning) return;
  const model = ctx.target()?.localModel;
  if (!model) return;
  st.planning = true; draw();
  const files = st.est?.files.map((f) => `${f.path} (≈${kfmt(f.tokens)} tokens)`).join(", ") || "none named";
  const system = "You plan how a request is split between a strong cloud coding agent and a small local model. The local model can read files and summarize them, digest long outputs (logs, test results), and write prose (emails, copy, docs, posts) into text files. It cannot write code, run commands, use a browser or make decisions. Put prose-writing steps on the mac. Reply with JSON only: {\"steps\":[{\"what\":\"short step\",\"where\":\"mac\"|\"cloud\",\"why\":\"under 10 words\"}]} with 3 to 6 steps in order.";
  const prompt = `Request: ${ctx.text().slice(0, 4000)}\nFiles it names: ${files}`;
  try {
    const r = await invoke<{ text: string }>("local_run", { model, system, prompt });
    const m = r.text.match(/\{[\s\S]*\}/);
    const steps: Step[] = (JSON.parse(m ? m[0] : r.text).steps ?? []).slice(0, 6).map((s: any) => ({ what: String(s.what ?? "").slice(0, 90), where: s.where === "mac" ? "mac" : "cloud", why: String(s.why ?? "").slice(0, 80) }));
    // Safety over the model's opinion: anything that acts stays in the cloud.
    for (const s of steps) {
      if (s.where === "mac" && !canMove(s)) { s.where = "cloud"; s.why = "Changes things, so Claude Code does it"; }
      // Bulky read-only work is what this Mac is for, whatever the model said.
      else if (s.where === "cloud" && canMove(s)) { s.where = "mac"; s.why = PROSE_VERB.test(verb(s)) ? "Prose, so this Mac writes it" : "Read-only, so this Mac does it"; }
    }
    st.plan = steps.length ? steps : null;
    st.planFor = ctx.text();
    if (!st.plan) toast("Couldn't make a plan this time. Split still lets Claude Code use this Mac for the reading.");
  } catch {
    toast("Couldn't make a plan this time. Split still lets Claude Code use this Mac for the reading.");
  }
  st.planning = false;
  draw();
}

/** Text to add to a message for Claude Code when it's sent, or "". */
export function sendSuffix(): string {
  if (!ctx) return "";
  const t = ctx.target();
  if (!t || t.kind !== "claude" || !t.localModel) return "";
  const text = ctx.text().trim();
  const isTask = heavy() || looksLikeAction(text) || text.split(/\s+/).length >= 8;
  if (!isTask) return "";
  if (st.mode === "cloud") return "\n\n(Eiwaz split is off for this request.)";
  if (!heavy()) return "\n\n(Eiwaz split is on: start with your Split line.)";
  return st.plan
      ? `\n\n(Eiwaz split plan. On this Mac with the eiwaz tools: ${st.plan.filter((p) => p.where === "mac").map((p) => p.what).join("; ") || "none"}. Yourself: ${st.plan.filter((p) => p.where === "cloud").map((p) => p.what).join("; ")}.)`
      : "\n\n(Eiwaz split is on and this request is large: start with your Split line and do the bulky reading with the eiwaz tools.)";
}

export function afterSend() { st.est = null; st.plan = null; st.planFor = ""; st.open = false; draw(); }

// ---------- what this Mac did: status bar + Savings ----------

export async function poll(project: string) {
  try {
    const r = await invoke<{ events: any[]; offset: number }>("local_mcp_events", { offset: Math.max(0, st.offset) });
    if (st.offset < 0) { st.offset = r.offset; return; } // first call: skip history
    st.offset = r.offset;
    for (const ev of r.events) {
      if (project && ev.project && !String(ev.project).startsWith(project.replace(/\/$/, ""))) continue;
      let kept = Math.max(0, (ev.read_tokens ?? 0) - (ev.returned_tokens ?? 0));
      st.saved += kept;
      st.steps += 1;
      if (ev.tool === "write_draft") kept += ev.written_tokens ?? 0;
      st.last = ev.tool === "write_draft"
        ? `Mac: wrote ${ev.files?.[0] ?? "a draft"} (≈${kfmt(ev.written_tokens ?? 0)} tokens of writing)`
        : ev.tool === "digest_output"
        ? `Mac: digested output (≈${kfmt(ev.read_tokens)} → ≈${kfmt(ev.returned_tokens)})`
        : `Mac: read ${ev.files?.length ?? 0} file${ev.files?.length === 1 ? "" : "s"} (≈${kfmt(ev.read_tokens)} → ≈${kfmt(ev.returned_tokens)} sent)`;
      await savings.recordAgentStep({ ts: ev.ts, project: project || ev.project, model: ev.model, read_tokens: ev.read_tokens ?? 0, returned_tokens: ev.returned_tokens ?? 0, written_tokens: ev.written_tokens ?? 0, wrote: ev.tool === "write_draft", seconds: ev.seconds ?? 0 });
    }
  } catch { /* the log may not exist yet */ }
  const box = document.getElementById("st-split");
  if (box) {
    box.hidden = !st.steps;
    box.innerHTML = st.steps ? `<span class="dot done" style="display:inline-block;margin-right:6px"></span>${esc(st.last)} · saved ≈${kfmt(st.saved)} tokens this session` : "";
  }
}
