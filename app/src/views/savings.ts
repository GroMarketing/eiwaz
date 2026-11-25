// Savings: small, explicit jobs done by a model on this Mac instead of a cloud
// agent, and an honest count of what that kept off the cloud.
import { invoke } from "@tauri-apps/api/core";
import { basename, bus, db, el, esc, events, info, kfmt, local, segmented, toast } from "../core";

type Task = "summary" | "savenote" | "explain" | "agent" | "write";
type JobTask = Exclude<Task, "agent" | "write">;
interface Run { ts: number; project: string; task: Task; model: string; prompt_tokens: number; output_tokens: number; seconds: number; redone?: boolean }
interface LocalReply { text: string; prompt_tokens: number; output_tokens: number; seconds: number }

// Claude API list prices per million tokens, as of Nov 2026.
const PRICES = {
  sonnet: { label: "Claude Sonnet 5.5", inp: 2, out: 10 },
  opus: { label: "Claude Opus 5.5", inp: 4, out: 20 },
  haiku: { label: "Claude Haiku 5.5", inp: 0.1, out: 0.5 },
} as const;
type PriceKey = keyof typeof PRICES;

const TASK_LABEL: Record<Task, string> = { summary: "Summarize changes", savenote: "Write a save note", explain: "Explain an error", agent: "Read for Claude Code (split)", write: "Wrote copy for Claude Code (split)" };
const SYSTEM: Record<JobTask, string> = {
  summary: "You explain code changes to someone who doesn't code. Only describe what the diff actually shows. Use short bullet points grouped by what a person using the site or app would notice. If something is unclear, say so instead of guessing.",
  savenote: "Write a git commit message for this diff: one summary line under 72 characters, a blank line, then one to four short bullet points. Output only the message.",
  explain: "Explain this error or log to someone who doesn't code: what went wrong, the most likely cause, and exactly what to ask the coding agent to do about it. Be brief and plain.",
};

const st = { runs: [] as Run[], period: "week", scope: "project", price: (local.get("savings:price") as PriceKey) || "sonnet", visible: false, models: [] as string[], good: new Set<string>(), never: {} as Record<string, boolean> };

export function init() {
  db.read<Run[]>("savings", []).then((r) => (st.runs = r));
  db.read<Record<string, boolean>>("savings-never", {}).then((n) => (st.never = n));
  events.project.on(() => { if (st.visible) draw(); });
}

interface RouteMsg { ts: number; preview: string; words: number; turns: number; input: number; output: number; context_end: number; tool_tokens: number; tool_calls: number; local_calls: number; local_read: number; subagents: number; split_line: boolean; split_result: boolean }
interface RouteRow { route: { ts: number; split?: string; note?: string; heavy?: boolean; est_tokens?: number | null; from?: string; agent?: string }; msg: RouteMsg | null }
let routes: RouteRow[] = [];

export async function show() {
  st.visible = true;
  await loadModels();
  draw();
  const project = bus.project();
  if (project) {
    try { routes = await invoke<RouteRow[]>("route_report", { project, limit: 30 }); } catch { routes = []; }
    draw();
  }
}

