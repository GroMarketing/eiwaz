// Preview: your running site at phone/tablet/desktop width, plus pictures,
// videos, PDFs, markdown (with diagrams) from the project. One live web view
// at a time; it can sit on its own screen or beside the agents in Work.
import { invoke } from "@tauri-apps/api/core";
import { marked } from "marked";
import { basename, bus, el, esc, events, local, segmented, sleep, toast } from "../core";

interface DevServer { port: number; url: string; command: string; from_project: boolean }
interface Probe { servers: DevServer[]; start_command: string | null; files: string[] }

const DEVICES = { phone: [390, 844], tablet: [820, 1180], full: [1440, 900] } as const;
type Device = keyof typeof DEVICES;

const st = {
  project: "",
  mode: "none" as "none" | "site" | "file",
  url: "",
  file: "",
  device: "phone" as Device,
  autoShot: false,
  probe: null as Probe | null,
  base: "", // local file server base URL
  starting: false,
};

let rootEl: HTMLDivElement | null = null;
const key = (k: string) => `pv:${st.project}:${k}`;
const isVideo = (p: string) => /\.(mp4|mov|webm|m4v)$/i.test(p);
const isImage = (p: string) => /\.(png|jpe?g|gif|webp|avif)$/i.test(p);
const isMd = (p: string) => /\.(md|markdown|mmd)$/i.test(p);
// Preview frames show content Eiwaz didn't write, so they never get
// allow-top-navigation (which would let a page replace Eiwaz itself).
const SITE_SANDBOX = "allow-scripts allow-same-origin allow-forms allow-popups allow-modals allow-downloads";
const FILE_SANDBOX = "allow-scripts allow-forms allow-popups";
const fileUrl = (p: string) => st.base + p.split("/").map(encodeURIComponent).join("/");

export function root(): HTMLDivElement {
  if (rootEl) return rootEl;
  rootEl = document.createElement("div");
  rootEl.className = "pv";
  rootEl.innerHTML = `
    <div class="pv-bar">
      <button class="btn small" id="pv-home" title="Choose what to preview">Sources</button>
      <input id="pv-url" class="pv-url" spellcheck="false" aria-label="Address" placeholder="localhost:5173 or a file in the project">
      <button class="iconbtn" id="pv-reload" title="Reload" aria-label="Reload">↻</button>
      <span id="pv-dev"></span>
      <span class="grow"></span>
      <label class="check" title="After each agent turn, Eiwaz takes a picture of the site so Changes can show before and after"><input type="checkbox" id="pv-auto"> Picture after each agent turn</label>
      <button class="btn small" id="pv-shot" title="Take a picture of what you see and attach it to your next message">Send picture to agent</button>
      <button class="btn small" id="pv-dock"></button>
    </div>
    <div class="pv-stage" id="pv-stage"></div>`;
  const url = rootEl.querySelector<HTMLInputElement>("#pv-url")!;
  url.addEventListener("keydown", (e) => { if (e.key === "Enter") go(url.value.trim()); });
  rootEl.querySelector<HTMLButtonElement>("#pv-reload")!.onclick = () => draw(true);
  rootEl.querySelector<HTMLButtonElement>("#pv-home")!.onclick = () => { st.mode = "none"; save(); draw(); };
  rootEl.querySelector<HTMLButtonElement>("#pv-shot")!.onclick = sendPicture;
  rootEl.querySelector<HTMLInputElement>("#pv-auto")!.onchange = (e) => { st.autoShot = (e.target as HTMLInputElement).checked; save(); if (st.autoShot && !st.url) toast("Pictures are taken of your running site. Pick it under Sources."); };
  rootEl.querySelector<HTMLButtonElement>("#pv-dock")!.onclick = () => {
    if (rootEl!.closest("#view-preview")) bus.dockPreview();
    else bus.showView("preview");
  };
  rootEl.querySelector("#pv-dev")!.append(segmented({
    label: "Screen size", value: st.device,
    items: [{ id: "phone", label: "Phone" }, { id: "tablet", label: "Tablet" }, { id: "full", label: "Desktop" }],
    onChange: (v) => { st.device = v as Device; save(); sizeFrame(); },
  }));
  return rootEl;
}

function save() {
  local.set(key("mode"), st.mode);
  local.set(key("url"), st.url);
  local.set(key("file"), st.file);
  local.set(key("device"), st.device);
  local.set(key("auto"), st.autoShot ? "1" : "");
}

async function loadProject(p: string) {
  st.project = p;
  st.mode = (local.get(key("mode")) as any) || "none";
  st.url = local.get(key("url"));
  st.file = local.get(key("file"));
  st.device = (local.get(key("device")) as Device) || "phone";
  st.autoShot = local.get(key("auto")) === "1";
  st.probe = null;
  try { st.base = await invoke<string>("preview_serve", { project: p }); } catch { st.base = ""; }
  if (rootEl && rootEl.isConnected) { await probe(); draw(); }
}

