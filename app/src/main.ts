import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { getCurrentWebview } from "@tauri-apps/api/webview";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { open } from "@tauri-apps/plugin-dialog";
import { Terminal } from "@xterm/xterm";
import { FitAddon } from "@xterm/addon-fit";
import "@xterm/xterm/css/xterm.css";
import "./style.css";
import { AgentInfo, Kind, Status, View, basename, bus, db, el, esc, events, gb, local, notify, sheet, sleep, toast } from "./core";
import * as split from "./split";
import * as chat from "./chat";
import * as changes from "./views/changes";
import * as preview from "./views/preview";
import * as bench from "./views/bench";
import * as guide from "./views/guide";
import * as savings from "./views/savings";

interface Session {
  id: number;
  kind: Kind;
  name: string;
  term?: Terminal; // terminals: Claude Code, Codex, plain shell
  fit?: FitAddon;
  chat?: chat.Chat; // local models: a chat pane instead of a terminal
  el: HTMLDivElement;
  lastOutput: number;
  lastInput: number;
  agentSpoke: boolean; // output arrived after the user's last input
  status: Status;
  stopAt: number;
  turnTimer?: number;
  resizeAt: number; // TUIs redraw on resize; that output isn't the agent working
  sawBusyMarker: boolean; // the agent's own "working" marker has appeared at least once
  localModel?: string; // Claude Code started with Eiwaz's "do it on this Mac" tools, using this model
  sessionId?: string; // Claude Code's session id, so Eiwaz can read its log
  color: number; // which colour bubble tells this session apart
  title?: string; // what it's working on: Claude Code's own session title
  firstMsg?: string; // until a title exists: the first words of the first message
  custom?: string; // a name you gave it (double-click to rename)
}
interface Attachment { path: string; label: string; thumb?: string }
interface Stats { mem_used: number; mem_total: number; claude_mem: number; codex_mem: number; ollama_mem: number }
interface Ollama { up: boolean; loaded: { name: string; size: number }[]; installed: string[] }

// Split layout: columns side by side, each holding panes stacked top to bottom.
type PaneItem = { t: "s"; id: number } | { t: "preview" };
interface Pane { item: PaneItem; h: number }
interface Col { w: number; panes: Pane[] }
const MAX_PANES = 4, MAX_COLS = 3, MAX_ROWS = 3;

const win = getCurrentWindow();

const state = {
  project: local.get("project"),
  sessions: new Map<number, Session>(),
  order: [] as number[],
  active: 0,
  tools: {} as Record<string, string>,
  ollama: { up: false, loaded: [], installed: [] } as Ollama,
  attachments: [] as Attachment[],
  focused: true,
  view: "work" as View,
  cols: [] as Col[],
  focusPane: null as Pane | null,
  maxPane: null as Pane | null,
};

const LABEL: Record<Kind, string> = { claude: "Claude Code", codex: "Codex", ollama: "Local model", shell: "Terminal" };
const STATUS_TEXT: Record<Status, string> = {
  starting: "Starting…", running: "Working…", waiting: "Needs you", done: "Done · your turn", ready: "Ready", exited: "Ended",
};
const STATUS_ICON: Record<Status, string> = { starting: "◌", running: "●", waiting: "◆", done: "✓", ready: "○", exited: "■" };

function b64ToBytes(b64: string) {
  const bin = atob(b64);
  const out = new Uint8Array(bin.length);
  for (let i = 0; i < bin.length; i++) out[i] = bin.charCodeAt(i);
  return out;
}
function blobToB64(blob: Blob): Promise<string> {
  return new Promise((res, rej) => {
    const r = new FileReader();
    r.onload = () => res(String(r.result).split(",")[1] ?? "");
    r.onerror = () => rej(r.error);
    r.readAsDataURL(blob);
  });
}
// Escape a path the way Finder does when dropping into Terminal.
const shellPath = (p: string) => p.replace(/([^A-Za-z0-9_\-./~+@%,:])/g, "\\$1");
const info = (s: Session): AgentInfo => ({ id: s.id, kind: s.kind, name: s.name, status: s.status });

// ---------- telling sessions apart: a colour each, labelled by the work ----------
const COLORS = ["#7aa2ff", "#c792ea", "#4fd1c5", "#f6ad55", "#f687b3", "#a3e635", "#fb7185", "#38bdf8"];
const pickColor = () => {
  const used = new Set([...state.sessions.values()].map((s) => s.color));
  for (let i = 0; i < COLORS.length; i++) if (!used.has(i)) return i;
  return state.sessions.size % COLORS.length;
};
/** What the session is doing, in a few words. */
const label = (s: Session) => s.custom || s.title || s.firstMsg || s.name;
/** The agent behind it, when the label is about the work. */
const sub2 = (s: Session) => (label(s) === s.name ? "" : s.name);
const initials = (t: string) => (t.match(/[A-Za-z0-9]+/g) ?? ["?"]).slice(0, 2).map((w) => w[0].toUpperCase()).join("");
const bubble = (s: Session) => `<span class="bub" style="--c:${COLORS[s.color]}" aria-hidden="true">${esc(initials(label(s)))}</span>`;

async function refreshTitles() {
  let changed = false;
  for (const s of state.sessions.values()) {
    if (s.kind !== "claude" || !s.sessionId || s.status === "exited") continue;
    const t = await invoke<string>("claude_title", { project: state.project, sessionId: s.sessionId }).catch(() => "");
    if (t && t !== s.title) { s.title = t; changed = true; }
  }
  if (changed) { render(); layoutRender(); }
}

async function rename(s: Session) {
  const ans = await sheet({
    title: "Name this session",
    html: `<label class="field col">Name <input id="rn-in" type="text" maxlength="60" value="${esc(label(s))}" style="width:100%"></label><p class="hint">Leave it empty to go back to the automatic name (what Claude Code says it's working on).</p>`,
    buttons: [{ id: "", label: "Cancel" }, { id: "ok", label: "Save", kind: "primary" }],
  });
  if (ans !== "ok") return;
  const v = (document.getElementById("rn-in") as HTMLInputElement | null)?.value.trim() ?? "";
  s.custom = v || undefined;
  render(); layoutRender();
}

// ---------- project ----------

async function pickProject() {
  const dir = await open({ directory: true, multiple: false, title: "Choose a project folder" });
  if (typeof dir === "string" && dir) {
    state.project = dir;
    local.set("project", dir);
    renderProject();
    events.project.emit(dir);
  }
}

function renderProject() {
  const btn = el("project-btn");
  btn.textContent = state.project ? basename(state.project) : "Open a project folder…";
  btn.title = state.project ? `${state.project}\nClick to switch folder (⌘O)` : "Open a project folder (⌘O)";
  win.setTitle(state.project ? `${basename(state.project)} — Eiwaz` : "Eiwaz").catch(() => {});
  el("empty-sub").textContent = state.project
    ? `Agents start inside ${basename(state.project)}. Each one runs in its own real terminal. Drag a tab down to see it beside the one you're in.`
    : "Open a project folder, then pick an agent. Each one runs in its own real terminal inside that folder.";
  renderNewMenu();
}

// ---------- new-agent menu ----------

function presets(): { label: string; note: string; kind: Kind; model?: string; ok: boolean }[] {
  const list: { label: string; note: string; kind: Kind; model?: string; ok: boolean }[] = [
    { label: "Claude Code", note: state.tools.claude ? "cloud" : "not installed", kind: "claude", ok: !!state.tools.claude },
    { label: "Codex", note: state.tools.codex ? "cloud" : "not installed", kind: "codex", ok: !!state.tools.codex },
  ];
  // Models that passed Bench first, then untested, then chat-only, then failed.
  const rank = (m: string) => ({ ok: 0, warn: split.label(m).text === "Not tested" ? 1 : 2, bad: 3 } as const)[split.label(m).cls];
  const models = state.ollama.installed.filter((m) => !/embed/i.test(m)).sort((a, b) => rank(a) - rank(b));
  for (const m of models) list.push({ label: m, note: "this Mac", kind: "ollama", ok: true, model: m });
  list.push({ label: "Plain terminal", note: "zsh", kind: "shell", ok: true });
  return list;
}

