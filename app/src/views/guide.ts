// Eiwaz Guide: a short list (0 to 3) of add-ons that fit THIS project, with
// plain facts, what's already set up, and setup gaps. "You need nothing" is a
// valid answer.
import { invoke } from "@tauri-apps/api/core";
import { ago, basename, bus, db, el, esc, events, info, openExternal, sheet, toast } from "../core";

interface Signal { integration: string; evidence: string }
interface Installed { name: string; app: string; scope: string; detail: string }
interface Facts { signals: Signal[]; stack: string[]; has_claude_md: boolean; has_agents_md: boolean; installed: Installed[]; skills: number; subagents: number; git_remote: string }
interface Pick { integration: string; entries: any[] }
interface Catalog { picks: Pick[]; built_at: string; fetched_at: number; offline: boolean; error: string }

const NICE: Record<string, string> = {
  stripe: "Stripe", supabase: "Supabase", postgres: "PostgreSQL", playwright: "Playwright", sentry: "Sentry", notion: "Notion", slack: "Slack",
  linear: "Linear", shopify: "Shopify", mongodb: "MongoDB", redis: "Redis", aws: "AWS", vercel: "Vercel", remotion: "Remotion", hubspot: "HubSpot",
  cloudflare: "Cloudflare", figma: "Figma", github: "GitHub", docker: "Docker",
};
const WHY: Record<string, string> = {
  stripe: "lets the agent look up your Stripe products, prices and payments itself, instead of you copying them over",
  supabase: "lets the agent see your database tables and run queries, so it stops guessing your data's shape",
  postgres: "lets the agent read your database's tables directly, so it stops guessing your data's shape",
  playwright: "lets the agent open your site in a real browser, click around and check its own work",
  sentry: "lets the agent read the real errors your users hit",
  notion: "lets the agent read and update your Notion pages",
  slack: "lets the agent read and post Slack messages",
  linear: "lets the agent read and update your Linear issues",
  shopify: "lets the agent work with your Shopify store's products and orders",
  mongodb: "lets the agent look inside your MongoDB data",
  redis: "lets the agent inspect what's stored in Redis",
  aws: "lets the agent look at your AWS resources",
  vercel: "lets the agent check your Vercel deployments and their logs",
  remotion: "gives the agent Remotion's own guidance for building videos",
  hubspot: "lets the agent read your HubSpot contacts and deals",
  cloudflare: "lets the agent check your Cloudflare Workers, logs and settings",
  figma: "lets the agent read your Figma designs, so it builds what you designed",
  github: "lets the agent read issues and pull requests on GitHub",
  docker: "lets the agent manage your Docker containers",
};
// owners that are the vendor itself
const OFFICIAL: Record<string, string[]> = {
  stripe: ["stripe"], supabase: ["supabase", "supabase-community"], playwright: ["microsoft"], sentry: ["getsentry"], notion: ["makenotion"],
  linear: ["linear"], shopify: ["shopify"], mongodb: ["mongodb-js", "mongodb"], redis: ["redis"], aws: ["awslabs", "aws"], vercel: ["vercel"],
  remotion: ["remotion-dev"], hubspot: ["hubspot"], cloudflare: ["cloudflare"], figma: ["figma"], github: ["github"], docker: ["docker"],
  slack: ["slackapi"], postgres: [],
};

const st = { facts: null as Facts | null, cat: null as Catalog | null, hidden: {} as Record<string, string[]>, open: "", loading: false, visible: false, project: "" };

export function init() {
  db.read<Record<string, string[]>>("guide-hidden", {}).then((h) => (st.hidden = h));
  events.project.on(() => { st.facts = null; st.cat = null; if (st.visible) load(); });
}
export function show() { st.visible = true; if (!st.facts || st.project !== bus.project()) load(); else draw(); }
export function hide() { st.visible = false; }

