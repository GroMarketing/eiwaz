// Browser harness for Eiwaz's UI: real frontend, mocked Tauri backend, real Ollama.
// Run `npx vite`, open http://localhost:1420/ui-test.html. No macOS permissions needed.
import { mockIPC, mockWindows } from "@tauri-apps/api/mocks";
import { emit } from "@tauri-apps/api/event";

mockWindows("main");
try { localStorage.setItem("project", "/tmp/eiwaz-ui-test"); } catch { /* ignore */ }

const longDoc = Array.from({ length: 950 }, (_, i) => `Paragraph ${i + 1}. ` + "The quarterly plan covers hiring, budget and launch timing in detail. ".repeat(6)).join("\n\n")
  + "\n\nThe secret code word is PINEAPPLE.";
// ~200 pages, ~600K characters: more than the 131K-token window, so it must be searched.
const hugeDoc = Array.from({ length: 200 }, (_, i) => `Section ${i + 1}. ` + "The committee reviewed the hiring plan and vendor invoices again. ".repeat(46)
  + (i === 162 ? "Our office cat, a grey tabby named Biscuit, sleeps on the server rack every afternoon. " : "")).join("\f");
const FILES: Record<string, unknown> = {
  "/tmp/eiwaz-ui-test/huge.pdf": { name: "huge.pdf", kind: "text", text: hugeDoc, image: "", pages: 200, cut: false },
  "/tmp/eiwaz-ui-test/note.md": { name: "note.md", kind: "text", text: "# Note\nThe secret code word is MANGO.", image: "", pages: 0, cut: false },
  "/tmp/eiwaz-ui-test/report.pdf": { name: "report.pdf", kind: "text", text: longDoc, image: "", pages: 95, cut: false },
};
const aborts = new Map<number, AbortController>();

async function chatSend(a: any) {
  const ctl = new AbortController();
  aborts.set(a.id, ctl);
  (window as any).__lastSend = { numCtx: a.numCtx, messages: a.messages.map((m: any) => ({ role: m.role, chars: m.content.length, head: m.content.slice(0, 80) })) };
  await emit("chat-status", { id: a.id, text: "loading" });
  await fetch("http://127.0.0.1:11434/api/generate", { method: "POST", body: JSON.stringify({ model: a.model, keep_alive: "15m", options: { num_ctx: a.numCtx } }) });
  await emit("chat-status", { id: a.id, text: "reading" });
  try {
    const r = await fetch("http://127.0.0.1:11434/api/chat", {
      method: "POST", signal: ctl.signal,
      body: JSON.stringify({ model: a.model, messages: a.messages, stream: true, think: false, options: { num_ctx: a.numCtx } }),
    });
    const rd = r.body!.getReader(); const dec = new TextDecoder(); let buf = "", p = 0, e = 0;
    for (;;) {
      const { value, done } = await rd.read(); if (done) break;
      buf += dec.decode(value, { stream: true });
      let i; while ((i = buf.indexOf("\n")) >= 0) {
        const line = buf.slice(0, i); buf = buf.slice(i + 1); if (!line.trim()) continue;
        const v = JSON.parse(line);
        if (v.error) throw new Error(v.error);
        await emit("chat-delta", { id: a.id, content: v.message?.content ?? "", thinking: v.message?.thinking ?? "" });
        if (v.done) { p = v.prompt_eval_count; e = v.eval_count; }
      }
    }
    await emit("chat-done", { id: a.id, ok: true, stopped: false, error: "", prompt_tokens: p, reply_tokens: e });
  } catch (err) {
    const stopped = ctl.signal.aborted;
    await emit("chat-done", { id: a.id, ok: false, stopped, error: stopped ? "" : String(err), prompt_tokens: 0, reply_tokens: 0 });
  }
}

// Stand-in for the Rust search (tested on its own with cargo): exact words, or an even sample for a summary.
async function chatSearch(a: any) {
  (window as any).__lastSearch = { query: a.query, summary: a.summary, budgetChars: a.budgetChars };
  const all: { doc: number; start: number; end: number; page: number; text: string }[] = [];
  a.docs.forEach((d: any, doc: number) => {
    for (let s = 0; s < d.text.length; s += 1500) {
      const t = d.text.slice(s, s + 1500);
      all.push({ doc, start: s, end: s + t.length, page: d.text.slice(0, s).split("\f").length, text: t });
    }
  });
  await emit("chat-search", { id: a.id, done: 0, total: all.length });
  const q = new Set(String(a.query).toLowerCase().split(/\W+/).filter((w) => w.length > 3));
  const score = (t: string) => t.toLowerCase().split(/\W+/).filter((w) => q.has(w)).length;
  const n = Math.max(1, Math.floor(a.budgetChars / 1540));
  const order = a.summary ? Array.from({ length: n }, (_, j) => Math.floor((j * all.length) / n)) : all.map((_, i) => i).sort((x, y) => score(all[y].text) - score(all[x].text)).slice(0, n);
  await emit("chat-search", { id: a.id, done: all.length, total: all.length });
  return {
    semantic: false, note: "",
    docs: a.docs.map((d: any, i: number) => {
      const picked = order.map((k) => all[k]).filter((c) => c.doc === i).sort((x, y) => x.start - y.start);
      const pages = [...new Set(picked.map((c) => c.page))];
      return { name: d.name, text: picked.map((c) => `[page ${c.page}]\n${c.text.replace(/\f/g, "\n")}`).join("\n\n[…]\n\n"), places: `pages ${pages.slice(0, 6).join(", ")}${pages.length > 6 ? "…" : ""}`, passages: picked.length, total_passages: all.filter((c) => c.doc === i).length };
    }),
  };
}

mockIPC(async (cmd, a: any) => {
  switch (cmd) {
    case "sessions_sweep": return new URLSearchParams(location.search).has("sweep")
      ? { unclean: true, ended: ["claude", "node", "node"], summary: "claude, node ×2" }
      : { unclean: false, ended: [], summary: "" };
    case "detect_tools": return { claude: "/usr/local/bin/claude" };
    case "ollama_status": return { up: true, loaded: [], installed: ["gemma4:latest"] };
    case "system_stats": return { mem_used: 20e9, mem_total: 48e9, claude_mem: 0, codex_mem: 0, ollama_mem: 0 };
    case "chat_plan": return { max_ctx: 131072, safe_ctx: 131072, vision: true };
    case "chat_file": { const f = FILES[a.path]; if (!f) throw `${a.path.split("/").pop()} isn't something a model can read.`; return f; }
    case "chat_send": chatSend(a); return null;
    case "chat_search": return chatSearch(a);
    case "chat_stop": aborts.get(a.id)?.abort(); return null;
    case "stage_file": return a.path;
    default: return null;
  }
}, { shouldMockEvents: true });

(window as any).__drop = (paths: string[]) => emit("tauri://drag-drop", { paths, position: { x: 10, y: 10 } });
