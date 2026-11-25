// A chat pane for local models. Replaces `ollama run` in a terminal, which can't
// open files, treats a message starting with "/" as a command, and always holds
// the same amount of text. Eiwaz reads attachments itself (text, PDF, Word,
// images), sizes the model's memory to fit them, and streams the reply here.
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { marked } from "marked";
import { basename, esc } from "./core";

interface Plan { max_ctx: number; safe_ctx: number; vision: boolean }
interface ChatFile { name: string; kind: "text" | "image"; text: string; image: string; pages: number; cut: boolean }
interface ApiMsg { role: "user" | "assistant"; content: string; images?: string[] }
interface Doc { name: string; text: string; pages: number }
interface Found { name: string; text: string; places: string; passages: number; total_passages: number }
interface SearchResult { docs: Found[]; semantic: boolean; note: string }

export interface Chat {
  id: number;
  model: string;
  el: HTMLDivElement;
  list: HTMLDivElement;
  info: HTMLElement;
  history: ApiMsg[];
  busy: boolean;
  ctx: number; // context (tokens) the model is currently loaded with for this chat
  plan?: Plan;
  reading?: string; // what to call the input while the model reads it ("report.pdf, 140 pages")
  sentEstimate?: number; // our token estimate for the last request, to correct against Ollama's count
  searching?: HTMLElement; // the progress label while a big file is searched
  docs?: Doc[]; // files too big to send whole: later questions search them again
  lastAsk?: string; // the question those files were last searched for
  reply?: { body: HTMLElement; think: HTMLDetailsElement; thinkPre: HTMLElement; text: string; thought: string; status: HTMLElement; timer?: number };
  onChange: () => void; // tell the Work view the status changed
  onActivity: (spoke: boolean) => void;
}