async function load(refresh = false) {
  const project = bus.project();
  st.project = project;
  if (!project) { draw(); return; }
  st.loading = true;
  draw();
  try { st.facts = await invoke<Facts>("guide_scan", { project }); } catch (e) { toast(String(e)); st.facts = null; }
  const ints = (st.facts?.signals ?? []).map((s) => s.integration);
  st.cat = ints.length || refresh ? await invoke<Catalog>("guide_catalog", { integrations: ints, refresh }) : null;
  st.loading = false;
  draw();
}

const isOfficial = (integ: string, e: any) => (OFFICIAL[integ] ?? []).includes(String(e.github?.owner ?? e.repo?.split("/")[0] ?? "").toLowerCase());
function rank(p: Pick) {
  return [...p.entries].sort((a, b) => Number(isOfficial(p.integration, b)) - Number(isOfficial(p.integration, a)) || (b.score?.total ?? 0) - (a.score?.total ?? 0));
}
const already = (e: any) => (st.facts?.installed ?? []).some((i) => i.name.toLowerCase() === String(e.name).toLowerCase() || (e.install?.["claude-code"] ?? []).some((l: string) => i.detail && l.includes(i.detail)));

// Marketplace listings and monorepos: `repo`/`github` describe the containing repo,
// and the add-on itself lives at `subpath`. Don't present the container's owner,
// stars or activity as the add-on's own.
const inMarketplace = (e: any) => /^anthropics\/claude-plugins/i.test(String(e.repo ?? ""));
const maker = (e: any) => (inMarketplace(e) ? e.author ?? "" : e.github?.owner ?? e.author ?? "");
const subdir = (e: any) => String(e.subpath ?? "").replace(/^\.\//, "");
const pageUrl = (e: any) => {
  const repo = e.github?.repo ?? e.repo;
  return subdir(e) ? `https://github.com/${repo}/tree/HEAD/${subdir(e)}` : `https://github.com/${repo}`;
};
const ticks = (s: string) => esc(s).replace(/`([^`]+)`/g, "<code>$1</code>");

function facts(e: any, integ: string) {
  const g = e.github ?? {};
  const mkt = inMarketplace(e);
  const shared = !!subdir(e);
  const age = g.ownerCreatedAt ? Math.floor((Date.now() - Date.parse(g.ownerCreatedAt)) / (365 * 86400000)) : null;
  const line = (e.install?.["claude-code"] ?? [])[0] ?? "";
  const runs = e.hosted ? `Runs on ${esc(e.github?.owner ?? "the vendor")}'s servers. What the agent asks it goes to them.` : /npx|uvx|docker|node|python/.test(line) ? "Runs on your Mac as a program, with the same access to your files as you." : e.type === "plugin" ? "Adds instructions and tools to Claude Code." : "Check its page for where it runs.";
  return `<ul class="facts">
    ${mkt
      ? `<li><b>Listed in</b> ${esc(e.origin ?? "an Anthropic plugin marketplace")}${e.author ? ` · made by ${esc(e.author)}` : " · the listing doesn't say who made it"}</li>`
      : `<li><b>Made by</b> ${esc(maker(e) || "unknown")}${isOfficial(integ, e) ? ` <span class="tag ok">${esc(NICE[integ] ?? integ)} itself</span>` : ""}${age !== null ? ` · account ${age < 1 ? "less than a year" : age + " year" + (age === 1 ? "" : "s")} old` : ""}</li>`}
    <li><b>Where it runs</b> ${runs}</li>
    <li><b>Last updated</b> ${shared
      ? `<span class="hint">not tracked for this add-on on its own.</span> It's one folder in ${esc(g.repo ?? e.repo)}${mkt ? ", which holds many plugins" : ""}${g.license ? ` · ${esc(g.license)}` : ""}`
      : `${g.pushedAt ? ago(Date.parse(g.pushedAt) / 1000) : "unknown"} · ${esc(g.license ?? "no license listed")} · ${(g.stars ?? 0).toLocaleString()} stars`}</li>
    <li><b>Safety check</b> <span class="hint">Not checked yet by Plado's scanner. Read what it can do before adding it.</span></li>
    <li><b>Working memory</b> <span class="hint">Every turn, its list of tools takes up part of the agent's working memory ${info("Working memory (context) is how much the agent can keep in mind at once. Each add-on's tool list uses some of it on every turn, even when unused.")}. Not measured yet for this one.</span></li>
  </ul>`;
}

function draw() {
  if (!st.visible) return;
  const root = el("view-guide");
  const project = bus.project();
  if (!project) { root.innerHTML = `<div class="vempty"><h1>Guide</h1><p>Open a project folder and Guide looks at what it's built with, then suggests the few add-ons (if any) worth adding.</p></div>`; return; }
  if (st.loading || !st.facts) { root.innerHTML = `<div class="vempty"><h1>Guide</h1><p><span class="spinner"></span> Looking at ${esc(basename(project))}…</p></div>`; return; }
  const f = st.facts;
  const hidden = new Set(st.hidden[project] ?? []);
  const picks = (st.cat?.picks ?? []).filter((p) => !hidden.has(p.integration) && p.entries.length).slice(0, 3);
  const setup: string[] = [];
  if (!f.has_claude_md && !f.has_agents_md) setup.push(`<div class="gcard setup"><div class="gtitle"><span class="gicon">📝</span><b>Your agents start every session knowing nothing about this project</b></div>
    <p>A short notes file (CLAUDE.md for Claude Code, AGENTS.md for Codex) tells them what the project is, how to run it and what to avoid. It's the single most useful thing to add, and it's free.</p>
    <div class="row"><button class="btn primary" id="g-init">Ask Claude Code to write one</button><span class="hint">Sends <code>/init</code>. You can read and edit the file it writes.</span></div></div>`);
  const mcpCount = f.installed.length;
  if (mcpCount > 5) setup.push(`<div class="gcard setup"><div class="gtitle"><span class="gicon">⚖️</span><b>${mcpCount} add-ons are set up</b></div><p>Each one takes up some of the agent's working memory on every turn. If you don't use some of them, removing them makes the agent sharper. See “Already set up” below.</p></div>`);

  root.innerHTML = `
    <div class="vhead">
      <div><h1>Guide</h1><p class="sub">The few add-ons worth adding to ${esc(basename(project))}, if any. Most projects need none.</p></div>
      <div class="vhead-actions"><button class="btn" id="g-refresh">Look again</button></div>
    </div>
    <div class="gfound">
      <div><span class="hint">Built with</span> ${f.stack.length ? f.stack.map((s) => `<span class="chipx">${esc(s)}</span>`).join("") : `<span class="hint">nothing recognized</span>`}</div>
      <div><span class="hint">Uses</span> ${f.signals.length ? f.signals.map((s) => `<span class="chipx" title="Found ${esc(s.evidence)}">${esc(NICE[s.integration] ?? s.integration)}</span>`).join("") : `<span class="hint">no outside services found</span>`}</div>
    </div>
    ${setup.length ? `<h2 class="sec">Start here</h2>${setup.join("")}` : ""}
    <h2 class="sec">Could help this project</h2>
    ${st.cat?.offline && !st.cat?.picks.length ? `<div class="banner">Couldn't reach the Plado catalog (${esc(st.cat.error)}). Showing project checks only.</div>` : ""}
    ${picks.length ? picks.map(pickCard).join("") : `<div class="gcard calm"><b>You probably don't need any add-ons for this project.</b><p class="hint">That's fine. Agents work well without them. Add one only when you keep doing the same thing by hand.</p></div>`}
    ${(st.hidden[project] ?? []).length ? `<p class="hint">Hidden: ${(st.hidden[project] ?? []).map((h) => esc(NICE[h] ?? h)).join(", ")} · <button class="linkbtn" id="g-unhide">Show again</button></p>` : ""}
    <h2 class="sec">Already set up</h2>
    ${f.installed.length ? `<table class="tbl"><thead><tr><th>Add-on</th><th>For</th><th>Where</th><th>What it runs</th><th></th></tr></thead><tbody>${f.installed.map((i, n) => `<tr><td><b>${esc(i.name)}</b></td><td>${esc(i.app)}</td><td>${esc(i.scope)}</td><td><code class="clip">${esc(i.detail || "—")}</code></td><td><button class="btn small" data-rm="${n}">Remove…</button></td></tr>`).join("")}</tbody></table>` : `<p class="hint">No add-ons (MCP servers) are set up for this project.${f.skills ? ` ${f.skills} skill${f.skills === 1 ? "" : "s"} in .claude/skills.` : ""}</p>`}
    <p class="hint gfoot">Suggestions come from the public Plado catalog${st.cat?.built_at ? ` (built ${new Date(st.cat.built_at).toLocaleDateString()})` : ""}, downloaded as a whole file. Nothing about your project is sent anywhere.</p>`;

  document.getElementById("g-init")?.addEventListener("click", async () => {
    const a = await bus.sendToAgent("/init", { kinds: ["claude"], start: "claude" });
    if (a) toast(`Asked ${a.name} to write CLAUDE.md. Watch it in Work.`);
  });
  el("g-refresh").onclick = () => load(true);
  document.getElementById("g-unhide")?.addEventListener("click", async () => { st.hidden[project] = []; await db.write("guide-hidden", st.hidden); draw(); });
  root.querySelectorAll<HTMLElement>("[data-hide]").forEach((b) => (b.onclick = async () => { (st.hidden[project] ??= []).push(b.dataset.hide!); await db.write("guide-hidden", st.hidden); draw(); }));
  root.querySelectorAll<HTMLElement>("[data-cmp]").forEach((b) => (b.onclick = () => { st.open = st.open === b.dataset.cmp ? "" : b.dataset.cmp!; draw(); }));
  root.querySelectorAll<HTMLElement>("[data-add]").forEach((b) => (b.onclick = () => add(b.dataset.add!, b.dataset.id!)));
  root.querySelectorAll<HTMLElement>("[data-gh]").forEach((b) => (b.onclick = () => openExternal(b.dataset.gh!)));
  root.querySelectorAll<HTMLElement>("[data-rm]").forEach((b) => (b.onclick = () => remove(f.installed[Number(b.dataset.rm)])));
}

function pickCard(p: Pick) {
  const ranked = rank(p);
  const top = ranked[0];
  const sig = st.facts!.signals.find((s) => s.integration === p.integration);
  const name = NICE[p.integration] ?? p.integration;
  const others = ranked.slice(1);
  return `<div class="gcard">
    <div class="gtitle"><span class="gicon">${esc(name.slice(0, 1))}</span><div><b>${esc(name)}: ${esc(top.registryName ?? top.name)}</b><div class="hint">Because this project uses ${esc(name)} (found ${ticks(sig?.evidence ?? "")}). It ${esc(WHY[p.integration] ?? "connects the agent to it")}.</div></div></div>
    <p>${esc(top.description ?? "")}</p>
    ${facts(top, p.integration)}
    <div class="row wrap">
      ${already(top) ? `<span class="tag ok">Already set up</span>` : `<button class="btn primary" data-add="${esc(p.integration)}" data-id="${esc(top.id)}">Add to Claude Code…</button>`}
      <button class="btn" data-gh="${esc(pageUrl(top))}">Read about it</button>
      ${others.length ? `<button class="btn" data-cmp="${esc(p.integration)}">${st.open === p.integration ? "Hide" : "Compare"} the other ${others.length}</button>` : ""}
      <span class="grow"></span><button class="linkbtn" data-hide="${esc(p.integration)}">Not for me</button>
    </div>
    ${st.open === p.integration ? `<p class="hint">Why the top pick: ${isOfficial(p.integration, top) ? `it's made by ${esc(name)} itself.` : "it has the best Plado score (popularity, upkeep, license and owner) among these."}</p><table class="tbl compact"><thead><tr><th>Add-on</th><th>Made by</th><th>Plado score</th><th>Updated</th><th></th></tr></thead><tbody>${others.map((e) => `<tr><td><b>${esc(e.registryName ?? e.name)}</b><div class="hint">${esc((e.description ?? "").slice(0, 110))}</div></td><td>${esc(maker(e) || (inMarketplace(e) ? "not stated" : ""))}${isOfficial(p.integration, e) ? " ✓" : ""}</td><td>${e.score?.total ?? "—"}</td><td>${!subdir(e) && e.github?.pushedAt ? ago(Date.parse(e.github.pushedAt) / 1000) : "—"}</td><td>${already(e) ? `<span class="tag ok">set up</span>` : `<button class="btn small" data-add="${esc(p.integration)}" data-id="${esc(e.id)}">Add…</button>`}</td></tr>`).join("")}</tbody></table>` : ""}
  </div>`;
}

// Commands come from catalog data, so only a narrow, harmless shape is ever run.
const SAFE_CMD = /^(claude|codex) mcp add [\w@./:=\- ]+$/;

async function add(integ: string, id: string) {
  const e = st.cat?.picks.find((p) => p.integration === integ)?.entries.find((x) => x.id === id);
  if (!e) return;
  const lines: string[] = e.install?.["claude-code"] ?? [];
  const slash = lines.filter((l) => l.startsWith("/"));
  const shell = lines.filter((l) => !l.startsWith("/"));
  const safe = shell.every((l) => SAFE_CMD.test(l.trim()));
  const ans = await sheet({
    title: `Add ${e.registryName ?? e.name} to Claude Code?`,
    html: `${facts(e, integ)}
      <p><b>Where:</b> ${slash.length ? "Claude Code, as a plugin" : "This project only, just for you (Claude Code's “local” setting). Other projects won't see it."}</p>
      <p><b>What Eiwaz will run</b>${slash.length ? " (typed into your Claude Code session)" : " (in a terminal tab you can watch)"}:</p>
      <pre class="cmd">${esc(lines.join("\n"))}</pre>
      ${e.hosted ? `<p class="hint">The first time the agent uses it, ${esc(NICE[integ] ?? integ)} may ask you to sign in in your browser.</p>` : `<p class="hint">If it needs a key (an API key), its page explains where to get one. Eiwaz doesn't store keys.</p>`}
      <p class="hint">Afterwards, restart your Claude Code session so it picks it up. You can remove it later from “Already set up”.</p>
      ${!safe ? `<p class="banner">This install line isn't a plain “claude mcp add” command, so Eiwaz won't run it for you. Copy it into a terminal yourself if you trust it.</p>` : ""}`,
    buttons: [{ id: "", label: "Cancel" }, { id: "copy", label: "Copy command" }, ...(safe || slash.length ? [{ id: "go", label: "Add it", kind: "primary" as const }] : [])],
  });
  if (ans === "copy") { await navigator.clipboard.writeText(lines.join("\n")); toast("Copied."); return; }
  if (ans !== "go") return;
  if (slash.length) {
    for (const l of slash) await bus.sendToAgent(l, { kinds: ["claude"], start: "claude" });
  } else {
    await bus.spawnTerminal(shell.join(" && "), `Adding ${e.name}`);
    bus.showView("work");
  }
  toast("Running it now. Restart Claude Code afterwards to use it.", 5000);
  setTimeout(() => load(), 8000);
}

async function remove(i: Installed) {
  if (!/^[\w@.\-]+$/.test(i.name)) return toast("That name has unusual characters, so remove it by hand.");
  const scope = i.scope.startsWith("all") ? "user" : i.scope.includes(".mcp.json") ? "project" : "local";
  const cmd = i.app === "Codex" ? `codex mcp remove ${i.name}` : `claude mcp remove ${i.name} -s ${scope}`;
  const ans = await sheet({
    title: `Remove ${i.name}?`,
    html: `<p>${esc(i.app)} will stop using it (${esc(i.scope)}).${i.scope.includes(".mcp.json") ? " This edits the shared .mcp.json file, so it affects anyone else using this project too." : ""}</p><pre class="cmd">${esc(cmd)}</pre><p class="hint">Restart the agent afterwards.</p>`,
    buttons: [{ id: "", label: "Cancel" }, { id: "go", label: "Remove", kind: "danger" }],
  });
  if (ans !== "go") return;
  await bus.spawnTerminal(cmd, `Removing ${i.name}`);
  toast("Removing it in a terminal tab.");
  setTimeout(() => load(), 5000);
}
