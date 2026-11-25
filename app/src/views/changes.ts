// Changes: what the agents changed since a save point, in plain words, with
// before/after pictures, per-file undo, "undo all" and a save-point timeline.
import { invoke } from "@tauri-apps/api/core";
import { AgentInfo, ago, basename, bus, clock, db, el, esc, events, segmented, sheet, sleep, toast } from "../core";
import * as preview from "./preview";
import * as savings from "./savings";

interface SavePoint { id: string; ts: number; label: string; agent: string; files: number }
interface FileChange { path: string; status: "added" | "changed" | "removed"; added: number; removed: number; agents: string[] }
interface RestoreResult { restored: string[]; removed: string[]; failed: string[]; safety_point: string | null }

const IMG = /\.(png|jpe?g|gif|webp|avif|svg)$/i;
const ROUTINE = /(^|\/)(package-lock\.json|yarn\.lock|pnpm-lock\.yaml|bun\.lockb?|Cargo\.lock|poetry\.lock|Gemfile\.lock|composer\.lock|go\.sum)$|\.min\.(js|css)$|\.map$|(^|\/)(dist|build|out|\.vercel|coverage)\//;

const st = {
  project: "",
  points: [] as SavePoint[],
  files: [] as FileChange[],
  base: "",
  selected: "",
  who: "all",
  mode: "inline" as "inline" | "side",
  error: "",
  loading: false,
  visible: false,
};
let settings: Record<string, { base?: string }> = {};
let busy: Promise<unknown> = Promise.resolve();

/** Serialize git work so snapshots and reads never overlap. */
function queue<T>(f: () => Promise<T>): Promise<T> {
  const p = busy.then(f, f);
  busy = p.catch(() => {});
  return p;
}

export async function snapshot(label: string, agent: string, force = false): Promise<SavePoint | null> {
  const project = bus.project();
  if (!project) return null;
  try {
    const sp = await queue(() => invoke<SavePoint | null>("sp_create", { project, label, agent, force }));
    if (sp) events.savePoint.emit({ id: sp.id, agent });
    return sp;
  } catch (e) {
    st.error = String(e);
    return null;
  }
}

/** Called right before a message goes to an agent: remember how things were. Never waits more than 2.5 s. */
export async function beforeSend(a: AgentInfo) {
  if (a.kind === "shell") return;
  await Promise.race([
    snapshot(`Before you messaged ${a.name}`, "").then((sp) => { if (sp) preview.captureForSavePoint(sp.id); }),
    sleep(2500),
  ]);
}

/** The current changes as a diff, for local jobs. */
export async function currentDiff(): Promise<{ diff: string; since: string } | null> {
  if (!st.project || !st.base) { toast("Open a project folder first."); return null; }
  await refresh();
  if (!st.files.length) { toast("There are no changes since the last save point."); return null; }
  const diff = await queue(() => invoke<string>("sp_diff", { project: st.project, base: st.base, path: null, maxBytes: 120000 }));
  return { diff, since: since() };
}

async function loadProject(project: string) {
  st.project = project;
  st.selected = "";
  st.error = "";
  if (!project) return;
  settings = await db.read("changes-settings", {} as typeof settings);
  try {
    st.points = await queue(() => invoke<SavePoint[]>("sp_list", { project }));
    if (!st.points.length) {
      await snapshot("Eiwaz started watching this folder", "", true);
      st.points = await queue(() => invoke<SavePoint[]>("sp_list", { project }));
    }
  } catch (e) {
    st.error = String(e);
    st.points = [];
  }
  const saved = settings[project]?.base;
  st.base = saved && st.points.some((p) => p.id === saved) ? saved : st.points[st.points.length - 1]?.id ?? "";
  await refresh();
}

async function setBase(id: string) {
  st.base = id;
  settings[st.project] = { ...(settings[st.project] ?? {}), base: id };
  await db.write("changes-settings", settings);
  await refresh();
}

export async function refresh() {
  if (!st.project || !st.base) { bus.setBadge("changes", 0); if (st.visible) draw(); return; }
  st.loading = true;
  try {
    st.points = await queue(() => invoke<SavePoint[]>("sp_list", { project: st.project }));
    st.files = await queue(() => invoke<FileChange[]>("sp_changes", { project: st.project, base: st.base }));
    st.error = "";
  } catch (e) {
    st.error = String(e);
    st.files = [];
  }
  st.loading = false;
  bus.setBadge("changes", st.files.length);
  if (st.visible) draw();
}

