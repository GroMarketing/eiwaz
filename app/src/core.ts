// Shared helpers and the small "bus" the screens use to talk to the Work view
// (agents, compose box) without importing main.ts.
import { invoke } from "@tauri-apps/api/core";
import { isPermissionGranted, requestPermission, sendNotification } from "@tauri-apps/plugin-notification";

export type Kind = "claude" | "codex" | "ollama" | "shell";
// waiting = the agent is asking you something (approval, a choice); done = its turn finished.
export type Status = "starting" | "running" | "waiting" | "done" | "ready" | "exited";
export interface AgentInfo { id: number; kind: Kind; name: string; status: Status }
export type View = "work" | "changes" | "guide" | "bench" | "preview" | "savings";

export const el = <T extends HTMLElement = HTMLElement>(id: string) => document.getElementById(id) as T;
export const esc = (s: string) => String(s).replace(/[&<>"']/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" }[c]!));
export const gb = (b: number) => (b / 1024 ** 3).toFixed(1);
export const basename = (p: string) => p.split("/").filter(Boolean).pop() ?? p;
export const sleep = (ms: number) => new Promise((r) => setTimeout(r, ms));
export const kfmt = (n: number) => (n >= 1e6 ? (n / 1e6).toFixed(1) + "M" : n >= 1000 ? (n / 1000).toFixed(n >= 1e4 ? 0 : 1) + "K" : String(Math.round(n)));
export const ago = (sec: number) => {
  const d = Date.now() / 1000 - sec;
  if (d < 60) return "just now";
  if (d < 3600) return `${Math.floor(d / 60)} min ago`;
  if (d < 86400) return `${Math.floor(d / 3600)} h ago`;
  return new Date(sec * 1000).toLocaleDateString(undefined, { month: "short", day: "numeric" });
};
export const clock = (sec: number) => new Date(sec * 1000).toLocaleTimeString(undefined, { hour: "numeric", minute: "2-digit" });

export const local = {
  get: (k: string) => { try { return localStorage.getItem(k) ?? ""; } catch { return ""; } },
  set: (k: string, v: string) => { try { localStorage.setItem(k, v); } catch { /* ignore */ } },
};

/** Durable JSON documents in the app's data folder. */
export const db = {
  async read<T>(name: string, fallback: T): Promise<T> {
    try { const v = await invoke<T | null>("store_read", { name }); return (v ?? fallback) as T; } catch { return fallback; }
  },
  async write(name: string, value: unknown) {
    try { await invoke("store_write", { name, value }); } catch (e) { toast(`Couldn't save: ${e}`); }
  },
};

export function toast(msg: string, ms = 2800) {
  const t = el("toast");
  t.textContent = msg;
  t.hidden = false;
  clearTimeout((toast as any).timer);
  (toast as any).timer = setTimeout(() => (t.hidden = true), ms);
}

export async function notify(title: string, body: string) {
  try {
    let ok = await isPermissionGranted();
    if (!ok) ok = (await requestPermission()) === "granted";
    if (ok) sendNotification({ title, body });
  } catch { /* optional */ }
}

/** A modal sheet. Resolves with the id of the button pressed, or "" when dismissed. */
export function sheet(opts: { title: string; html: string; buttons: { id: string; label: string; kind?: "primary" | "danger" | "" }[]; wide?: boolean }): Promise<string> {
  return new Promise((resolve) => {
    const d = document.createElement("dialog");
    d.className = "sheet" + (opts.wide ? " wide" : "");
    d.innerHTML = `<h2>${esc(opts.title)}</h2><div class="sheet-body">${opts.html}</div><div class="sheet-foot"></div>`;
    const foot = d.querySelector(".sheet-foot")!;
    for (const b of opts.buttons) {
      const btn = document.createElement("button");
      btn.className = `btn ${b.kind ?? ""}`;
      btn.textContent = b.label;
      btn.onclick = () => { d.close(b.id); };
      foot.append(btn);
    }
    d.addEventListener("close", () => { resolve(d.returnValue || ""); d.remove(); });
    document.body.append(d);
    d.showModal();
    (foot.querySelector(".primary, .danger") as HTMLElement | null)?.focus();
  });
}

/** Segmented control with aria-pressed. */
export function segmented(opts: { label: string; items: { id: string; label: string }[]; value: string; onChange: (id: string) => void }) {
  const g = document.createElement("div");
  g.className = "seg";
  g.setAttribute("role", "group");
  g.setAttribute("aria-label", opts.label);
  const draw = (v: string) => {
    g.innerHTML = "";
    for (const it of opts.items) {
      const b = document.createElement("button");
      b.textContent = it.label;
      b.setAttribute("aria-pressed", String(it.id === v));
      b.onclick = () => { draw(it.id); opts.onChange(it.id); };
      g.append(b);
    }
  };
  draw(opts.value);
  return g;
}

/** Hooks filled in by main.ts. */
export const bus = {
  project: () => "",
  agents: (): AgentInfo[] => [],
  activeAgent: (): AgentInfo | undefined => undefined,
  /** Send text to an agent as if typed in the compose box. Starts Claude Code if none is running and `start` is set. */
  sendToAgent: async (_text: string, _opts?: { id?: number; kinds?: Kind[]; start?: Kind }): Promise<AgentInfo | undefined> => undefined,
  spawnTerminal: async (_startup: string, _name: string): Promise<number | undefined> => undefined,
  addAttachment: async (_path: string, _thumbB64?: string) => {},
  showView: (_v: View) => {},
  setBadge: (_v: View, _n: number) => {},
  tools: (): Record<string, string> => ({}),
  /** Put an element (the preview) into a Work pane beside the agents. */
  dockPreview: () => {},
};

type Listener<T> = (v: T) => void;
function emitter<T>() {
  const ls: Listener<T>[] = [];
  return { on: (f: Listener<T>) => ls.push(f), emit: (v: T) => ls.forEach((f) => { try { f(v); } catch (e) { console.error(e); } }) };
}
export const events = {
  project: emitter<string>(),
  /** An agent finished a turn (went quiet after working). */
  turnEnd: emitter<AgentInfo>(),
  /** The user sent a message to an agent. */
  sent: emitter<AgentInfo>(),
  agentStarted: emitter<AgentInfo>(),
  savePoint: emitter<{ id: string; agent: string }>(),
};

export async function openExternal(url: string) {
  try { await invoke("open_url", { url }); } catch (e) { toast(`Couldn't open: ${e}`); }
}

/** Info icon with a one-line explanation (title for mouse, aria-label for screen readers). */
export const info = (text: string) => `<span class="info" tabindex="0" role="img" aria-label="${esc(text)}" title="${esc(text)}">i</span>`;

/** Horizontal bar 0..1 with text. */
export const bar = (frac: number, cls = "") => `<span class="hbar ${cls}" aria-hidden="true"><i style="width:${Math.max(2, Math.min(100, frac * 100)).toFixed(0)}%"></i></span>`;