async function probe() {
  if (!st.project) return;
  try { st.probe = await invoke<Probe>("preview_probe", { project: st.project }); } catch { st.probe = null; }
}

export function init() {
  root();
  events.project.on((p) => loadProject(p));
}

export async function show() {
  el("view-preview").append(root());
  if (!st.probe) await probe();
  draw();
}
export function hide() {}
export async function onDocked() {
  if (!st.probe) await probe();
  draw();
}

export function siteUrl() { return st.mode === "site" ? st.url : ""; }

export function statusText() {
  if (st.mode === "site") return `${st.url.replace(/^https?:\/\//, "")} · ${st.device}`;
  if (st.mode === "file") return basename(st.file);
  return "pick something to show";
}

function go(input: string) {
  if (!input) return;
  if (/^(https?:\/\/|localhost|127\.0\.0\.1|\[::1\])/.test(input) || /^[\w.-]+:\d+/.test(input)) {
    st.mode = "site";
    st.url = /^https?:\/\//.test(input) ? input : `http://${input}`;
  } else {
    st.mode = "file";
    st.file = input.replace(/^\.?\//, "");
  }
  save();
  draw(st.mode === "site"); // a fresh look at what's running
}

function sizeFrame() {
  const frame = rootEl?.querySelector<HTMLElement>(".pv-frame");
  if (!frame) return;
  const [w] = DEVICES[st.device];
  frame.style.width = st.device === "full" ? "100%" : `${w}px`;
  frame.classList.toggle("device", st.device !== "full");
}

function bar() {
  if (!rootEl) return;
  const docked = !rootEl.closest("#view-preview");
  const dock = rootEl.querySelector<HTMLButtonElement>("#pv-dock")!;
  dock.textContent = docked ? "Open on its own screen" : "Show beside agents";
  dock.title = docked ? "Give the preview the whole window" : "Put the preview next to your agents so you can watch it change";
  rootEl.querySelector<HTMLInputElement>("#pv-auto")!.checked = st.autoShot;
  rootEl.querySelector<HTMLInputElement>("#pv-url")!.value = st.mode === "site" ? st.url : st.mode === "file" ? st.file : "";
  rootEl.querySelectorAll<HTMLButtonElement>("#pv-dev button").forEach((b) => b.setAttribute("aria-pressed", String(b.textContent!.toLowerCase().startsWith(st.device === "full" ? "desktop" : st.device))));
}

async function draw(reload = false) {
  if (!rootEl) return;
  bar();
  const stage = rootEl.querySelector<HTMLElement>("#pv-stage")!;
  if (!st.project) { stage.innerHTML = `<div class="vempty"><h1>Preview</h1><p>Open a project folder to preview its site, pictures, videos and docs.</p></div>`; return; }
  if (st.mode === "site") {
    const port = Number(/:(\d+)/.exec(st.url)?.[1] ?? 0);
    const isLocal = /localhost|127\.0\.0\.1|\[::1\]/.test(st.url);
    if (reload) await probe();
    // The probe only lists likely dev servers, so ask about the exact port before saying it's down.
    const listed = !!st.probe?.servers.some((s) => s.port === port);
    const up = listed || !isLocal || !port || (await invoke<boolean>("preview_port_up", { port }).catch(() => false));
    if (isLocal && !up) {
      stage.innerHTML = `<div class="vempty"><h1>Your site isn't running</h1><p>Nothing is answering at ${esc(st.url)}.</p><div class="empty-actions" id="pv-start"></div></div>`;
      startButtons(stage.querySelector("#pv-start")!);
      return;
    }
    // Your own site, from its own address: it may run scripts and keep its own
    // storage, but it can't navigate or reach Eiwaz's window.
    stage.innerHTML = `<div class="pv-frame"><iframe title="Your site" sandbox="${SITE_SANDBOX}" src="${esc(st.url)}"></iframe></div>`;
    sizeFrame();
    return;
  }
  if (st.mode === "file" && st.file) {
    const f = st.file;
    if (isImage(f) || /\.svg$/i.test(f)) stage.innerHTML = `<div class="pv-media"><img alt="${esc(basename(f))}" src="${esc(fileUrl(f))}"></div>`;
    else if (isVideo(f)) stage.innerHTML = `<div class="pv-media"><video controls playsinline preload="metadata" src="${esc(fileUrl(f))}"></video><p class="hint">Streams from your Mac in pieces, so large videos start quickly.</p></div>`;
    else if (isMd(f)) {
      stage.innerHTML = `<div class="pv-frame doc"><iframe title="${esc(basename(f))}" sandbox="allow-popups"></iframe></div>`;
      const frame = stage.querySelector("iframe")!;
      try { frame.srcdoc = await renderDoc(f); } catch (e) { stage.innerHTML = `<p class="banner err">${esc(String(e))}</p>`; }
      return;
    } else {
      // A page file from the project is untrusted: without allow-same-origin it
      // can't read other project files through Eiwaz's file server or touch Eiwaz.
      const box = /\.html?$/i.test(f) ? ` sandbox="${FILE_SANDBOX}"` : "";
      stage.innerHTML = `<div class="pv-frame"><iframe title="${esc(basename(f))}"${box} src="${esc(fileUrl(f))}"></iframe></div>`;
      sizeFrame();
    }
    return;
  }
  drawSources(stage);
}

function startButtons(box: HTMLElement) {
  const cmd = st.probe?.start_command;
  if (cmd) {
    const b = document.createElement("button");
    b.className = "btn primary";
    b.textContent = st.starting ? "Starting…" : `Start it (${cmd})`;
    b.disabled = st.starting;
    b.onclick = () => startSite(cmd);
    box.append(b);
  }
  const p = document.createElement("button");
  p.className = "btn";
  p.textContent = "Pick something else";
  p.onclick = () => { st.mode = "none"; save(); draw(); };
  box.append(p);
}

async function startSite(cmd: string) {
  st.starting = true;
  draw();
  const before = new Set((st.probe?.servers ?? []).map((s) => s.port));
  await bus.spawnTerminal(cmd, "Site server");
  toast(`Starting your site in a terminal tab called “Site server”.`, 4000);
  for (let i = 0; i < 45; i++) {
    await sleep(2000);
    await probe();
    const fresh = st.probe?.servers.find((s) => s.from_project && !before.has(s.port)) ?? st.probe?.servers.find((s) => s.from_project);
    if (fresh) {
      st.starting = false;
      st.mode = "site";
      st.url = fresh.url;
      save();
      draw();
      toast(`Your site is running at ${fresh.url}`);
      return;
    }
  }
  st.starting = false;
  draw();
  toast("The site didn't start within 90 seconds. Check the “Site server” tab in Work for errors.", 6000);
}

function drawSources(stage: HTMLElement) {
  const p = st.probe;
  const servers = p?.servers ?? [];
  const files = p?.files ?? [];
  const group = (label: string, test: (f: string) => boolean) => {
    const fs = files.filter(test);
    return fs.length ? `<h3>${label} <span class="hint">${fs.length}</span></h3><div class="pv-files">${fs.slice(0, 80).map((f) => `<button class="pv-file" data-f="${esc(f)}">${isImage(f) ? `<img loading="lazy" alt="" src="${esc(fileUrl(f))}">` : `<span class="pv-ico">${esc(f.split(".").pop()!.toUpperCase())}</span>`}<span>${esc(f)}</span></button>`).join("")}</div>` : "";
  };
  stage.innerHTML = `<div class="pv-sources">
    <h2>Your site</h2>
    ${servers.length ? servers.map((s) => `<button class="pv-server" data-u="${esc(s.url)}"><span class="dot ready"></span><b>${esc(s.url)}</b><span class="hint">${s.from_project ? "started from this folder" : "running on this Mac"} · ${esc(s.command)}</span></button>`).join("") : `<p class="hint">No site is running for this project yet.</p>`}
    <div class="empty-actions left" id="pv-start"></div>
    ${group("Pages", (f) => /\.html?$/i.test(f))}
    ${group("Docs and diagrams", isMd)}
    ${group("Pictures", (f) => isImage(f) || /\.svg$/i.test(f))}
    ${group("Videos", isVideo)}
    ${group("PDFs", (f) => /\.pdf$/i.test(f))}
    ${!files.length && !servers.length ? `<p class="hint">Nothing to preview found in this folder.</p>` : ""}
  </div>`;
  if (p?.start_command && !servers.some((s) => s.from_project)) startButtons(stage.querySelector("#pv-start")!);
  stage.querySelectorAll<HTMLElement>(".pv-server").forEach((b) => (b.onclick = () => { st.mode = "site"; st.url = b.dataset.u!; save(); draw(); }));
  stage.querySelectorAll<HTMLElement>(".pv-file").forEach((b) => (b.onclick = () => { st.mode = "file"; st.file = b.dataset.f!; save(); draw(); }));
}

// ---------- markdown + diagrams ----------

let mermaidReady: Promise<any> | null = null;
function loadMermaid() {
  mermaidReady ??= import("mermaid").then(({ default: m }) => {
    m.initialize({ startOnLoad: false, theme: "dark", securityLevel: "strict", fontFamily: "-apple-system, sans-serif" });
    return m;
  });
  return mermaidReady;
}
let diagramN = 0;
async function diagram(code: string) {
  try {
    const m = await loadMermaid();
    const { svg } = await m.render(`pvd${++diagramN}`, code);
    return `<figure class="diagram">${svg}</figure>`;
  } catch (e) {
    return `<pre class="bad">This diagram has a mistake, so it can't be drawn:\n${esc(String((e as any)?.message ?? e))}\n\n${esc(code)}</pre>`;
  }
}

async function renderDoc(path: string) {
  let text = await invoke<string>("read_text", { project: st.project, path });
  let front = "";
  if (/\.mmd$/i.test(path)) return docShell(await diagram(text));
  const fm = /^---\n([\s\S]*?)\n---\n/.exec(text);
  if (fm) {
    text = text.slice(fm[0].length);
    const rows = fm[1].split("\n").map((l) => /^([\w-]+):\s*(.*)$/.exec(l)).filter(Boolean) as RegExpExecArray[];
    front = rows.length ? `<table class="front">${rows.map((r) => `<tr><th>${esc(r[1])}</th><td>${esc(r[2])}</td></tr>`).join("")}</table>` : "";
  }
  const blocks: string[] = [];
  text = text.replace(/```mermaid\s*\n([\s\S]*?)```/g, (_, code) => { blocks.push(code); return `\n<div data-mmd="${blocks.length - 1}"></div>\n`; });
  let html = await marked.parse(text, { gfm: true });
  for (let i = 0; i < blocks.length; i++) html = html.replace(`<div data-mmd="${i}"></div>`, await diagram(blocks[i]));
  return docShell(front + html);
}

function docShell(body: string) {
  return `<!doctype html><meta charset="utf-8"><style>
    body{margin:0;padding:28px 34px 60px;background:#0f1218;color:#e7e9ef;font:15px/1.65 -apple-system,BlinkMacSystemFont,sans-serif;max-width:860px}
    h1,h2,h3{line-height:1.25;margin:1.6em 0 .5em} h1{font-size:28px} h2{font-size:21px;border-bottom:1px solid #2a2f3c;padding-bottom:6px} h3{font-size:17px}
    a{color:#9dbbff} code{font:13px "SF Mono",Menlo,monospace;background:#1d212c;padding:2px 5px;border-radius:5px}
    pre{background:#151821;border:1px solid #2a2f3c;padding:14px;border-radius:10px;overflow:auto} pre code{background:none;padding:0}
    table{border-collapse:collapse;margin:1em 0} th,td{border:1px solid #2a2f3c;padding:6px 10px;text-align:left} th{background:#151821}
    blockquote{margin:1em 0;padding:4px 16px;border-left:3px solid #7ea6ff;color:#c3c9d6} img{max-width:100%}
    .front{font-size:13px;color:#a3aabb} .diagram{margin:1.4em 0;padding:16px;background:#151821;border:1px solid #2a2f3c;border-radius:12px;overflow:auto;text-align:center}
    .diagram svg{max-width:100%;height:auto} pre.bad{border-color:#8a3a3a;color:#f0b0b0;white-space:pre-wrap}
    input[type=checkbox]{transform:scale(1.2);margin-right:6px}
  </style><body>${body}</body>`;
}

// ---------- pictures ----------

async function picture(url: string, saveId?: string) {
  const [w, h] = DEVICES[st.device];
  return invoke<string>("preview_screenshot", { url, width: w, height: h, project: saveId ? st.project : null, savePoint: saveId ?? null });
}

async function sendPicture() {
  let path = "";
  if (st.mode === "site" && st.url) {
    toast("Taking a picture…");
    try { path = await picture(st.url); } catch (e) { return toast(String(e), 6000); }
  } else if (st.mode === "file" && st.file) {
    if (/\.html?$/i.test(st.file)) {
      toast("Taking a picture…");
      try { path = await picture(fileUrl(st.file)); } catch (e) { return toast(String(e), 6000); }
    } else {
      // pictures, videos and docs: attach the file itself
      path = `${st.project}/${st.file}`;
      await bus.addAttachment(await invoke<string>("stage_file", { path }));
      bus.showView("work");
      el("msg").focus();
      toast(`Attached ${basename(st.file)}. Say what you'd like changed.`);
      return;
    }
  } else return toast("Pick something to preview first.");
  let thumb: string | undefined;
  try { thumb = await invoke<string>("read_image", { path }); } catch { /* chip without thumbnail */ }
  await bus.addAttachment(path, thumb);
  bus.showView("work");
  el("msg").focus();
  toast("Picture attached to your message. Say what you'd like changed.", 4000);
}

/** After an agent turn: picture of the site, kept with the save point for before/after. */
export async function captureForSavePoint(id: string) {
  if (!st.autoShot || st.mode !== "site" || !st.url) return;
  try { await picture(st.url, id); } catch { /* the site may be restarting; skip */ }
}