export function init() {
  events.project.on((p) => loadProject(p));
  events.agentStarted.on((a) => { snapshot(`Before ${a.name} started`, "").then(refresh); });
  events.turnEnd.on(async (a) => {
    const sp = await snapshot(`After ${a.name}'s turn`, a.name);
    if (sp) preview.captureForSavePoint(sp.id);
    refresh();
  });
}

export function show() { st.visible = true; draw(); refresh(); }
export function hide() { st.visible = false; }

// ---------- drawing ----------

const pointName = (p?: SavePoint) => (p ? `${p.label} · ${clock(p.ts)}${Date.now() / 1000 - p.ts > 86400 ? ` (${ago(p.ts)})` : ""}` : "");
const isRoutine = (p: string) => ROUTINE.test(p);
const STATUS_WORD = { added: "New", changed: "Changed", removed: "Removed" } as const;
const STATUS_ICON = { added: "+", changed: "●", removed: "−" } as const;

function place(p: string) {
  const parts = p.split("/");
  const name = parts.pop()!;
  return `<span class="fdir">${esc(parts.join("/") ? parts.join("/") + "/" : "")}</span><b>${esc(name)}</b>`;
}

function changeBar(f: FileChange) {
  if (f.added < 0) return `<span class="cbar-note">not text</span>`;
  const total = f.added + f.removed || 1;
  const w = Math.min(60, 8 + Math.log2(total + 1) * 8);
  return `<span class="cbar" style="width:${w}px" title="${f.added} lines added, ${f.removed} removed" aria-label="${f.added} lines added, ${f.removed} removed"><i class="a" style="flex:${f.added}"></i><i class="r" style="flex:${f.removed}"></i></span><span class="cnum">+${f.added} −${f.removed}</span>`;
}

function draw() {
  const root = el("view-changes");
  const project = bus.project();
  if (!project) {
    root.innerHTML = `<div class="vempty"><h1>Changes</h1><p>Open a project folder first. Eiwaz then keeps save points, so you can see and undo anything an agent changes.</p></div>`;
    return;
  }
  const base = st.points.find((p) => p.id === st.base);
  const agents = [...new Set(st.files.flatMap((f) => f.agents))];
  const shown = st.files.filter((f) => st.who === "all" || f.agents.includes(st.who));
  const main = shown.filter((f) => !isRoutine(f.path));
  const routine = shown.filter((f) => isRoutine(f.path));
  if (!st.selected || !shown.some((f) => f.path === st.selected)) st.selected = main[0]?.path ?? routine[0]?.path ?? "";

  root.innerHTML = `
    <div class="vhead">
      <div>
        <h1>Changes</h1>
        <p class="sub">These changes are already on your Mac. You can undo any of them.</p>
      </div>
      <div class="vhead-actions">
        <label class="field">Since <select id="ch-base"></select></label>
        <button class="btn" id="ch-refresh" title="Look again">Refresh</button>
      </div>
    </div>
    ${st.error ? `<div class="banner err">${esc(st.error)}</div>` : ""}
    <div class="ch-actions">
      <button class="btn primary" id="ch-ok" ${st.files.length ? "" : "disabled"} title="Marks these as reviewed. Nothing on disk changes.">Looks good</button>
      <button class="btn danger" id="ch-undo-all" ${st.files.length ? "" : "disabled"}>Undo all of this…</button>
      <span class="grow"></span>
      <button class="btn" id="ch-explain" ${st.files.length ? "" : "disabled"} title="Asks a cloud agent to describe the changes and the decisions it made. Its answer appears in its terminal.">Ask an agent to explain</button>
      <button class="btn" id="ch-local" ${st.files.length ? "" : "disabled"} title="A model on this Mac summarizes the changes. Free, private, sometimes less accurate.">Summarize on this Mac</button>
    </div>
    <div id="ch-shots" class="ch-shots"></div>
    ${agents.length > 1 || agents.some((a) => a !== "You or another app") ? `<div class="ch-who"><span class="hint">Changed by</span> <span id="ch-who-seg"></span><span class="hint">Worked out from when each agent was working. If two worked at once, this can be wrong.</span></div>` : ""}
    <div class="ch-body">
      <div class="ch-files" role="list" aria-label="Changed files">
        ${!st.files.length ? `<div class="ch-none"><b>No changes since this save point.</b><p class="hint">When an agent edits files, they show up here with what changed and an Undo button.</p></div>` : ""}
        ${main.map(fileRow).join("")}
        ${routine.length ? `<details class="routine"><summary>Routine changes (${routine.length}) <span class="hint">lock files and built files, usually safe to skip</span></summary>${routine.map(fileRow).join("")}</details>` : ""}
      </div>
      <div class="ch-diff" id="ch-diff">${st.files.length ? `<p class="hint pad">Loading…</p>` : ""}</div>
    </div>
    <h2 class="sec">Save points <span class="hint">Eiwaz saves a point before each message you send and after each agent turn.</span></h2>
    <div class="timeline" id="ch-timeline"></div>`;

  const sel = el<HTMLSelectElement>("ch-base");
  for (const p of st.points.slice(0, 60)) {
    const o = document.createElement("option");
    o.value = p.id;
    o.textContent = pointName(p);
    sel.append(o);
  }
  if (base) sel.value = base.id;
  sel.onchange = () => setBase(sel.value);
  el("ch-refresh").onclick = () => refresh();
  el("ch-ok").onclick = markReviewed;
  el("ch-undo-all").onclick = () => undoTo(st.base, null);
  el("ch-explain").onclick = explain;
  el("ch-local").onclick = summarizeLocally;
  const seg = document.getElementById("ch-who-seg");
  if (seg) seg.append(segmented({ label: "Changed by", value: st.who, items: [{ id: "all", label: "Everyone" }, ...agents.map((a) => ({ id: a, label: a }))], onChange: (v) => { st.who = v; draw(); } }));
  root.querySelectorAll<HTMLElement>(".frow").forEach((r) => {
    r.querySelector(".fpick")!.addEventListener("click", () => { st.selected = r.dataset.path!; root.querySelectorAll(".frow").forEach((x) => x.classList.toggle("sel", x === r)); drawDiff(); });
    r.querySelector(".fundo")!.addEventListener("click", () => undoTo(st.base, [r.dataset.path!]));
  });
  drawDiff();
  drawShots();
  drawTimeline();
}