const M = (n: number) => (n >= 1e6 ? `${(n / 1e6).toFixed(1)}M` : kfmt(n));
function messagesSection() {
  const rows = routes.filter((r) => r.msg);
  if (!routes.length) return `<div class="vempty small"><p>Nothing yet. Messages you send to Claude Code from the Work screen show up here once Claude has answered, with what each one cost and what ran on this Mac.</p></div>`;
  const total = rows.reduce((n, r) => n + r.msg!.input, 0);
  const local = rows.filter((r) => r.msg!.local_calls > 0).length;
  const short = rows.filter((r) => r.msg!.words <= 12);
  const shortIn = short.reduce((n, r) => n + r.msg!.input, 0);
  // Whether work really ran here comes from Claude Code's own log (its calls to
  // Eiwaz's tools), not from what Claude said it would do.
  // Only messages where a step on this Mac was expected: a split plan, or a heavy request with split on.
  const planned = (r: RouteRow) => r.route.note === "plan" || (r.route.note === "on" && !!r.route.heavy);
  const skipped = rows.filter((r) => planned(r) && r.msg!.local_calls === 0).length;
  const splitTag = (r: RouteRow) => r.msg && planned(r) && r.msg.local_calls === 0 ? `<span class="warnc" title="Split was on for this message, but Claude Code's log shows no step run on this Mac. Claude did all of it in the cloud.">${r.route.note} · didn't run here</span>` : r.route.split === "unavailable" ? `<span class="hint">no local tools</span>` : r.route.note === "off" || r.route.split === "cloud" ? `<span class="hint">all cloud</span>` : r.route.note === "plan" ? `<span class="okc">plan</span>` : r.route.note === "on" ? `<span class="okc">on</span>` : `<span class="hint">not a task</span>`;
  return `<p class="hint">${rows.length} message${rows.length === 1 ? "" : "s"} · ${M(total)} tokens of cloud input in total · ${local} used this Mac${skipped ? ` · <span class="warnc">${skipped} with split on never ran a step here</span>` : ""} · messages of 12 words or fewer were ${total ? Math.round((100 * shortIn) / total) : 0}% of the input ${info("Short messages aren't the cause by themselves: every message re-sends the whole conversation, so even a short one costs as much as the conversation is long.")}</p>
    <div class="tbl-wrap"><table class="tbl compact"><thead><tr>
      <th>When</th><th>Your message</th><th>Split</th>
      <th>Claude turns ${info("How many times Claude went back to the cloud model while working on this message, including any helpers (subagents) it started.")}</th>
      <th>Cloud input ${info("Every turn re-sends the whole conversation, so this is turns × conversation size. It's mostly cached, which costs less than new input.")}</th>
      <th>Conversation at the end ${info("How big the conversation was on the last turn. Over about 250K, starting a fresh session saves a lot on every later message.")}</th>
      <th>Tool output ${info("Files, command output and web pages that entered the conversation while it worked.")}</th>
      <th>On this Mac ${info("Steps Claude Code actually handed to this Mac, counted from its own log.")}</th><th>Split lines ${info("Did Claude say up front which steps run on this Mac, and report what it did at the end?")}</th>
    </tr></thead><tbody>${routes.map((r) => {
      const m = r.msg;
      const when = new Date(r.route.ts).toLocaleString(undefined, { month: "short", day: "numeric", hour: "numeric", minute: "2-digit" });
      if (!m) return `<tr><td>${when}</td><td colspan="8"><span class="hint">Waiting for Claude Code's log (still working, or the session was closed before it answered).</span></td></tr>`;
      return `<tr><td>${when}</td><td><span title="${esc(m.preview)}">${esc(m.preview.length > 48 ? m.preview.slice(0, 48) + "…" : m.preview)}</span><div class="hint">${m.words} words${r.route.from === "Eiwaz" ? " · sent by Eiwaz" : ""}</div></td>
        <td>${splitTag(r)}</td><td>${m.turns}${m.subagents ? `<div class="hint">${m.subagents} helper${m.subagents === 1 ? "" : "s"}</div>` : ""}</td>
        <td><b>${M(m.input)}</b></td><td class="${m.context_end > 250000 ? "warnc" : ""}">${M(m.context_end)}</td><td>${M(m.tool_tokens)}</td>
        <td>${m.local_calls ? `<span class="okc">${m.local_calls} step${m.local_calls === 1 ? "" : "s"}</span><div class="hint">read ${M(m.local_read)}</div>` : `<span class="hint">none</span>`}</td>
        <td>${m.split_line ? "✓" : "—"} / ${m.split_result ? "✓" : "—"}</td></tr>`;
    }).join("")}</tbody></table></div>`;
}
export function hide() { st.visible = false; }

async function loadModels() {
  try {
    const env = await invoke<{ models: { name: string }[] }>("bench_env");
    st.models = env.models.map((m) => m.name).filter((n) => !/embed/i.test(n));
  } catch { st.models = []; }
  const bench = await db.read<Record<string, any>>("bench", {});
  st.good = new Set(Object.values(bench).filter((r: any) => r.verdict === "good").map((r: any) => r.model));
}

function chosenModel() {
  const saved = local.get("savings:model");
  if (saved && st.models.includes(saved)) return saved;
  return st.models.find((m) => st.good.has(m)) ?? st.models[0] ?? "";
}

const cost = (r: Run, p = PRICES[st.price]) => (r.prompt_tokens * p.inp + r.output_tokens * p.out) / 1e6;

/** Run one job on this Mac. Used by this screen and by Changes. */
export async function localTask(task: JobTask, input: string, ctx: { since?: string } = {}) {
  const project = bus.project();
  if (project && st.never[project]) return toast("This project is set to never use the local model (see Savings).");
  if (!st.models.length) await loadModels();
  const model = chosenModel();
  if (!model) return toast("No local model found. Install Ollama and a model, then try again.", 5000);
  if (!input.trim()) return toast("There's nothing to work on.");
  let text = input;
  let cut = false;
  if (text.length > 100_000) { text = text.slice(0, 100_000); cut = true; }
  const prompt = task === "summary" ? `These are the changes since ${ctx.since ?? "the last save point"}:\n\n${text}` : text;

  const d = document.createElement("dialog");
  d.className = "sheet wide";
  d.innerHTML = `<h2>${esc(TASK_LABEL[task])} on this Mac</h2><div class="sheet-body"><p class="hint">Working with ${esc(model)}${st.good.has(model) ? " (passed Bench)" : " (not tested in Bench yet)"}. Nothing leaves your Mac.</p><div class="lt-out"><span class="spinner" aria-hidden="true"></span> Thinking…</div></div><div class="sheet-foot"><button class="btn" value="close">Close</button></div>`;
  document.body.append(d);
  d.addEventListener("close", () => d.remove());
  d.querySelector<HTMLButtonElement>("button[value=close]")!.onclick = () => d.close();
  d.showModal();
  const out = d.querySelector<HTMLElement>(".lt-out")!;
  let r: LocalReply;
  try {
    r = await invoke<LocalReply>("local_run", { model, system: SYSTEM[task], prompt });
  } catch (e) {
    out.innerHTML = `<p class="banner err">The local model couldn't do it: ${esc(String(e))}</p>`;
    return;
  }
  const run: Run = { ts: Math.floor(Date.now() / 1000), project, task, model, prompt_tokens: r.prompt_tokens, output_tokens: r.output_tokens, seconds: r.seconds };
  st.runs.push(run);
  await db.write("savings", st.runs);
  out.innerHTML = `<div class="lt-text">${esc(r.text).replace(/\n/g, "<br>")}</div>
    <p class="hint">${cut ? "Only the first part was read (it was very long). " : ""}Took ${r.seconds.toFixed(0)} s · ${kfmt(r.prompt_tokens + r.output_tokens)} tokens kept on this Mac · written by a local model, so check it.</p>`;
  const foot = d.querySelector(".sheet-foot")!;
  foot.innerHTML = "";
  const btn = (label: string, kind: string, f: () => void) => { const b = document.createElement("button"); b.className = `btn ${kind}`; b.textContent = label; b.onclick = f; foot.append(b); };
  btn("Not good enough: ask a cloud agent", "", async () => {
    run.redone = true;
    await db.write("savings", st.runs);
    d.close();
    const ask = task === "summary" ? `Please explain, in plain English for someone who doesn't code, what changed since ${ctx.since ?? "the last save point"} and any decisions you made on your own. Don't change any files.`
      : task === "savenote" ? "Please write a short commit message for the current changes. Don't commit, just show it."
      : `Please explain this error in plain English and what you'd do about it:\n${text.slice(0, 4000)}`;
    await bus.sendToAgent(ask, { kinds: ["claude", "codex"], start: bus.tools().claude ? "claude" : undefined });
  });
  btn("Copy", "", () => { navigator.clipboard.writeText(r.text).then(() => toast("Copied.")); });
  btn("Close", "primary", () => d.close());
  if (st.visible) draw();
}