function renderNewMenu() {
  const menu = el("new-menu");
  const empty = el("empty-actions");
  menu.innerHTML = "";
  empty.innerHTML = "";
  if (!state.project) {
    const b = document.createElement("button");
    b.className = "btn primary";
    b.textContent = "Open a project folder…";
    b.onclick = pickProject;
    empty.append(b);
  }
  const items = presets();
  items.forEach((p, i) => {
    if (p.kind === "ollama" && items[i - 1]?.kind !== "ollama") menu.append(Object.assign(document.createElement("div"), { className: "sep" }));
    if (p.kind === "shell") menu.append(Object.assign(document.createElement("div"), { className: "sep" }));
    const b = document.createElement("button");
    b.disabled = !p.ok;
    const lb = p.kind === "ollama" ? split.label(p.model!) : null;
    b.innerHTML = `<span class="mi-main"><span>${esc(p.label)}</span>${lb ? `<span class="mi-tag ${lb.cls}" title="${esc(lb.why)}">${esc(lb.text)}</span>` : p.kind === "claude" && p.ok ? `<span class="mi-tag ac">Agent · edits and runs commands, asking you first</span>` : ""}</span><small>${esc(p.note)}</small>`;
    if (lb) b.title = lb.why;
    b.onclick = () => { toggleMenu(false); spawn(p.kind, p.model); };
    menu.append(b);
    if (state.project && p.ok && (p.kind !== "ollama" || empty.childElementCount < 3)) {
      const e = document.createElement("button");
      e.className = p.kind === "claude" ? "btn primary" : "btn";
      e.textContent = p.kind === "ollama" ? `${p.label} (this Mac)` : p.label;
      e.onclick = () => spawn(p.kind, p.model);
      empty.append(e);
    }
  });
}

/** What ⌘T and the + tab start: Claude Code, or a plain terminal when it isn't installed. */
const defaultKind = (): Kind => (state.tools.claude ? "claude" : "shell");
const newDefault = () => spawn(defaultKind());

function toggleMenu(show?: boolean) {
  const menu = el("new-menu");
  const next = show ?? menu.hidden;
  if (next) split.refreshLabels().then(renderNewMenu);
  menu.hidden = !next;
  el("new-btn").setAttribute("aria-expanded", String(next));
}

// ---------- sessions ----------

interface Saved { sid: string; color: number; custom?: string; title?: string; firstMsg?: string }
type SpawnOpts = { startup?: string; name?: string; background?: boolean; resume?: Saved };

// Open Claude Code sessions are remembered per project, so quitting or updating
// Eiwaz reopens them where they left off (closing one with × forgets it).
const savedKey = () => `open-sessions:${state.project}`;
function saveOpen() {
  if (!state.project) return;
  const list: Saved[] = state.order.map((id) => state.sessions.get(id)!).filter((s) => s.kind === "claude" && s.sessionId && s.status !== "exited")
    .map((s) => ({ sid: s.sessionId!, color: s.color, custom: s.custom, title: s.title, firstMsg: s.firstMsg }));
  local.set(savedKey(), JSON.stringify(list));
}
async function reopenSaved() {
  if (!state.project || !state.tools.claude) return;
  let list: Saved[] = [];
  try { list = JSON.parse(local.get(savedKey()) || "[]"); } catch { list = []; }
  // A one-time hand-off file (sessions open before this feature existed).
  const once = await db.read<Record<string, Saved[]>>("reopen-once", {});
  if (once[state.project]?.length) {
    for (const r of once[state.project]) if (!list.some((x) => x.sid === r.sid)) list.push(r);
    delete once[state.project];
    await db.write("reopen-once", once);
  }
  for (const r of list) if (r.sid && /^[0-9a-f-]{36}$/i.test(r.sid)) await spawn("claude", undefined, { resume: r, background: true });
  if (list.length) { toast(`Reopened ${list.length} Claude Code session${list.length === 1 ? "" : "s"} where you left off.`); const first = state.order[0]; if (first) activate(first); }
}

async function spawn(kind: Kind, model?: string, opts: SpawnOpts = {}): Promise<number | undefined> {
  if (!state.project) {
    await pickProject();
    if (!state.project) return;
  }
  if (model && !/^[\w.:\/-]+$/.test(model)) { toast("That model name looks unusual, so it wasn't started."); return; }
  if (kind === "ollama" && model) return spawnChat(model, opts);

  const host = document.createElement("div");
  host.className = "term";
  el("parking").append(host);
  const term = new Terminal({
    fontFamily: '"SF Mono", Menlo, monospace',
    fontSize: 13,
    lineHeight: 1.15,
    scrollback: 10000,
    macOptionIsMeta: true,
    allowProposedApi: true,
    theme: { background: "#0b0d11", foreground: "#e7e9ef", cursor: "#7ea6ff", selectionBackground: "#34415f" },
  });
  const fit = new FitAddon();
  term.loadAddon(fit);
  term.open(host);

  let startup = opts.startup ?? (kind === "claude" ? "claude" : kind === "codex" ? "codex" : kind === "ollama" ? `ollama run ${model}` : "");
  let localModel = "";
  const sessionId = kind === "claude" && !opts.startup ? opts.resume?.sid ?? crypto.randomUUID() : "";
  if (sessionId) ({ cmd: startup, local: localModel } = await split.claudeStartup(state.project, sessionId, !!opts.resume));
  if (kind === "ollama" && model) { await split.refreshLabels(); term.write(split.chatOnlyNotice(model)); }
  let id: number;
  try {
    id = await invoke<number>("spawn_session", { cwd: state.project, startup, cols: 100, rows: 30 });
  } catch (e) {
    host.remove();
    term.dispose();
    toast(`Couldn't start: ${e}`);
    return;
  }

  const sameKind = [...state.sessions.values()].filter((s) => s.kind === kind && !opts.name).length;
  const base = opts.name ?? (kind === "ollama" ? model! : LABEL[kind]);
  const s: Session = {
    id, kind, term, fit, el: host,
    name: sameKind ? `${base} ${sameKind + 1}` : base,
    lastOutput: 0, lastInput: 0, agentSpoke: false, status: "starting", stopAt: 0, resizeAt: 0, sawBusyMarker: false,
    localModel: localModel || undefined,
    sessionId: sessionId || undefined,
    color: opts.resume && ![...state.sessions.values()].some((x) => x.color === opts.resume!.color) ? opts.resume.color : pickColor(),
    custom: opts.resume?.custom,
    title: opts.resume?.title,
    firstMsg: opts.resume?.firstMsg,
  };
  state.sessions.set(id, s);
  state.order.push(id);

  term.onData((d) => {
    s.lastInput = Date.now();
    s.agentSpoke = false;
    invoke("write_session", { id, data: d }).catch(() => {});
  });
  term.onResize(({ cols, rows }) => { s.resizeAt = Date.now(); invoke("resize_session", { id, cols, rows }).catch(() => {}); });
  // Images pasted while the terminal has focus become attachments instead of garbage text.
  host.addEventListener("paste", (e) => handlePaste(e as ClipboardEvent), true);
  host.addEventListener("pointerdown", () => { const p = findPane({ t: "s", id }); if (p) { state.focusPane = p; state.active = id; render(); } });

  if (kind !== "shell") events.agentStarted.emit(info(s));
  if (opts.background) {
    render();
  } else {
    activate(id);
  }
  return id;
}