function fileRow(f: FileChange) {
  return `<div class="frow ${f.status}${f.path === st.selected ? " sel" : ""}" data-path="${esc(f.path)}" role="listitem">
    <button class="fpick" title="${esc(f.path)}">
      <span class="ficon ${f.status}" aria-hidden="true">${STATUS_ICON[f.status]}</span>
      <span class="fmain"><span class="fname">${place(f.path)}</span>
      <span class="fmeta"><span class="fword">${STATUS_WORD[f.status]}</span>${changeBar(f)}${f.agents.map((a) => `<span class="abadge">${esc(a)}</span>`).join("")}</span></span>
    </button>
    <button class="btn small fundo" title="Put this file back the way it was at the save point">Undo</button>
  </div>`;
}

async function drawDiff() {
  const box = document.getElementById("ch-diff");
  if (!box) return;
  const f = st.files.find((x) => x.path === st.selected);
  if (!f) { box.innerHTML = ""; return; }
  const head = `<div class="dhead"><div>${place(f.path)}<div class="hint">${STATUS_WORD[f.status]}${f.agents.length ? " by " + esc(f.agents.join(" and ")) : ""}</div></div><span id="d-mode"></span></div>`;
  if (IMG.test(f.path)) {
    box.innerHTML = head + `<div class="ba"><figure><figcaption>Before</figcaption><div id="img-b" class="baimg"></div></figure><figure><figcaption>After</figcaption><div id="img-a" class="baimg"></div></figure></div>`;
    const load = async (rev: string, id: string) => {
      const t = document.getElementById(id)!;
      try {
        const b64 = await invoke<string>("sp_file", { project: st.project, rev, path: f.path });
        const mime = f.path.toLowerCase().endsWith(".svg") ? "image/svg+xml" : "image/" + (f.path.split(".").pop()!.toLowerCase().replace("jpg", "jpeg"));
        t.innerHTML = `<img alt="${id === "img-b" ? "Before" : "After"}" src="data:${mime};base64,${b64}">`;
      } catch { t.innerHTML = `<p class="hint">${id === "img-b" ? "Didn't exist yet" : "Removed"}</p>`; }
    };
    load(st.base, "img-b");
    load("", "img-a");
    return;
  }
  if (f.added < 0) { box.innerHTML = head + `<p class="hint pad">This file isn't text, so its change can't be shown line by line. You can still undo it.</p>`; return; }
  let diff = "";
  try { diff = await queue(() => invoke<string>("sp_diff", { project: st.project, base: st.base, path: f.path, maxBytes: 300000 })); }
  catch (e) { box.innerHTML = head + `<p class="banner err">${esc(String(e))}</p>`; return; }
  if (st.selected !== f.path) return;
  box.innerHTML = head + `<div class="diff ${st.mode}" id="d-body"></div>`;
  document.getElementById("d-mode")!.append(segmented({ label: "Diff layout", value: st.mode, items: [{ id: "inline", label: "One column" }, { id: "side", label: "Side by side" }], onChange: (v) => { st.mode = v as any; drawDiff(); } }));
  document.getElementById("d-body")!.innerHTML = st.mode === "side" ? sideBySide(diff) : inline(diff);
}