const chats = new Map<number, Chat>();
let nextId = 1_000_000; // apart from terminal ids, which count up from 1
const START_CTX = 32768;
const REPLY_ROOM = 4096; // tokens kept free for the answer
const IMAGE_TOKENS = 600;
// History kept when a big file is searched: the rest of the window goes to its passages.
const HISTORY_SHARE = 0.25;
// Passages sent for a question. The best ~60 passages answer it; more only slows the reply
// (a full 131K window takes Gemma 4 about a minute to read) and adds distractions.
const QUESTION_TOKENS = 24576;
// Asks with nothing specific to search for. These get an even sample of the whole file.
const GENERIC = /^\s*(summari[sz]e|sum (it|this) up|tl;?dr|overview|outline|key (points|takeaways)|what('?s| is) (this|it)( about)?|describe)\b[^?]{0,60}\??\s*$/i;
// Measured on Gemma 4 (2026-11-22): prose ~4.2 characters a token, code ~3.1.
// Each reply's real count (from Ollama) then corrects the estimate for that model.
const fix = new Map<string, number>();
const looksLikeCode = (t: string) => { const n = Math.min(t.length, 20000); const sym = t.slice(0, n).match(/[{}()\[\];=<>]/g)?.length ?? 0; return n > 0 && sym / n > 0.03; };
const tokensOf = (t: string, model = "") => Math.ceil((t.length / (looksLikeCode(t) ? 3.0 : 4.0)) * 1.05 * (fix.get(model) ?? 1));
const WORDS_PER_TOKEN = 0.75, WORDS_PER_PAGE = 450;
const pages = (tok: number) => Math.max(1, Math.round((tok * WORDS_PER_TOKEN) / WORDS_PER_PAGE));
const msgTokens = (m: ApiMsg, model = "") => tokensOf(m.content, model) + (m.images?.length ?? 0) * IMAGE_TOKENS + 8;

// Model output is untrusted: raw HTML is shown as text, links open in the browser, images become their alt text.
marked.use({
  renderer: {
    html({ text }) { return esc(text); },
    image({ text }) { return esc(text); },
    link({ href, text }) {
      const ok = /^https?:\/\//i.test(href);
      return ok ? `<a href="#" data-href="${esc(href)}" title="${esc(href)}">${esc(text)}</a>` : esc(text);
    },
  },
});
const md = (s: string) => marked.parse(s, { gfm: true, async: false }) as string;

export const isChat = (id: number) => chats.has(id);
export const get = (id: number) => chats.get(id);

export function create(model: string, onChange: () => void, onActivity: (spoke: boolean) => void): Chat {
  const id = nextId++;
  const root = document.createElement("div");
  root.className = "chat";
  root.innerHTML = `<div class="chat-info"><span class="dot ready"></span><span class="ci-text">Chat only · runs on this Mac · nothing leaves it</span></div><div class="chat-list" tabindex="0" aria-live="polite"></div>`;
  const c: Chat = {
    id, model, el: root, list: root.querySelector(".chat-list")!, info: root.querySelector(".ci-text")!,
    history: [], busy: false, ctx: START_CTX, onChange, onActivity,
  };
  c.list.addEventListener("click", (e) => {
    const a = (e.target as HTMLElement).closest<HTMLAnchorElement>("a[data-href]");
    if (a) { e.preventDefault(); invoke("open_url", { url: a.dataset.href }).catch(() => {}); }
  });
  chats.set(id, c);
  empty(c);
  invoke<Plan>("chat_plan", { model }).then((p) => {
    c.plan = p;
    c.ctx = Math.min(START_CTX, p.safe_ctx);
    showCapacity(c);
  }).catch(() => { /* Ollama may be off; plan again on first send */ });
  return c;
}

function empty(c: Chat) {
  c.list.innerHTML = `<div class="chat-empty"><b>${esc(c.model)}</b><p>Type below to chat. Attach text or code files, PDFs, Word documents or images and Eiwaz reads them for the model. Everything stays on this Mac.</p><p class="hint">Local models can talk about your files but can't change them or run commands. Use Claude Code for that.</p></div>`;
}

function showCapacity(c: Chat) {
  if (!c.plan) return;
  c.info.textContent = `Chat only · runs on this Mac · can read about ${pages(c.plan.safe_ctx).toLocaleString()} pages at once`;
}

export function dispose(id: number) {
  stop(id);
  chats.get(id)?.el.remove();
  chats.delete(id);
}

export function stop(id: number) {
  const c = chats.get(id);
  if (c?.busy) invoke("chat_stop", { id }).catch(() => {});
}

function note(c: Chat, text: string, cls = "") {
  const n = document.createElement("div");
  n.className = `chat-note ${cls}`;
  n.textContent = text;
  c.list.append(n);
  scroll(c);
}

function scroll(c: Chat) {
  const l = c.list;
  if (l.scrollHeight - l.scrollTop - l.clientHeight < 160) l.scrollTop = l.scrollHeight;
}

/** Send a message with attachments. Returns false if nothing was sent. */
export async function send(c: Chat, text: string, paths: string[]): Promise<boolean> {
  if (c.busy) { note(c, "Still answering. Press Stop (Esc) first, or wait for it to finish.", "warn"); return false; }
  if (!c.history.length) c.list.innerHTML = "";

  // The user's bubble goes up first so it's clear the message was taken.
  const bubble = document.createElement("div");
  bubble.className = "chat-msg user";
  bubble.innerHTML = `${paths.length ? `<div class="chat-files"></div>` : ""}<div class="chat-text">${esc(text)}</div>`;
  if (!text) bubble.querySelector(".chat-text")!.remove();
  c.list.append(bubble);
  c.list.scrollTop = c.list.scrollHeight;
  const filesEl = bubble.querySelector<HTMLElement>(".chat-files");

  c.busy = true;
  c.onChange();
  if (!c.plan) c.plan = await invoke<Plan>("chat_plan", { model: c.model }).catch(() => undefined);

  // Read every attachment. Problems become notes; the rest still goes.
  const texts: { name: string; body: string; pages: number; cut: boolean; places?: string }[] = [];
  const images: string[] = [];
  for (const p of paths) {
    const chip = document.createElement("span");
    chip.className = "chat-file";
    chip.innerHTML = `<span class="spinner"></span> ${esc(basename(p))}`;
    filesEl?.append(chip);
    try {
      const f = await invoke<ChatFile>("chat_file", { path: p });
      if (f.kind === "image") {
        if (c.plan && !c.plan.vision) {
          chip.className = "chat-file bad";
          chip.textContent = `${f.name} · not sent`;
          note(c, `${c.model} can't see images, so ${f.name} wasn't sent. Pick a model with vision (Gemma 4 has it).`, "warn");
          continue;
        }
        images.push(f.image);
        chip.innerHTML = `<img alt="" src="data:image/jpeg;base64,${f.image}"> ${esc(f.name)}`;
      } else {
        texts.push({ name: f.name, body: f.text, pages: f.pages, cut: f.cut });
        const words = f.text.split(/\s+/).filter(Boolean).length;
        chip.textContent = `${f.name} · ${f.pages ? `${f.pages} page${f.pages === 1 ? "" : "s"}` : `${words.toLocaleString()} words`}`;
      }
    } catch (e) {
      chip.className = "chat-file bad";
      chip.textContent = `${basename(p)} · couldn't read`;
      note(c, String(e), "warn");
    }
  }
  if (!text && !texts.length && !images.length) { c.busy = false; c.onChange(); return false; }

  // Fit everything in the model's memory: grow the window first, then drop the
  // oldest turns. A file too big for the window on its own is searched instead:
  // Eiwaz sends the passages that answer the question (or an even sample, for a summary).
  const max = c.plan?.safe_ctx ?? c.ctx;
  const ask = text || (images.length && !texts.length ? "Describe this image." : "Summarize this.");
  const m = c.model;
  const fixed = tokensOf(ask, m) + images.length * IMAGE_TOKENS + REPLY_ROOM + 200;
  let history = [...c.history];
  const histTok = () => history.reduce((n, x) => n + msgTokens(x, m), 0);
  const fileTok = () => texts.reduce((n, f) => n + tokensOf(f.body, m) + 20, 0);

  // A question about a big file attached earlier: search it again for this question.
  const followUp = !texts.length && !images.length && !!c.docs?.length && text.trim().length >= 12;
  const tooBig = texts.length > 0 && fileTok() + fixed > max;
  let dropped = 0;
  const cutNotes: string[] = [];
  let searched: SearchResult | undefined;
  if (tooBig || followUp) {
    const docs: Doc[] = tooBig ? texts.map((f) => ({ name: f.name, text: f.body, pages: f.pages })) : c.docs!;
    // Earlier excerpts are replaced by this search's: keep only the questions they came with.
    history = history.map((h) => h.role === "user" && h.content.includes(" excerpts=\"") ? { ...h, content: h.content.slice(h.content.lastIndexOf("</file>") + 7).trim() } : h);
    while (history.length && histTok() > (max - fixed) * HISTORY_SHARE) { history.splice(0, 2); dropped += 1; }
    const summary = tooBig && (!text || GENERIC.test(text));
    const room = Math.min(summary ? Infinity : QUESTION_TOKENS, max - fixed - histTok() - docs.length * 120);
    const chars = docs.reduce((n, d) => n + d.text.length, 0);
    const charsPerTok = chars / Math.max(1, docs.reduce((n, d) => n + tokensOf(d.text, m), 0));
    const query = followUp && c.lastAsk ? `${c.lastAsk}\n${text}` : ask;
    const progress = document.createElement("div");
    progress.className = "chat-note";
    progress.innerHTML = `<span class="spinner"></span> <span>${summary ? "Picking passages from across" : "Searching"} ${esc(docs.map((d) => d.name).join(" and "))}…</span>`;
    c.list.append(progress);
    scroll(c);
    c.searching = progress.querySelector("span:last-child") as HTMLElement;
    c.docs = docs;
    try {
      searched = await invoke<SearchResult>("chat_search", { id: c.id, docs, query, summary, budgetChars: Math.max(4000, Math.floor(room * charsPerTok * 0.97)) });
    } catch (e) {
      progress.remove();
      c.searching = undefined;
      if (String(e) !== "stopped") note(c, `Eiwaz couldn't search ${docs.map((d) => d.name).join(" and ")}: ${e}`, "bad");
      else note(c, "Stopped.");
      c.busy = false; c.onChange();
      return false;
    }
    progress.remove();
    c.searching = undefined;
    c.lastAsk = summary ? undefined : ask;
    const head = summary
      ? "This file is too long to send whole. Below is an even sample of passages from across it, in order. Summarize from these and say the summary is based on a sample."
      : "This file is too long to send whole. Below are only the passages most related to the question, in order, labelled with where they come from. If the answer isn't in them, say so rather than guessing.";
    texts.length = 0;
    for (const f of searched.docs) {
      if (!f.passages) continue;
      const d = docs.find((x) => x.name === f.name);
      texts.push({ name: f.name, body: `${head}\n\n${f.text}`, pages: d?.pages ?? 0, cut: false, places: f.places });
      const whole = d ? (d.pages > 1 ? `${d.pages} pages` : `about ${pages(tokensOf(d.text, m)).toLocaleString()} pages of text`) : "";
      cutNotes.push(summary
        ? `${f.name} (${whole}) is more than ${m} can hold at once (about ${pages(max).toLocaleString()} pages). Eiwaz sent an even sample from across it (${f.passages} of ${f.total_passages} passages), so the summary covers the whole file but not every detail`
        : `${followUp ? `Searched ${f.name} again for this question` : `${f.name} (${whole}) is more than ${m} can hold at once, so Eiwaz searched it`} and sent the parts most related: ${f.places}`);
    }
    if (searched.note) cutNotes.push(searched.note);
  } else {
    while (history.length && histTok() + fileTok() + fixed > max) { history.splice(0, 2); dropped += 1; }
    if (texts.length) { c.docs = undefined; c.lastAsk = undefined; }
  }
  for (const f of texts) if (f.cut) cutNotes.push(`${f.name} is very long; Eiwaz read the first 2 million characters`);

  const content = texts.length
    ? texts.map((f) => `<file name="${f.name}"${f.places ? ` excerpts="${f.places}"` : ""}>\n${f.body}\n</file>`).join("\n\n") + `\n\n${ask}`
    : ask;
  const msg: ApiMsg = { role: "user", content, ...(images.length ? { images } : {}) };
  const needed = histTok() + msgTokens(msg, m) + REPLY_ROOM;
  // Only ever grow: each change reloads the model, which takes a few seconds.
  if (needed > c.ctx) {
    const next = Math.min(max, Math.ceil(needed / 8192) * 8192);
    if (next > c.ctx) {
      note(c, `Making room: ${c.model} now holds about ${pages(next).toLocaleString()} pages at once (was ${pages(c.ctx).toLocaleString()}).`);
      c.ctx = next;
    }
  }
  if (dropped) note(c, `To fit this, the model no longer sees the earliest ${dropped === 1 ? "exchange" : `${dropped} exchanges`} of this chat.`, "warn");
  for (const n of cutNotes) note(c, `${n}.`, searched ? "" : "warn");

  c.history = history;
  c.history.push(msg);
  c.sentEstimate = needed - REPLY_ROOM;
  c.reading = texts.length ? texts.map((f) => f.places ? `${f.name}, ${f.places}` : `${f.name}${f.pages > 1 ? `, ${f.pages} pages` : ""}`).join(" and ") : images.length ? "the image" : "";
  startReply(c);
  invoke("chat_send", { id: c.id, model: c.model, messages: c.history, numCtx: c.ctx }).catch((e) => finish(c, { id: c.id, ok: false, stopped: false, error: String(e), prompt_tokens: 0, reply_tokens: 0 }));
  c.onActivity(false);
  return true;
}

function startReply(c: Chat) {
  const box = document.createElement("div");
  box.className = "chat-msg bot";
  box.innerHTML = `<div class="chat-status"><span class="spinner"></span> <span>${c.reading ? `Reading ${esc(c.reading)}…` : "Thinking…"}</span></div><details class="chat-think" hidden><summary>Thinking</summary><pre></pre></details><div class="chat-text md"></div>`;
  c.list.append(box);
  c.list.scrollTop = c.list.scrollHeight;
  c.reply = {
    body: box.querySelector(".chat-text")!, think: box.querySelector("details")!, thinkPre: box.querySelector("pre")!,
    status: box.querySelector(".chat-status")!, text: "", thought: "",
  };
}

function paint(c: Chat) {
  const r = c.reply;
  if (!r || r.timer) return;
  r.timer = window.setTimeout(() => {
    r.timer = undefined;
    r.body.innerHTML = md(r.text);
    if (r.thought) { r.think.hidden = false; r.thinkPre.textContent = r.thought; }
    scroll(c);
  }, 60);
}

interface Done { id: number; ok: boolean; stopped: boolean; error: string; prompt_tokens: number; reply_tokens: number }

function finish(c: Chat, d: Done) {
  const r = c.reply;
  c.busy = false;
  if (r) {
    clearTimeout(r.timer);
    r.status.remove();
    r.body.innerHTML = md(r.text);
    if (r.thought) { r.think.hidden = false; r.thinkPre.textContent = r.thought; }
    if (r.text) c.history.push({ role: "assistant", content: r.text });
    if (d.prompt_tokens > 500 && c.sentEstimate) {
      // Ollama counted the real tokens: correct future estimates for this model (never below 85% of ours, to stay safe).
      const ratio = (d.prompt_tokens / c.sentEstimate) * (fix.get(c.model) ?? 1);
      fix.set(c.model, Math.min(1.6, Math.max(0.85, ratio)));
    }
    else c.history.pop(); // nothing came back: let the next message retry cleanly
    if (d.stopped) note(c, "Stopped.");
    else if (!d.ok) {
      const m = /memory|alloc/i.test(d.error) ? `Your Mac ran out of memory for this. Close other apps or unload other models, then try again. (${d.error})` : `The model didn't answer: ${d.error}`;
      note(c, m, "bad");
    }
  }
  c.reply = undefined;
  scroll(c);
  c.onActivity(true);
  c.onChange();
}

export async function init() {
  await listen<{ id: number; content: string; thinking: string }>("chat-delta", (e) => {
    const c = chats.get(e.payload.id);
    if (!c?.reply) return;
    c.reply.text += e.payload.content;
    c.reply.thought += e.payload.thinking;
    const label = c.reply.status.querySelector("span:last-child");
    if (label) label.textContent = c.reply.text ? "Writing…" : "Thinking…";
    paint(c);
    c.onActivity(true);
  });
  await listen<{ id: number; text: string }>("chat-status", (e) => {
    const c = chats.get(e.payload.id);
    const label = c?.reply?.status.querySelector("span:last-child");
    if (!c || !label) return;
    const t = ({
      starting: "Starting Ollama…",
      loading: "Loading the model into memory…",
      resizing: "Making room for more text (reloading the model)…",
      reading: c.reading ? `Reading ${c.reading}…` : "Thinking…",
    } as Record<string, string>)[e.payload.text];
    if (t) label.textContent = t;
  });
  await listen<{ id: number; done: number; total: number }>("chat-search", (e) => {
    const c = chats.get(e.payload.id);
    if (!c?.searching || !e.payload.total) return;
    const { done, total } = e.payload;
    c.searching.textContent = done < total
      ? `Reading ${c.docs?.map((d) => d.name).join(" and ") ?? "the file"} to search it: ${Math.round((done * 100) / total)}% (first time only)…`
      : "Finding the passages that answer your question…";
  });
  await listen<Done>("chat-done", (e) => {
    const c = chats.get(e.payload.id);
    if (c) finish(c, e.payload);
  });
}