/** A local model opens as a chat pane that talks to Ollama directly (see chat.ts). */
function spawnChat(model: string, opts: SpawnOpts): number {
  let s!: Session;
  const c = chat.create(model, () => tick(), (spoke) => {
    const now = Date.now();
    if (spoke) { s.lastOutput = now; s.agentSpoke = true; } else { s.lastInput = now; s.agentSpoke = false; }
  });
  const id = c.id;
  el("parking").append(c.el);
  const sameKind = [...state.sessions.values()].filter((x) => x.kind === "ollama").length;
  const base = opts.name ?? model;
  s = {
    id, kind: "ollama", chat: c, el: c.el,
    name: sameKind ? `${base} ${sameKind + 1}` : base,
    lastOutput: Date.now(), lastInput: 0, agentSpoke: false, status: "ready", stopAt: 0, resizeAt: 0, sawBusyMarker: false,
    color: pickColor(),
  };
  state.sessions.set(id, s);
  state.order.push(id);
  c.el.addEventListener("paste", (e) => handlePaste(e as ClipboardEvent), true);
  c.el.addEventListener("pointerdown", () => { const p = findPane({ t: "s", id }); if (p) { state.focusPane = p; state.active = id; render(); } });
  events.agentStarted.emit(info(s));
  if (opts.background) render(); else activate(id);
  return id;
}

async function closeSession(id: number) {
  const s = state.sessions.get(id);
  if (!s) return;
  if (s.chat) chat.dispose(id);
  else await invoke("kill_session", { id }).catch(() => {});
  removeFromLayout({ t: "s", id });
  s.term?.dispose();
  s.el.remove();
  state.sessions.delete(id);
  state.order = state.order.filter((x) => x !== id);
  if (state.active === id) {
    const visible = visibleSessions();
    state.active = visible[0] ?? 0;
  }
  layoutRender();
  render();
}

async function stop(id = state.active) {
  const s = state.sessions.get(id);
  if (!s) return;
  if (s.chat) { if (s.chat.busy) { chat.stop(id); toast(`Stopped ${s.name}.`); } return; }
  const now = Date.now();
  if (now - s.stopAt < 2000) {
    await invoke("write_session", { id, data: "\x03" }).catch(() => {});
    toast(`Force-stopped ${s.name} (Ctrl+C).`);
    s.stopAt = 0;
  } else {
    await invoke("write_session", { id, data: "\x1b" }).catch(() => {});
    toast(`Asked ${s.name} to stop (Esc). Press Stop again to force it.`);
    s.stopAt = now;
  }
}

// ---------- split layout ----------

const same = (a: PaneItem, b: PaneItem) => a.t === b.t && (a.t === "preview" || (b.t === "s" && a.id === b.id));
const paneCount = () => state.cols.reduce((n, c) => n + c.panes.length, 0);
function findPane(item: PaneItem): Pane | null {
  for (const c of state.cols) for (const p of c.panes) if (same(p.item, item)) return p;
  return null;
}
function visibleSessions(): number[] {
  const out: number[] = [];
  for (const c of state.cols) for (const p of c.panes) if (p.item.t === "s") out.push(p.item.id);
  return out;
}
function removeFromLayout(item: PaneItem) {
  for (const c of state.cols) c.panes = c.panes.filter((p) => !same(p.item, item));
  state.cols = state.cols.filter((c) => c.panes.length);
  if (state.focusPane && same(state.focusPane.item, item)) state.focusPane = state.cols[0]?.panes[0] ?? null;
}

type Zone = "left" | "right" | "top" | "bottom" | "center";

/** Put `item` into the layout relative to `target`. */
function place(item: PaneItem, target: Pane | null, zone: Zone): boolean {
  if (target && same(target.item, item)) return false;
  const already = findPane(item);
  if (!target || !state.cols.length) {
    removeFromLayout(item);
    state.cols = [{ w: 1, panes: [{ item, h: 1 }] }];
    state.focusPane = state.cols[0].panes[0];
    return true;
  }
  const extra = already ? 0 : 1;
  if (zone !== "center" && paneCount() + extra > MAX_PANES) { toast(`Up to ${MAX_PANES} panes at once. Drop it in the middle of a pane to swap instead.`); return false; }
  const ownCol = already ? state.cols.find((c) => c.panes.includes(already)) : undefined;
  const targetCol = state.cols.find((c) => c.panes.includes(target));
  if ((zone === "left" || zone === "right") && state.cols.length >= MAX_COLS && !(ownCol && ownCol.panes.length === 1)) { toast(`Up to ${MAX_COLS} side by side.`); return false; }
  if ((zone === "top" || zone === "bottom") && targetCol && targetCol.panes.length >= MAX_ROWS && ownCol !== targetCol) { toast(`Up to ${MAX_ROWS} stacked.`); return false; }
  if (zone === "center") {
    removeFromLayout(item);
    target.item = item;
    state.focusPane = target;
    return true;
  }
  removeFromLayout(item);
  // the target may have moved if a column disappeared; find it again by identity
  const col = state.cols.find((c) => c.panes.includes(target));
  if (!col) return place(item, null, "center");
  const pane: Pane = { item, h: target.h };
  if (zone === "left" || zone === "right") {
    const i = state.cols.indexOf(col);
    col.w /= 2;
    state.cols.splice(zone === "left" ? i : i + 1, 0, { w: col.w, panes: [{ item, h: 1 }] });
    state.focusPane = state.cols[zone === "left" ? i : i + 1].panes[0];
  } else {
    const i = col.panes.indexOf(target);
    target.h /= 2;
    pane.h = target.h;
    col.panes.splice(zone === "top" ? i : i + 1, 0, pane);
    state.focusPane = pane;
  }
  return true;
}

function paneTitle(item: PaneItem): { name: string; status: string; dot: string; s?: Session } {
  if (item.t === "preview") return { name: "Preview", status: preview.statusText(), dot: "ready" };
  const s = state.sessions.get(item.id);
  if (!s) return { name: "Ended", status: "", dot: "exited" };
  return { name: label(s), status: (sub2(s) ? sub2(s) + " · " : "") + STATUS_TEXT[s.status], dot: s.status, s };
}

/** Rescale weights so each set averages 1. Splits halve weights and moves or closes
 *  never restore them, so a pane left alone kept e.g. 0.5 and flexbox then filled
 *  only half its column (a flex-grow total under 1 leaves the rest empty). */
function normalizeWeights() {
  const even = <T>(xs: T[], get: (x: T) => number, set: (x: T, v: number) => void) => {
    const sum = xs.reduce((n, x) => n + (get(x) > 0 && isFinite(get(x)) ? get(x) : 0), 0);
    for (const x of xs) set(x, sum > 0 && get(x) > 0 && isFinite(get(x)) ? (get(x) * xs.length) / sum : 1);
  };
  even(state.cols, (c) => c.w, (c, v) => { c.w = v; });
  for (const c of state.cols) even(c.panes, (p) => p.h, (p, v) => { p.h = v; });
}