interface Hunk { at: number; ctx: string; lines: { k: " " | "+" | "-" | "\\"; t: string }[] }
function parse(diff: string): Hunk[] {
  const hunks: Hunk[] = [];
  let cur: Hunk | null = null;
  for (const line of diff.split("\n")) {
    const m = /^@@ -\d+(?:,\d+)? \+(\d+)(?:,\d+)? @@ ?(.*)$/.exec(line);
    if (m) { cur = { at: Number(m[1]), ctx: m[2], lines: [] }; hunks.push(cur); continue; }
    if (!cur) continue;
    const k = line[0] as any;
    if (k === "+" || k === "-" || k === " " || k === "\\") cur.lines.push({ k, t: line.slice(1) });
  }
  return hunks;
}
const hunkHead = (h: Hunk) => `<div class="hh">Around line ${h.at}${h.ctx ? ` <span>· ${esc(h.ctx.trim().slice(0, 80))}</span>` : ""}</div>`;
const SR = { "+": `<span class="sr">added: </span>`, "-": `<span class="sr">removed: </span>`, " ": "", "\\": "" };

function inline(diff: string) {
  const hunks = parse(diff);
  if (!hunks.length) return `<p class="hint pad">No line changes (only the file's permissions or name changed).</p>`;
  let n = 0;
  let out = "";
  for (const h of hunks) {
    out += hunkHead(h);
    let ln = h.at;
    for (const l of h.lines) {
      if (++n > 2500) return out + `<p class="hint pad">Long change: showing the first 2,500 lines.</p>`;
      const cls = l.k === "+" ? "add" : l.k === "-" ? "del" : l.k === "\\" ? "note" : "ctx";
      const num = l.k === "-" || l.k === "\\" ? "" : String(ln++);
      out += `<div class="dl ${cls}"><span class="ln">${num}</span><span class="mk" aria-hidden="true">${l.k === " " ? "" : l.k === "\\" ? "" : l.k}</span><code>${SR[l.k]}${esc(l.t) || " "}</code></div>`;
    }
  }
  return out;
}

function sideBySide(diff: string) {
  const hunks = parse(diff);
  if (!hunks.length) return `<p class="hint pad">No line changes.</p>`;
  let out = `<div class="sbs-head"><span>Before</span><span>After</span></div>`;
  for (const h of hunks) {
    out += hunkHead(h);
    const ls = h.lines.filter((l) => l.k !== "\\");
    for (let i = 0; i < ls.length;) {
      if (ls[i].k === " ") { out += row(ls[i].t, "ctx", ls[i].t, "ctx"); i++; continue; }
      const dels: string[] = [], adds: string[] = [];
      while (i < ls.length && ls[i].k === "-") dels.push(ls[i++].t);
      while (i < ls.length && ls[i].k === "+") adds.push(ls[i++].t);
      for (let j = 0; j < Math.max(dels.length, adds.length); j++) out += row(dels[j], dels[j] !== undefined ? "del" : "gap", adds[j], adds[j] !== undefined ? "add" : "gap");
    }
  }
  return out;
  function row(a: string | undefined, ac: string, b: string | undefined, bc: string) {
    const cell = (t: string | undefined, c: string, k: "-" | "+") => `<code class="${c}">${c === "gap" ? "" : (c !== "ctx" ? SR[k] : "") + (esc(t ?? "") || " ")}</code>`;
    return `<div class="sbs">${cell(a, ac, "-")}${cell(b, bc, "+")}</div>`;
  }
}