/** A step Claude Code handed to this Mac (from the split tools). Only the tokens
 *  kept off the cloud count: what was read here minus the brief sent back. */
export async function recordAgentStep(ev: { ts: number; project: string; model: string; read_tokens: number; returned_tokens: number; written_tokens?: number; wrote?: boolean; seconds: number }) {
  if (!st.runs.length) st.runs = await db.read<Run[]>("savings", []);
  // Writing: the cloud model didn't have to write the text (output) or keep it in the conversation.
  st.runs.push({ ts: ev.ts, project: ev.project, task: ev.wrote ? "write" : "agent", model: ev.model, prompt_tokens: Math.max(0, ev.read_tokens - ev.returned_tokens), output_tokens: ev.written_tokens ?? 0, seconds: ev.seconds });
  await db.write("savings", st.runs);
  if (st.visible) draw();
}

/** For each kind of local job, the most reliable model: it must have passed
 *  Bench's file checks and planted-instruction test, and score at least 80 in
 *  that job's test. Ties go to the faster model. Categories with no such model
 *  are left out, so Claude Code does that work itself. */
export async function bestModels(): Promise<Record<string, string>> {
  const bench = await db.read<Record<string, any>>("bench", {});
  const installed = new Set(st.models.length ? st.models : (await loadModels(), st.models));
  const safe = Object.values(bench).filter((r: any) => installed.has(r.model) && !(r.checks ?? []).some((c: any) => c.status === "bad") && (r.injection ?? []).length > 0 && !(r.injection ?? []).some((c: any) => c.followed));
  const out: Record<string, string> = {};
  for (const cat of ["summarize", "digest", "write"]) {
    const ranked = safe
      .map((r: any) => ({ model: r.model, job: (r.jobs ?? []).find((j: any) => j.category === cat) }))
      .filter((x) => x.job && x.job.score >= 80)
      .sort((a, b) => b.job.score - a.job.score || a.job.seconds - b.job.seconds);
    if (ranked[0]) out[cat] = ranked[0].model;
  }
  return out;
}