/** Build the pane grid. Terminal elements are moved, never recreated. */
function layoutRender() {
  normalizeWeights();
  const grid = el("grid");
  const parking = el("parking");
  for (const s of state.sessions.values()) if (s.el.parentElement !== parking) parking.append(s.el);
  const pr = preview.root();
  if (state.view === "work" && pr.parentElement && pr.closest("#grid")) parking.append(pr);
  grid.innerHTML = "";
  el("empty").hidden = paneCount() > 0;
  if (state.maxPane && !state.cols.some((c) => c.panes.includes(state.maxPane!))) state.maxPane = null;
  const cols: Col[] = state.maxPane ? [{ w: 1, panes: [state.maxPane] }] : state.cols;

  cols.forEach((c, ci) => {
    if (ci > 0 && !state.maxPane) grid.append(divider("v", ci));
    const col = document.createElement("div");
    col.className = "col";
    col.style.flexGrow = String(c.w);
    c.panes.forEach((p, ri) => {
      if (ri > 0 && !state.maxPane) col.append(divider("h", ci, ri));
      const pane = document.createElement("div");
      pane.className = "pane" + (p === state.focusPane && paneCount() > 1 ? " focus" : "");
      pane.style.flexGrow = state.maxPane ? "1" : String(p.h);
      (pane as any)._pane = p;
      const t = paneTitle(p.item);
      const head = document.createElement("div");
      head.className = "pane-head";
      if (t.s) { pane.style.setProperty("--c", COLORS[t.s.color]); pane.classList.add("tinted"); }
      head.innerHTML = `<span class="grip" aria-hidden="true">⠿</span>${t.s ? bubble(t.s) : ""}<span class="dot ${t.dot}"></span><b class="pt" title="Double-click to rename">${esc(t.name)}</b><small>${esc(t.status)}</small><span class="grow"></span>` +
        (paneCount() > 1 ? `<button class="mx" title="${state.maxPane ? "Restore the split (or double-click the title bar)" : "Fill the whole area (or double-click the title bar)"}">${state.maxPane ? `Restore · ${paneCount() - 1} hidden` : "Maximize"}</button>` : "") +
        (paneCount() > 1 && !state.maxPane ? `<button class="x" title="Hide this pane (the agent keeps running; its tab stays above)" aria-label="Hide ${esc(t.name)} pane">×</button>` : "");
      head.querySelector(".x")?.addEventListener("click", (e) => { e.stopPropagation(); removeFromLayout(p.item); layoutRender(); render(); });
      const toggleMax = () => { state.maxPane = state.maxPane ? null : p; state.focusPane = p; layoutRender(); };
      head.querySelector(".mx")?.addEventListener("click", (e) => { e.stopPropagation(); toggleMax(); });
      head.addEventListener("dblclick", (e) => {
        if ((e.target as HTMLElement).closest(".pt") && t.s) { e.stopPropagation(); rename(t.s); return; }
        if (!(e.target as HTMLElement).closest("button") && paneCount() > 1) toggleMax();
      });
      startDragOn(head, () => p.item, true);
      const body = document.createElement("div");
      body.className = "pane-body";
      if (p.item.t === "s") {
        const s = state.sessions.get(p.item.id);
        if (s) body.append(s.el);
      } else if (state.view === "work") {
        body.append(pr);
      }
      pane.append(head, body);
      pane.addEventListener("pointerdown", () => {
        if (state.focusPane === p) return;
        state.focusPane = p;
        if (p.item.t === "s") state.active = p.item.id;
        document.querySelectorAll(".pane.focus").forEach((x) => x.classList.remove("focus"));
        if (paneCount() > 1) pane.classList.add("focus");
        render();
      });
      col.append(pane);
    });
    grid.append(col);
  });
  requestAnimationFrame(fitAll);
}

function fitAll() {
  for (const id of visibleSessions()) {
    const s = state.sessions.get(id);
    try { s?.fit?.fit(); } catch { /* not laid out */ }
  }
}

/** Draggable divider between columns (v) or panes (h). */
function divider(dir: "v" | "h", ci: number, ri = 0) {
  const d = document.createElement("div");
  d.className = `div-${dir}`;
  d.setAttribute("role", "separator");
  d.setAttribute("aria-orientation", dir === "v" ? "vertical" : "horizontal");
  d.addEventListener("pointerdown", (e) => {
    e.preventDefault();
    d.setPointerCapture(e.pointerId);
    const a = dir === "v" ? state.cols[ci - 1] : state.cols[ci].panes[ri - 1];
    const b = dir === "v" ? state.cols[ci] : state.cols[ci].panes[ri];
    const getW = (x: Col | Pane) => ("w" in x ? x.w : x.h);
    const setW = (x: Col | Pane, v: number) => { if ("w" in x) x.w = v; else x.h = v; };
    const total = getW(a) + getW(b);
    const prev = d.previousElementSibling as HTMLElement, next = d.nextElementSibling as HTMLElement;
    const r1 = prev.getBoundingClientRect(), r2 = next.getBoundingClientRect();
    const start = dir === "v" ? r1.left : r1.top;
    const span = dir === "v" ? r2.right - r1.left : r2.bottom - r1.top;
    // Embedded pages (Preview) would swallow the pointer once it crosses them.
    document.body.classList.add("resizing", dir === "v" ? "resizing-v" : "resizing-h");
    const move = (ev: PointerEvent) => {
      const pos = (dir === "v" ? ev.clientX : ev.clientY) - start;
      const frac = Math.min(0.85, Math.max(0.15, pos / span));
      setW(a, total * frac);
      setW(b, total * (1 - frac));
      prev.style.flexGrow = String(getW(a));
      next.style.flexGrow = String(getW(b));
    };
    const up = () => {
      d.removeEventListener("pointermove", move); d.removeEventListener("pointerup", up); d.removeEventListener("pointercancel", up);
      document.body.classList.remove("resizing", "resizing-v", "resizing-h");
      fitAll();
    };
    d.addEventListener("pointermove", move);
    d.addEventListener("pointerup", up);
    d.addEventListener("pointercancel", up);
  });
  d.addEventListener("dblclick", () => {
    const a = dir === "v" ? state.cols[ci - 1] : state.cols[ci].panes[ri - 1];
    const b = dir === "v" ? state.cols[ci] : state.cols[ci].panes[ri];
    if ("w" in a && "w" in b) { const t = a.w + b.w; a.w = b.w = t / 2; } else if ("h" in a && "h" in b) { const t = a.h + b.h; a.h = b.h = t / 2; }
    layoutRender();
  });
  return d;
}

// Pointer-based dragging (HTML drag-and-drop is shared with file drops in the Mac webview).
let drag: { item: PaneItem; ghost: HTMLElement; zoneEl: HTMLElement; target: Pane | null; zone: Zone; noop: boolean } | null = null;

function startDragOn(handle: HTMLElement, item: () => PaneItem, fromPane = false) {
  handle.addEventListener("pointerdown", (e) => {
    if (e.button !== 0 || (e.target as HTMLElement).closest(".x, .stopb")) return;
    const sx = e.clientX, sy = e.clientY;
    let started = false;
    const move = (ev: PointerEvent) => {
      if (!started) {
        document.body.classList.add("resizing");
        if (Math.hypot(ev.clientX - sx, ev.clientY - sy) < 6) return;
        started = true;
        beginDrag(item(), handle.textContent?.replace(/[×⠿]/g, "").trim() || "Pane");
      }
      dragMove(ev.clientX, ev.clientY);
    };
    const up = (ev: PointerEvent) => {
      window.removeEventListener("pointermove", move);
      window.removeEventListener("pointerup", up);
      document.body.classList.remove("resizing");
      if (started) { ev.preventDefault(); endDrag(); (handle as any)._justDragged = Date.now(); }
    };
    window.addEventListener("pointermove", move);
    window.addEventListener("pointerup", up);
    if (fromPane) e.preventDefault();
  });
}

function beginDrag(item: PaneItem, label: string) {
  const ghost = document.createElement("div");
  ghost.className = "drag-ghost";
  ghost.textContent = label.slice(0, 40);
  const zoneEl = document.createElement("div");
  zoneEl.className = "drop-zone";
  zoneEl.hidden = true;
  document.body.append(ghost, zoneEl);
  document.body.classList.add("dragging");
  drag = { item, ghost, zoneEl, target: null, zone: "center", noop: false };
  el("drag-hint").hidden = false;
}

/** Would `place` accept this split without hitting a pane limit? (Same rules, no toasts.) */
function fits(item: PaneItem, target: Pane, zone: Zone): boolean {
  if (zone === "center") return true;
  const already = findPane(item);
  if (paneCount() + (already ? 0 : 1) > MAX_PANES) return false;
  const ownCol = already ? state.cols.find((c) => c.panes.includes(already)) : undefined;
  const targetCol = state.cols.find((c) => c.panes.includes(target));
  if ((zone === "left" || zone === "right") && state.cols.length >= MAX_COLS && !(ownCol && ownCol.panes.length === 1)) return false;
  if ((zone === "top" || zone === "bottom") && targetCol && targetCol.panes.length >= MAX_ROWS && ownCol !== targetCol) return false;
  return true;
}