async function drawShots() {
  const box = document.getElementById("ch-shots");
  if (!box) return;
  const latest = st.points.find((p) => p.ts >= (st.points.find((x) => x.id === st.base)?.ts ?? 0) && p.id !== st.base && p.agent);
  const get = (id?: string) => (id ? invoke<string | null>("sp_shot", { project: st.project, id }).catch(() => null) : Promise.resolve(null));
  // the closest pictures: at/before the baseline, and the newest after it
  const baseIdx = st.points.findIndex((p) => p.id === st.base);
  let before: string | null = null;
  for (const p of st.points.slice(Math.max(0, baseIdx))) { before = await get(p.id); if (before) break; }
  let after: string | null = null;
  for (const p of st.points.slice(0, Math.max(0, baseIdx))) { after = await get(p.id); if (after) break; }
  if (!before && !after) {
    box.innerHTML = preview.siteUrl()
      ? `<p class="hint">Before/after pictures of your site appear here once “Take a picture after each agent turn” is on in Preview.</p>`
      : "";
    return;
  }
  box.innerHTML = `<div class="ba small">
    <figure><figcaption>Before</figcaption>${before ? `<img alt="Your site before these changes" src="data:image/png;base64,${before}">` : `<p class="hint">No picture from back then</p>`}</figure>
    <figure><figcaption>After${latest ? " · " + esc(latest.agent) : ""}</figcaption>${after ? `<img alt="Your site after these changes" src="data:image/png;base64,${after}">` : `<p class="hint">No picture yet</p>`}</figure>
  </div>`;
  box.querySelectorAll("img").forEach((img) => (img.onclick = () => sheet({ title: img.alt, html: `<img class="full" alt="" src="${img.src}">`, buttons: [{ id: "", label: "Close", kind: "primary" }], wide: true })));
}

async function drawTimeline() {
  const box = document.getElementById("ch-timeline");
  if (!box) return;
  box.innerHTML = "";
  for (const p of st.points.slice(0, 40)) {
    const card = document.createElement("div");
    card.className = "tp" + (p.id === st.base ? " base" : "") + (p.agent ? " agent" : "");
    card.innerHTML = `<div class="tthumb" aria-hidden="true"></div>
      <div class="tmeta"><b>${esc(p.label)}</b><span class="hint">${clock(p.ts)} · ${ago(p.ts)}${p.files ? ` · ${p.files} file${p.files === 1 ? "" : "s"}` : ""}</span></div>
      <div class="tbtns">${p.id === st.base ? `<span class="hint">Comparing from here</span>` : `<button class="btn small cmp">Compare from here</button>`}<button class="btn small back">Go back here…</button></div>`;
    card.querySelector(".cmp")?.addEventListener("click", () => setBase(p.id));
    card.querySelector(".back")!.addEventListener("click", () => undoTo(p.id, null, p));
    box.append(card);
    invoke<string | null>("sp_shot", { project: st.project, id: p.id }).then((b64) => {
      if (b64) (card.querySelector(".tthumb") as HTMLElement).innerHTML = `<img alt="" src="data:image/png;base64,${b64}">`;
    }).catch(() => {});
  }
}

// ---------- actions ----------

async function markReviewed() {
  const sp = await snapshot("You reviewed these changes", "", true);
  if (!sp) return toast("Couldn't make a save point.");
  await setBase(sp.id);
  toast("Marked as reviewed. New changes from here on will show up.");
}