/** The local model Bench rated best, for steps run on this Mac. */
export async function bestLocalModel(): Promise<string> {
  const bench = await db.read<Record<string, any>>("bench", {});
  const good = Object.values(bench).filter((r: any) => r.verdict === "good" && !(r.injection ?? []).some((c: any) => c.followed));
  good.sort((a: any, b: any) => (b.score ?? 0) - (a.score ?? 0) || (b.tool_cases?.filter((c: any) => c.ok).length ?? 0) - (a.tool_cases?.filter((c: any) => c.ok).length ?? 0));
  return good[0]?.model ?? "";
}

export const neverLocal = (project: string) => !!st.never[project];

function inPeriod(r: Run) {
  const now = Date.now() / 1000;
  return st.period === "all" || r.ts > now - (st.period === "week" ? 7 : 30) * 86400;
}

function draw() {
  if (!st.visible) return;
  const root = el("view-savings");
  const project = bus.project();
  const runs = st.runs.filter((r) => inPeriod(r) && (st.scope === "all" || r.project === project));
  // A job you sent to the cloud anyway saved nothing (the cloud did the work too),
  // so only jobs you kept count toward tokens and dollars.
  const kept = runs.filter((r) => !r.redone);
  const tokIn = kept.reduce((n, r) => n + r.prompt_tokens, 0);
  const tokOut = kept.reduce((n, r) => n + r.output_tokens, 0);
  const dollars = kept.reduce((n, r) => n + cost(r), 0);
  const redone = runs.length - kept.length;
  const model = chosenModel();

  // bars per day
  const days = st.period === "week" ? 7 : st.period === "month" ? 30 : 60;
  const today = new Date(); today.setHours(0, 0, 0, 0);
  const buckets = Array.from({ length: days }, (_, i) => ({ day: new Date(today.getTime() - (days - 1 - i) * 86400000), tok: 0, n: 0 }));
  for (const r of kept) {
    const idx = days - 1 - Math.floor((today.getTime() - new Date(r.ts * 1000).setHours(0, 0, 0, 0)) / 86400000);
    if (idx >= 0 && idx < days) { buckets[idx].tok += r.prompt_tokens + r.output_tokens; buckets[idx].n++; }
  }
  const max = Math.max(1, ...buckets.map((b) => b.tok));

  root.innerHTML = `
    <div class="vhead">
      <div><h1>Savings</h1><p class="sub">Small jobs done by a model on this Mac instead of a cloud agent.</p></div>
      <div class="vhead-actions"><span id="sv-period"></span><span id="sv-scope"></span>
        <label class="field">Priced as <select id="sv-price">${(Object.keys(PRICES) as PriceKey[]).map((k) => `<option value="${k}">${PRICES[k].label}</option>`).join("")}</select></label></div>
    </div>
    <div class="tiles">
      <div class="tile"><span class="tl">Jobs done on this Mac</span><b>${runs.length}</b></div>
      <div class="tile"><span class="tl">Tokens kept off the cloud ${info("Counted by Ollama for each job: what the model read plus what it wrote. Jobs you sent to the cloud anyway don't count.")}</span><b>${kfmt(tokIn + tokOut)}</b><span class="hint">${kfmt(tokIn)} read · ${kfmt(tokOut)} written</span></div>
      <div class="tile"><span class="tl">API-equivalent value ${info(`Those tokens priced at ${PRICES[st.price].label}'s API rate ($${PRICES[st.price].inp} per million read, $${PRICES[st.price].out} written). On a subscription this isn't cash saved: it's usage that didn't count toward your plan's limits.`)}</span><b>$${dollars < 0.01 && dollars > 0 ? "<0.01" : dollars.toFixed(2)}</b><span class="hint">estimate, not cash saved</span></div>
      <div class="tile ${redone ? "warn" : ""}"><span class="tl">Sent to the cloud anyway ${info("Times you said the local answer wasn't good enough. A high number means the local model isn't up to these jobs.")}</span><b>${redone}</b><span class="hint">${runs.length ? Math.round((redone / runs.length) * 100) + "% of jobs" : "none yet"}</span></div>
    </div>
    ${kept.length ? `<div class="chart" aria-label="Tokens kept on this Mac per day">
      ${buckets.map((b) => `<div class="cb" title="${b.day.toLocaleDateString(undefined, { month: "short", day: "numeric" })}: ${b.n} job${b.n === 1 ? "" : "s"}, ${kfmt(b.tok)} tokens"><i style="height:${b.tok ? Math.max(4, (b.tok / max) * 100) : 0}%"></i></div>`).join("")}
    </div>
    <div class="chart-axis"><span>${buckets[0].day.toLocaleDateString(undefined, { month: "short", day: "numeric" })}</span><span>today</span></div>` : ""}

    <h2 class="sec">Messages to Claude Code</h2>
    ${messagesSection()}

    <h2 class="sec">Do a job on this Mac</h2>
    <div class="card">
      <div class="row">
        <label class="field">Model <select id="sv-model">${st.models.map((m) => `<option value="${esc(m)}">${esc(m)}${st.good.has(m) ? " · passed Bench" : ""}</option>`).join("") || `<option value="">No local models found</option>`}</select></label>
        ${model && !st.good.has(model) ? `<span class="hint">Not tested yet. <button class="linkbtn" id="sv-bench">Test it in Bench</button></span>` : ""}
      </div>
      <div class="row wrap">
        <button class="btn" id="sv-sum" ${project ? "" : "disabled"}>Summarize the current changes</button>
        <button class="btn" id="sv-note" ${project ? "" : "disabled"}>Write a save note for them</button>
      </div>
      <label class="field col">Paste an error or log to have it explained
        <textarea id="sv-err" rows="3" placeholder="Paste the red text from a terminal or the browser here"></textarea></label>
      <div class="row"><button class="btn" id="sv-explain">Explain it</button><span class="grow"></span>
        ${project ? `<label class="check"><input type="checkbox" id="sv-never" ${st.never[project] ? "checked" : ""}> Never use the local model for ${esc(basename(project))}</label>` : ""}</div>
    </div>

    <h2 class="sec">Recent jobs</h2>
    ${runs.length ? `<table class="tbl"><thead><tr><th>When</th><th>Job</th><th>Model</th><th>Tokens</th><th>Time</th><th>Est. API cost</th><th>Result</th></tr></thead><tbody>
      ${runs.slice().reverse().slice(0, 40).map((r) => `<tr><td>${new Date(r.ts * 1000).toLocaleString(undefined, { month: "short", day: "numeric", hour: "numeric", minute: "2-digit" })}</td><td>${TASK_LABEL[r.task]}${st.scope === "all" ? `<div class="hint">${esc(basename(r.project))}</div>` : ""}</td><td>${esc(r.model)}</td><td>${kfmt(r.prompt_tokens + r.output_tokens)}</td><td>${r.seconds.toFixed(0)} s</td><td>$${cost(r).toFixed(3)}</td><td>${r.redone ? `<span class="tag warn">sent to cloud</span>` : `<span class="tag ok">kept</span>`}</td></tr>`).join("")}
    </tbody></table>` : `<div class="vempty small"><p>No jobs done on this Mac ${st.period === "all" ? "yet" : "in this period"}. Try “Summarize the current changes” above, or from the Changes screen.</p></div>`}

    <details class="method"><summary>How this is counted</summary>
      <p>Each job's tokens are counted by Ollama on your Mac. The dollar figure prices them at the API rate of the cloud model you pick above (Claude API list prices, November 2026). It's an estimate: a cloud model counts tokens a little differently, and might have needed more or fewer of them.</p>
      <p>On a Claude subscription you don't pay per token. There, this is usage that didn't count toward your plan's limits. Eiwaz can't see your plan's limit, so it doesn't turn this into a percentage.</p>
      <p>Jobs you start here are counted, and so are steps Claude Code hands to this Mac when split is on: reading (“Read for Claude Code”) and writing emails, copy and docs (“Wrote copy for Claude Code”, counted as writing Claude didn't have to do). Eiwaz never sends work to the local model without Claude Code or you asking for it.</p>
      <p>Messages to Claude Code: Eiwaz records when it sent each message, the split setting and its estimate (not your text), and reads the rest from Claude Code's own session log on this Mac. Nothing is sent anywhere.</p>
    </details>`;

  el("sv-period").append(segmented({ label: "Period", value: st.period, items: [{ id: "week", label: "7 days" }, { id: "month", label: "30 days" }, { id: "all", label: "All time" }], onChange: (v) => { st.period = v; draw(); } }));
  el("sv-scope").append(segmented({ label: "Which projects", value: st.scope, items: [{ id: "project", label: "This project" }, { id: "all", label: "All projects" }], onChange: (v) => { st.scope = v; draw(); } }));
  const price = el<HTMLSelectElement>("sv-price");
  price.value = st.price;
  price.onchange = () => { st.price = price.value as PriceKey; local.set("savings:price", st.price); draw(); };
  const ms = el<HTMLSelectElement>("sv-model");
  if (model) ms.value = model;
  ms.onchange = () => { local.set("savings:model", ms.value); draw(); };
  document.getElementById("sv-bench")?.addEventListener("click", () => bus.showView("bench"));
  const diffNow = async () => {
    const changes = await import("./changes");
    return changes.currentDiff();
  };
  el("sv-sum").onclick = async () => { const d = await diffNow(); if (d) localTask("summary", d.diff, { since: d.since }); };
  el("sv-note").onclick = async () => { const d = await diffNow(); if (d) localTask("savenote", d.diff); };
  el("sv-explain").onclick = () => localTask("explain", el<HTMLTextAreaElement>("sv-err").value);
  document.getElementById("sv-never")?.addEventListener("change", async (e) => {
    st.never[project] = (e.target as HTMLInputElement).checked;
    await db.write("savings-never", st.never);
  });
}