/** The side you asked for if there's room, then the other direction, then swap. */
function bestZone(item: PaneItem, target: Pane, want: Zone): Zone {
  const other: Zone = want === "left" || want === "right" ? "bottom" : "right";
  return fits(item, target, want) ? want : fits(item, target, other) ? other : "center";
}

const paneEl = (p: Pane) => [...document.querySelectorAll<HTMLElement>("#grid .pane")].find((n) => (n as any)._pane === p) ?? null;

// Dragging a tab always means "show it side by side". Over a pane, it goes on the half you're
// pointing at. Anywhere else (the tab bar, the sidebar, a gap), it goes beside the pane you're
// working in. It only swaps a pane out when there's no room left for another.
function dragMove(x: number, y: number) {
  if (!drag) return;
  drag.ghost.style.transform = `translate(${x + 12}px, ${y + 10}px)`;
  const z = drag.zoneEl;
  const hit = document.elementsFromPoint(x, y).find((n) => (n as any)._pane) as HTMLElement | undefined;
  let target: Pane | null = hit ? (hit as any)._pane : null;
  let rectEl: HTMLElement | null = hit ?? null;
  let zone: Zone = "center";
  drag.noop = false;

  if (!paneCount()) {
    // nothing showing yet: just show it
    rectEl = el("terms");
  } else if (target && same(target.item, drag.item)) {
    drag.noop = true;
  } else if (target) {
    const r = hit!.getBoundingClientRect();
    const fx = (x - r.left) / r.width, fy = (y - r.top) / r.height;
    zone = bestZone(drag.item, target, fy < 0.22 ? "top" : fy > 0.78 ? "bottom" : fx < 0.5 ? "left" : "right");
  } else {
    const all = state.cols.flatMap((c) => c.panes);
    target = (state.focusPane && all.includes(state.focusPane) && !same(state.focusPane.item, drag.item) ? state.focusPane : null)
      ?? all.find((p) => !same(p.item, drag!.item)) ?? null;
    if (!target) drag.noop = true; // only the dragged one is showing
    else { rectEl = paneEl(target) ?? el("terms"); zone = bestZone(drag.item, target, "right"); }
  }

  drag.target = target;
  drag.zone = zone;
  if (drag.noop) {
    const own = findPane(drag.item);
    rectEl = (own && paneEl(own)) ?? el("terms");
  }
  const r = rectEl!.getBoundingClientRect();
  const box = { left: r.left, top: r.top, width: r.width, height: r.height };
  if (zone === "left") box.width /= 2;
  if (zone === "right") { box.left += r.width / 2; box.width /= 2; }
  if (zone === "top") box.height /= 2;
  if (zone === "bottom") { box.top += r.height / 2; box.height /= 2; }
  Object.assign(z.style, { left: box.left + "px", top: box.top + "px", width: box.width + "px", height: box.height + "px" });
  z.hidden = false;
  z.classList.toggle("noop", drag.noop);
  const name = target ? paneTitle(target.item).name : "";
  z.textContent = drag.noop ? "Already showing · drop it on another pane"
    : !target ? "Show"
    : zone === "center" ? `Swap in for ${name}`
    : `${zone === "left" ? "Left of" : zone === "right" ? "Right of" : zone === "top" ? "Above" : "Below"} ${name}`;
}

function endDrag() {
  document.body.classList.remove("resizing");
  if (!drag) return;
  const d = drag;
  drag = null;
  d.ghost.remove();
  d.zoneEl.remove();
  document.body.classList.remove("dragging");
  el("drag-hint").hidden = true;
  if (d.zoneEl.hidden || d.noop) return;
  if (state.view !== "work") showView("work");
  state.maxPane = null;
  if (place(d.item, d.target, d.zone)) {
    if (d.item.t === "s") state.active = d.item.id;
    layoutRender();
    render();
    if (d.item.t === "preview") preview.onDocked();
  }
}

function activate(id: number) {
  const s = state.sessions.get(id);
  if (!s) { state.active = 0; render(); return; }
  state.active = id;
  const existing = findPane({ t: "s", id });
  if (existing) state.focusPane = existing;
  else {
    const target = state.focusPane && state.focusPane.item.t === "s" ? state.focusPane : state.cols.flatMap((c) => c.panes).find((p) => p.item.t === "s");
    if (target) place({ t: "s", id }, target, "center");
    else if (state.cols.length && paneCount() < MAX_PANES && state.cols.length < MAX_COLS) place({ t: "s", id }, state.cols[0].panes[0], "left");
    else place({ t: "s", id }, state.focusPane ?? state.cols[0]?.panes[0] ?? null, "center");
  }
  if (state.view !== "work") showView("work");
  layoutRender();
  render();
  requestAnimationFrame(() => { if (s.term) { s.fit?.fit(); s.term.focus(); } else el("msg").focus(); });
}

// ---------- compose ----------

async function addPaths(paths: string[]) {
  for (const p of paths) {
    if (/(^|\/)(\.env(\..*)?|.*\.pem|id_rsa|id_ed25519|.*\.key|credentials\.json)$/i.test(p)) {
      const { sheet } = await import("./core");
      const ok = await sheet({
        title: "This looks like a password or key file",
        html: `<p><b>${esc(basename(p))}</b> usually holds secrets. Sending it to a cloud agent shares those secrets with that company.</p>`,
        buttons: [{ id: "", label: "Don't attach" }, { id: "yes", label: "Attach anyway", kind: "danger" }],
      });
      if (ok !== "yes") continue;
    }
    try {
      const staged = await invoke<string>("stage_file", { path: p });
      const isImg = /\.(png|jpe?g|gif|webp|heic|tiff)$/i.test(p);
      state.attachments.push({ path: staged, label: (isImg ? "Image · " : "") + basename(p) });
    } catch (e) {
      toast(`Couldn't attach ${basename(p)}: ${e}`);
    }
  }
  renderChips();
  el("msg").focus();
}

async function handlePaste(e: ClipboardEvent) {
  const items = [...(e.clipboardData?.items ?? [])];
  const img = items.find((i) => i.kind === "file" && i.type.startsWith("image/"));
  if (!img) return; // plain text: let the default paste happen
  e.preventDefault();
  e.stopPropagation();
  const blob = img.getAsFile();
  if (!blob) return;
  try {
    const ext = img.type.split("/")[1] || "png";
    const path = await invoke<string>("save_attachment", { b64: await blobToB64(blob), ext });
    state.attachments.push({ path, label: "Image · " + basename(path), thumb: URL.createObjectURL(blob) });
    renderChips();
    el("msg").focus();
  } catch (err) {
    toast(`Couldn't attach the image: ${err}`);
  }
}

function renderChips() {
  const box = el("chips");
  box.innerHTML = "";
  state.attachments.forEach((a, i) => {
    const c = document.createElement("div");
    c.className = "chip";
    c.title = a.path;
    c.innerHTML = (a.thumb ? `<img alt="" src="${a.thumb}">` : `<span class="ico">${a.label.startsWith("Image") ? "IMG" : "FILE"}</span>`) +
      `<span>${esc(a.label)}</span><button aria-label="Remove ${esc(a.label)}">×</button>`;
    c.querySelector("button")!.onclick = () => {
      if (a.thumb && a.thumb.startsWith("blob:")) URL.revokeObjectURL(a.thumb);
      state.attachments.splice(i, 1);
      renderChips();
    };
    box.append(c);
  });
  split.update();
}

async function writeMessage(s: Session, payload: string, route?: Record<string, unknown>) {
  if (s.chat) { await chat.send(s.chat, payload, []); return; }
  await changes.beforeSend(info(s));
  if (s.kind === "claude" && s.sessionId) {
    invoke("route_log", { entry: { ts: Date.now(), project: state.project, session_id: s.sessionId, agent: s.name, local_model: s.localModel ?? "", from: route ? "message box" : "Eiwaz", ...(route ?? {}) } }).catch(() => {});
  }
  // Bracketed paste keeps multi-line messages together as one message.
  await invoke("write_session", { id: s.id, data: `\x1b[200~${payload}\x1b[201~` });
  await sleep(90);
  await invoke("write_session", { id: s.id, data: "\r" });
  s.lastInput = Date.now();
  s.agentSpoke = false;
}