async function undoTo(base: string, paths: string[] | null, point?: SavePoint) {
  let list: FileChange[] = st.files;
  if (!paths) {
    try { list = await queue(() => invoke<FileChange[]>("sp_changes", { project: st.project, base })); } catch (e) { return toast(String(e)); }
  } else list = st.files.filter((f) => paths.includes(f.path));
  if (!list.length) return toast("Nothing to undo: the folder already matches that save point.");
  const restore = list.filter((f) => f.status !== "added");
  const remove = list.filter((f) => f.status === "added");
  const target = point ?? st.points.find((p) => p.id === base);
  // Edits made after the latest save point weren't part of any agent turn Eiwaz saw.
  let mine: string[] = [];
  try { mine = await queue(() => invoke<string[]>("sp_unsaved", { project: st.project, paths: list.map((f) => f.path) })); } catch { /* the warning is extra; undo still works */ }
  const names = (fs: FileChange[]) => fs.slice(0, 12).map((f) => `<li>${esc(f.path)}</li>`).join("") + (fs.length > 12 ? `<li>…and ${fs.length - 12} more</li>` : "");
  const ans = await sheet({
    title: paths?.length === 1 ? `Undo the changes to ${basename(paths[0])}?` : `Go back to “${target?.label ?? "this save point"}”?`,
    html: `${target ? `<p class="hint">${esc(pointName(target))}</p>` : ""}
      ${mine.length ? `<div class="banner warn"><b>${mine.length === 1 ? "1 file was" : `${mine.length} files were`} edited after the last save point</b>, maybe by you or by an agent that's still working. Undo will replace ${mine.length === 1 ? "it" : "them"} too:<ul class="flist">${mine.slice(0, 8).map((p) => `<li>${esc(p)}</li>`).join("")}${mine.length > 8 ? `<li>…and ${mine.length - 8} more</li>` : ""}</ul>Those edits aren't lost: the save point Eiwaz makes first keeps them.</div>` : ""}
      ${restore.length ? `<p><b>Will put back ${restore.length} file${restore.length === 1 ? "" : "s"}:</b></p><ul class="flist">${names(restore)}</ul>` : ""}
      ${remove.length ? `<p><b>Will remove ${remove.length} new file${remove.length === 1 ? "" : "s"}:</b></p><ul class="flist">${names(remove)}</ul>` : ""}
      <div class="wont"><b>This can't undo:</b><ul>
        <li>Anything outside ${esc(basename(st.project))}</li>
        <li>Files the project ignores (.gitignore) and folders like node_modules</li>
        <li>Things already sent or published: deploys, emails, payments, database changes, git pushes</li></ul></div>
      <p class="hint">Eiwaz makes a save point first, so you can undo this undo from the timeline.</p>`,
    buttons: [{ id: "", label: "Cancel" }, { id: "go", label: paths ? "Undo" : "Go back", kind: "danger" }],
  });
  if (ans !== "go") return;
  try {
    const r = await queue(() => invoke<RestoreResult>("sp_restore", { project: st.project, base, paths }));
    const n = r.restored.length + r.removed.length;
    toast(r.failed.length ? `Undid ${n}. Couldn't undo ${r.failed.length}: ${r.failed.slice(0, 3).join(", ")}` : `Undid ${n} file${n === 1 ? "" : "s"}.`, 4000);
    if (r.safety_point) events.savePoint.emit({ id: r.safety_point, agent: "" });
  } catch (e) {
    toast(`Couldn't undo: ${e}`);
  }
  refresh();
}

function since() {
  const b = st.points.find((p) => p.id === st.base);
  return b ? `${clock(b.ts)} (“${b.label}”)` : "the last save point";
}

async function explain() {
  const files = st.files.filter((f) => !isRoutine(f.path)).slice(0, 40).map((f) => `- ${f.path} (${STATUS_WORD[f.status].toLowerCase()})`).join("\n");
  const prompt = `Please explain what changed in this project since ${since()}, in plain English for someone who doesn't code. For each file below, say what the change does for the person using the site or app. Then list any decisions you made on your own that I should know about. Don't change any files.\n${files}`;
  const a = await bus.sendToAgent(prompt, { kinds: ["claude", "codex"], start: bus.tools().claude ? "claude" : bus.tools().codex ? "codex" : undefined });
  if (a) toast(`Asked ${a.name}. Its answer appears in its terminal. It's written by the agent that made the changes, so check it against the before/after.`, 6000);
}

async function summarizeLocally() {
  let diff = "";
  try { diff = await queue(() => invoke<string>("sp_diff", { project: st.project, base: st.base, path: null, maxBytes: 120000 })); }
  catch (e) { return toast(String(e)); }
  await savings.localTask("summary", diff, { since: since() });
}