async function send() {
  const target = Number((el<HTMLSelectElement>("target")).value) || state.active;
  const s = state.sessions.get(target);
  const box = el<HTMLTextAreaElement>("msg");
  const text = box.value.trim();
  if (!s) return toast("Start an agent first.");
  if (!text && !state.attachments.length) return;
  if (s.kind === "ollama" && split.looksLikeAction(text)) {
    const claude = [...state.sessions.values()].find((x) => x.kind === "claude" && x.status !== "exited");
    const ans = await sheet({
      title: `${s.name} can only chat`,
      html: `<p>This sounds like a request to change files or run commands. Eiwaz runs local models as chat only, so it can talk about it but can't do it.</p>`,
      buttons: [{ id: "", label: "Cancel" }, { id: "chat", label: "Send as chat anyway" }, ...(state.tools.claude ? [{ id: "claude", label: claude ? "Send to Claude Code" : "Start Claude Code and send", kind: "primary" as const }] : [])],
    });
    if (!ans) return;
    if (ans === "claude") {
      box.value = "";
      autosize();
      await bus.sendToAgent(text, { kinds: ["claude"], start: "claude" });
      return;
    }
  }
  if (s.kind === "claude") await split.ensureEstimate();
  if (!s.firstMsg && text) { s.firstMsg = text.split(/\s+/).slice(0, 6).join(" ") + (text.split(/\s+/).length > 6 ? "…" : ""); render(); layoutRender(); }
  const suffix = s.kind === "claude" ? split.sendSuffix() : "";
  const payload = [...state.attachments.map((a) => shellPath(a.path)), text].filter(Boolean).join(" ") + suffix;
  try {
    // A local model's chat reads the files itself; terminals get their paths.
    if (s.chat) { if (!(await chat.send(s.chat, text, state.attachments.map((a) => a.path)))) return; }
    else await writeMessage(s, payload, s.kind === "claude" ? split.routeInfo(text) : undefined);
  } catch (e) {
    return toast(`Couldn't send: ${e}`);
  }
  state.attachments.forEach((a) => a.thumb && a.thumb.startsWith("blob:") && URL.revokeObjectURL(a.thumb));
  state.attachments = [];
  box.value = "";
  autosize();
  renderChips();
  split.afterSend();
  if (!findPane({ t: "s", id: target })) activate(target);
}

function autosize() {
  const box = el<HTMLTextAreaElement>("msg");
  box.style.height = "auto";
  box.style.height = Math.min(box.scrollHeight, 180) + "px";
}

// ---------- rendering ----------

function render() {
  saveOpen();
  const list = el("session-list");
  const tabs = el("tabs");
  const target = el<HTMLSelectElement>("target");
  const prevTarget = target.value;
  list.innerHTML = "";
  tabs.innerHTML = "";
  target.innerHTML = "";
  const visible = new Set(visibleSessions());

  if (!state.order.length) {
    list.innerHTML = `<p class="hint">No agents yet. Use “+ New”.</p>`;
  }
  for (const id of state.order) {
    const s = state.sessions.get(id)!;
    const row = document.createElement("div");
    row.className = `sess ${s.status}${id === state.active ? " active" : ""}`;
    row.style.setProperty("--c", COLORS[s.color]);
    row.innerHTML = `<button class="txt" style="all:unset;flex:1;min-width:0;display:flex;flex-direction:column;gap:2px;cursor:default">
        <span class="name" title="Double-click to rename">${bubble(s)}<span class="nm">${esc(label(s))}</span></span>
        <span class="stat"><span class="dot ${s.status}"></span> ${STATUS_TEXT[s.status]} · ${sub2(s) ? esc(sub2(s)) + " · " : ""}${s.kind === "ollama" ? "on this Mac" : s.kind === "shell" ? "terminal" : "cloud"}</span></button>` +
      (s.status === "running" && s.kind !== "shell" ? `<button class="stopb" aria-label="Stop ${esc(s.name)}">Stop</button>` : "") +
      `<button class="x" aria-label="Close ${esc(s.name)}" title="Close and end this session">×</button>`;
    const txt = row.querySelector(".txt") as HTMLElement;
    txt.onclick = () => { if (Date.now() - ((txt as any)._justDragged ?? 0) > 300) activate(id); };
    txt.ondblclick = () => rename(s);
    startDragOn(txt, () => ({ t: "s", id }));
    row.querySelector<HTMLButtonElement>(".stopb")?.addEventListener("click", () => stop(id));
    (row.querySelector(".x") as HTMLElement).onclick = () => closeSession(id);
    list.append(row);

    const tab = document.createElement("button");
    tab.className = "tab" + (id === state.active ? " on" : "") + (visible.has(id) ? " shown" : "");
    tab.title = visible.has(id) ? "Showing. Drag it onto another pane to put them side by side." : "Click to switch to it. Drag it down to show it beside the one you're in.";
    tab.style.setProperty("--c", COLORS[s.color]);
    tab.innerHTML = `${bubble(s)}<span class="tn">${esc(label(s))}</span><span class="dot ${s.status}"></span><small>${STATUS_TEXT[s.status].toLowerCase()}</small>`;
    tab.ondblclick = () => rename(s);
    tab.onclick = () => { if (Date.now() - ((tab as any)._justDragged ?? 0) > 300) activate(id); };
    startDragOn(tab, () => ({ t: "s", id }));
    tabs.append(tab);

    const opt = document.createElement("option");
    opt.value = String(id);
    opt.textContent = `${label(s)} · ${sub2(s) ? sub2(s) + " · " : ""}${s.kind === "ollama" || s.kind === "shell" ? "this Mac" : "cloud"}`;
    target.append(opt);
  }
  const mini = el("mini-list");
  mini.innerHTML = "";
  for (const id of state.order) {
    const s = state.sessions.get(id)!;
    const b = document.createElement("button");
    b.className = `mini ${s.status}${id === state.active ? " active" : ""}`;
    b.title = `${label(s)}${sub2(s) ? ` (${sub2(s)})` : ""}: ${STATUS_TEXT[s.status]}. Click to show, drag to put side by side.`;
    b.setAttribute("aria-label", `${label(s)}, ${STATUS_TEXT[s.status]}`);
    b.style.setProperty("--c", COLORS[s.color]);
    b.innerHTML = `<span class="dot ${s.status}"></span><span class="ini">${esc(initials(label(s)))}</span>`;
    b.onclick = () => { if (Date.now() - ((b as any)._justDragged ?? 0) > 300) activate(id); };
    startDragOn(b, () => ({ t: "s", id }));
    mini.append(b);
  }

  const plus = document.createElement("button");
  plus.className = "tab add-tab";
  plus.textContent = "+";
  plus.title = `New ${LABEL[defaultKind()]} (⌘T). Right-click or ⌥-click to choose another agent.`;
  plus.setAttribute("aria-label", `New ${LABEL[defaultKind()]}`);
  plus.onclick = (e) => { e.stopPropagation(); if (e.altKey) toggleMenu(true); else newDefault(); };
  plus.oncontextmenu = (e) => { e.preventDefault(); e.stopPropagation(); toggleMenu(true); };
  tabs.append(plus);

  // The preview can sit beside the agents too.
  const pv = document.createElement("button");
  pv.className = "tab preview-tab" + (findPane({ t: "preview" }) ? " shown" : "");
  pv.title = "Drag onto the edge of a pane to watch your site next to an agent. Click to show it on the right.";
  pv.innerHTML = `<svg viewBox="0 0 24 24" aria-hidden="true"><path d="M2 12s3.5-7 10-7 10 7 10 7-3.5 7-10 7S2 12 2 12z"></path><circle cx="12" cy="12" r="3"></circle></svg>Preview`;
  pv.onclick = () => { if (Date.now() - ((pv as any)._justDragged ?? 0) > 300) dockPreview(); };
  startDragOn(pv, () => ({ t: "preview" }));
  tabs.append(pv);

  target.value = state.sessions.has(Number(prevTarget)) && prevTarget !== String(state.active) && document.activeElement === target
    ? prevTarget
    : String(state.active || "");
  el<HTMLButtonElement>("stop-btn").disabled = !state.active;
  if (target.value !== prevTarget) split.update();
  el<HTMLButtonElement>("send-btn").disabled = !state.active;

  const waiting = [...state.sessions.values()].filter((s) => s.status === "waiting" && s.kind !== "shell" && s.lastInput > 0);
  const pill = el("needs-you");
  pill.hidden = !waiting.length;
  pill.innerHTML = `<span class="dot waiting"></span>${waiting.length === 1 ? `${esc(label(waiting[0]))} needs you` : `${waiting.length} agents need you`}`;
  pill.onclick = () => waiting[0] && activate(waiting[0].id);

  const a = state.sessions.get(state.active);
  el("st-active").innerHTML = a ? `${bubble(a)} ${esc(label(a))} · ${STATUS_TEXT[a.status]}` : "No agent selected";

  // keep pane headers' status text fresh without rebuilding terminals
  document.querySelectorAll<HTMLElement>(".pane").forEach((pe) => {
    const p: Pane = (pe as any)._pane;
    if (!p) return;
    const t = paneTitle(p.item);
    const dot = pe.querySelector(".pane-head .dot");
    if (dot) dot.className = `dot ${t.dot}`;
    const sm = pe.querySelector(".pane-head small");
    if (sm) sm.textContent = t.status;
  });
}

function setCollapsed(c: boolean) {
  el("side").hidden = c;
  el("side-mini").hidden = !c;
  el("collapse-btn").setAttribute("aria-expanded", String(!c));
  local.set("sideCollapsed", c ? "1" : "");
  requestAnimationFrame(fitAll);
}

function dockPreview() {
  if (state.view !== "work") showView("work");
  if (!findPane({ t: "preview" })) {
    if (!state.cols.length) place({ t: "preview" }, null, "center");
    else {
      const last = state.cols[state.cols.length - 1];
      if (state.cols.length < MAX_COLS && paneCount() < MAX_PANES) place({ t: "preview" }, last.panes[0], "right");
      else place({ t: "preview" }, last.panes[last.panes.length - 1], "center");
    }
  }
  layoutRender();
  render();
  preview.onDocked();
}

// Agent status. Claude Code and Codex say what they're doing on screen: a
// "esc to interrupt" line while working, and a numbered choice ("❯ 1. Yes")
// when they ask for approval. Read that when it's there. Otherwise fall back to
// timing: output in the last ~1.5 s = working; quiet after speaking = done.
const BUSY_RE = /esc to interrupt/i;
const ASK_RE = /^\s*[❯›]\s*1[.)]\s+\S|Do you want to (proceed|make this edit|create|allow|run)|\(y\/n\)/im;
function screenTail(s: Session, lines = 30): string {
  if (!s.term) return "";
  const b = s.term.buffer.active;
  const end = b.baseY + s.term.rows;
  const out: string[] = [];
  for (let y = Math.max(0, end - lines); y < end; y++) out.push(b.getLine(y)?.translateToString(true) ?? "");
  return out.join("\n");
}
function classify(s: Session, now: number): Status {
  if (s.chat) return s.chat.busy ? "running" : s.agentSpoke && s.lastInput > 0 ? "done" : "ready";
  if (!s.lastOutput) return "starting";
  const quietTurn = (): Status => (s.agentSpoke && s.lastInput > 0 ? "done" : "ready");
  if (s.kind === "shell") return now - s.lastOutput < 1500 ? "running" : "ready";
  if (s.kind === "claude" || s.kind === "codex") {
    const text = screenTail(s);
    if (BUSY_RE.test(text)) { s.sawBusyMarker = true; return "running"; }
    if (ASK_RE.test(text)) return "waiting";
    // Once we've seen the marker, its absence means idle, whatever redraws happen.
    if (s.sawBusyMarker) return quietTurn();
  }
  return now - s.lastOutput < 1500 ? "running" : quietTurn();
}
const needsAttention = (s: Session) => (s.status === "waiting" || s.status === "done") && s.kind !== "shell" && s.lastInput > 0;

let lastBadge = -1;
function tick() {
  const now = Date.now();
  let changed = false;
  for (const s of state.sessions.values()) {
    if (s.status === "exited") continue;
    const next = classify(s, now);
    if (next !== s.status) {
      const prev = s.status;
      s.status = next;
      changed = true;
      const away = !state.focused || s.id !== state.active;
      if (next === "waiting" && s.lastInput > 0 && away) {
        notify(`${label(s)} needs you`, `${basename(state.project)} · it's asking you something`);
      } else if (prev === "running" && next === "done" && away) {
        notify(`${label(s)} is done`, `${basename(state.project)} · your turn`);
      }
      if (next === "running") { clearTimeout(s.turnTimer); s.turnTimer = undefined; }
      if (prev === "running" && (next === "done" || next === "waiting" || next === "ready") && s.kind !== "shell" && s.lastInput > 0) {
        clearTimeout(s.turnTimer);
        s.turnTimer = window.setTimeout(() => { if (s.status !== "running" && s.status !== "exited") events.turnEnd.emit(info(s)); }, 2500);
      }
    }
  }
  if (changed) render();
  const count = [...state.sessions.values()].filter(needsAttention).length;
  const badge = state.focused ? 0 : count;
  if (badge !== lastBadge) {
    lastBadge = badge;
    win.setBadgeCount(badge || undefined).catch(() => {});
  }
}

async function refreshStats() {
  try {
    const st = await invoke<Stats>("system_stats");
    const pct = st.mem_total ? (st.mem_used / st.mem_total) * 100 : 0;
    el("st-mem").innerHTML = `Mac memory<span class="meter"><i class="${pct > 90 ? "max" : pct > 75 ? "hi" : ""}" style="width:${pct.toFixed(0)}%"></i></span><b>${gb(st.mem_used)} / ${gb(st.mem_total)} GB</b>`;
    const parts = [];
    if (st.claude_mem) parts.push(`Claude Code <b>${gb(st.claude_mem)} GB</b>`);
    if (st.codex_mem) parts.push(`Codex <b>${gb(st.codex_mem)} GB</b>`);
    el("st-agents").innerHTML = parts.join(" · ");
  } catch { /* ignore */ }
  try {
    const o = await invoke<Ollama>("ollama_status");
    const firstLoad = !state.ollama.installed.length && o.installed.length;
    state.ollama = o;
    el("st-ollama").innerHTML = o.up ? `Ollama <b>${o.loaded.length ? o.loaded.length + " loaded" : "idle"}</b>` : `Ollama <b>off</b>`;
    const box = el("models");
    box.innerHTML = "";
    for (const m of o.loaded) {
      const pill = document.createElement("span");
      pill.className = "pill";
      pill.innerHTML = `On this Mac: ${esc(m.name)} <b>${gb(m.size)} GB</b><button class="btn">Unload</button>`;
      pill.querySelector("button")!.onclick = async () => {
        try { await invoke("unload_model", { name: m.name }); toast(`Unloaded ${m.name}. Memory freed.`); refreshStats(); }
        catch (e) { toast(`Couldn't unload: ${e}`); }
      };
      box.append(pill);
    }
    if (firstLoad) renderNewMenu();
  } catch { /* ignore */ }
}

// ---------- screens ----------

const VIEWS: View[] = ["work", "changes", "guide", "bench", "preview", "savings"];
const viewModules: Partial<Record<View, { show: () => void; hide?: () => void }>> = { changes, guide, bench, preview, savings };

function showView(v: View) {
  const prev = state.view;
  if (prev === v) return;
  viewModules[prev]?.hide?.();
  state.view = v;
  document.querySelectorAll<HTMLButtonElement>(".rail button[data-view]").forEach((b) => {
    const on = b.dataset.view === v;
    b.classList.toggle("on", on);
    if (on) b.setAttribute("aria-current", "page"); else b.removeAttribute("aria-current");
  });
  el("work").hidden = v !== "work";
  for (const name of VIEWS) if (name !== "work") el(`view-${name}`).hidden = name !== v;
  if (v === "work") {
    layoutRender();
    render();
  } else {
    viewModules[v]?.show();
  }
}

function setBadge(v: View, n: number) {
  const b = document.querySelector<HTMLElement>(`.rail button[data-view="${v}"] .badge`);
  if (!b) return;
  b.hidden = !n;
  b.textContent = n > 99 ? "99+" : String(n);
  b.setAttribute("aria-label", `${n} not reviewed`);
}

// ---------- wiring ----------

function wireBus() {
  bus.project = () => state.project;
  bus.agents = () => state.order.map((id) => info(state.sessions.get(id)!));
  bus.activeAgent = () => { const s = state.sessions.get(state.active); return s ? info(s) : undefined; };
  bus.tools = () => state.tools;
  bus.showView = showView;
  bus.setBadge = setBadge;
  bus.dockPreview = dockPreview;
  bus.spawnTerminal = (startup, name) => spawn("shell", undefined, { startup, name, background: true });
  bus.addAttachment = async (path, thumb) => {
    state.attachments.push({ path, label: "Image · " + basename(path), thumb: thumb ? `data:image/png;base64,${thumb}` : undefined });
    renderChips();
  };
  bus.sendToAgent = async (text, opts = {}) => {
    const kinds = opts.kinds ?? ["claude", "codex", "ollama"];
    let s = opts.id ? state.sessions.get(opts.id) : undefined;
    if (!s) {
      const act = state.sessions.get(state.active);
      s = act && kinds.includes(act.kind) && act.status !== "exited" ? act : state.order.map((id) => state.sessions.get(id)!).find((x) => kinds.includes(x.kind) && x.status !== "exited");
    }
    if (!s && opts.start) {
      if (opts.start !== "shell" && !state.tools[opts.start]) { toast(`${LABEL[opts.start]} isn't installed.`); return; }
      const id = await spawn(opts.start);
      s = id ? state.sessions.get(id) : undefined;
      if (!s) return;
      toast(`Starting ${s.name}… your message goes in once it's ready.`);
      for (let i = 0; i < 60 && (s.status === "starting" || Date.now() - s.lastOutput < 1500); i++) await sleep(500);
      await sleep(800);
    }
    if (!s) { toast("Start an agent first."); return; }
    try { await writeMessage(s, text); } catch (e) { toast(`Couldn't send: ${e}`); return; }
    activate(s.id);
    return info(s);
  };
}

async function init() {
  wireBus();
  await chat.init();
  renderProject();
  render();

  await listen<{ id: number; data: string }>("pty-output", (e) => {
    const s = state.sessions.get(e.payload.id);
    if (!s?.term) return;
    s.term.write(b64ToBytes(e.payload.data));
    const now = Date.now();
    if (s.lastOutput && now - s.resizeAt < 800) return; // redraw after a resize, not activity
    s.lastOutput = now;
    if (now - s.lastInput > 400) s.agentSpoke = true; // ignore the echo of the user's own typing
  });
  await listen<number>("pty-exit", (e) => {
    const s = state.sessions.get(e.payload);
    if (!s?.term) return;
    s.status = "exited";
    s.term.write("\r\n\x1b[90m[session ended — close it with ×]\x1b[0m\r\n");
    render();
  });

  await getCurrentWebview().onDragDropEvent((e) => {
    if (drag) return; // a pane is being dragged, not a file
    const p = e.payload;
    if (p.type === "enter" || p.type === "over") el("drop").hidden = false;
    else if (p.type === "leave") el("drop").hidden = true;
    else if (p.type === "drop") { el("drop").hidden = true; if (p.paths.length) { if (state.view !== "work") showView("work"); addPaths(p.paths); } }
  });
  await win.onFocusChanged(({ payload }) => { state.focused = payload; tick(); });

  el("project-btn").onclick = pickProject;
  el("collapse-btn").onclick = () => setCollapsed(true);
  el("expand-btn").onclick = () => setCollapsed(false);
  setCollapsed(local.get("sideCollapsed") === "1");
  el("new-btn").onclick = (e) => { e.stopPropagation(); toggleMenu(); };
  document.addEventListener("click", (e) => { if (!el("new-menu").contains(e.target as Node)) toggleMenu(false); });
  el("send-btn").onclick = send;
  el("stop-btn").onclick = () => stop();
  document.querySelectorAll<HTMLButtonElement>(".rail button[data-view]").forEach((b) => (b.onclick = () => showView(b.dataset.view as View)));

  const box = el<HTMLTextAreaElement>("msg");
  box.addEventListener("paste", handlePaste);
  box.addEventListener("input", () => { autosize(); split.update(); });
  el("target").addEventListener("change", () => split.update());
  split.initStrip({
    project: () => state.project,
    text: () => el<HTMLTextAreaElement>("msg").value,
    attachments: () => state.attachments.map((a) => a.path),
    target: () => { const s = state.sessions.get(Number(el<HTMLSelectElement>("target").value) || state.active); return s ? { kind: s.kind, localModel: s.localModel } : undefined; },
  });
  setInterval(() => split.poll(state.project), 2500);
  setInterval(refreshTitles, 5000);
  split.refreshLabels().then(renderNewMenu);
  box.addEventListener("keydown", (e) => {
    if (e.key === "Enter" && !e.shiftKey && !e.isComposing) { e.preventDefault(); send(); }
    if (e.key === "Escape") { e.preventDefault(); stop(); }
  });
  document.addEventListener("keydown", (e) => {
    if (e.key === "Escape" && drag) { drag.zoneEl.hidden = true; endDrag(); }
    if (e.metaKey && e.key === "\\") { e.preventDefault(); setCollapsed(!el("side").hidden); }
    if (e.metaKey && e.key.toLowerCase() === "o") { e.preventDefault(); pickProject(); }
    if (e.metaKey && e.key.toLowerCase() === "t") { e.preventDefault(); newDefault(); }
    if (e.metaKey && e.key.toLowerCase() === "l") { e.preventDefault(); showView("work"); box.focus(); }
    if (e.metaKey && !e.ctrlKey && /^[1-9]$/.test(e.key)) { const id = state.order[Number(e.key) - 1]; if (id) { e.preventDefault(); activate(id); } }
    if (e.ctrlKey && !e.metaKey && /^[1-6]$/.test(e.key)) { e.preventDefault(); showView(VIEWS[Number(e.key) - 1]); }
  });

  new ResizeObserver(() => fitAll()).observe(el("terms"));

  setInterval(tick, 700);
  refreshStats();
  setInterval(refreshStats, 4000);
  state.tools = await invoke<Record<string, string>>("detect_tools").catch(() => ({}));
  renderNewMenu();

  for (const m of [changes, preview, bench, guide, savings]) m.init();
  if (state.project) events.project.emit(state.project);
  // Before reopening sessions: if Eiwaz crashed or was force-quit last time, its
  // terminals' programs may still be running with no window. End them first, so a
  // reopened Claude Code session doesn't run alongside a leftover copy of itself.
  const swept = await invoke<{ unclean: boolean; ended: string[]; summary: string }>("sessions_sweep").catch(() => null);
  if (swept?.ended.length) {
    toast(`Eiwaz didn't close properly last time. Stopped ${swept.ended.length} program${swept.ended.length === 1 ? "" : "s"} its terminals left running: ${swept.summary}.`, 9000);
  }
  await reopenSaved();
}

init();
