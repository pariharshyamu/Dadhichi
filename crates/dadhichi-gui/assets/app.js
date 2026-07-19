/* Dadhichi web GUI frontend.
 *
 * One WebSocket streams every kernel event (agent progress, diagnostics,
 * approvals); REST endpoints handle files. Monaco provides the editor; the
 * agent console, problems panel, and approval cards are what make it an
 * agent-native IDE rather than an editor with a chat box.
 */
"use strict";

const $ = (id) => document.getElementById(id);

const state = {
  root: "",
  ws: null,
  wsOk: false,
  editor: null,
  models: new Map(),      // rel path -> { model, savedVersion }
  openOrder: [],          // rel paths in tab order
  active: null,           // rel path
  problems: new Map(),    // normalized uri -> [{line, severity, message}]
  pendingCompletions: new Map(), // rel path -> resolve fn
  treeCache: new Map(),   // rel dir -> children rows
  phaseTimer: null,
  phase: "idle",          // mirrors the phase chip, gates steering vs new goal
  autoAllow: new Set(),   // tool names auto-approved for this session
  fileCache: new Map(),   // rel path -> last-known text (diff baselines)
  lastToolCard: null,     // the open card awaiting its tool result
};

/* ---------------- boot ---------------- */

require.config({ paths: { vs: "/vs" } });
require(["vs/editor/editor.main"], () => {
  monaco.editor.defineTheme("dadhichi-dark", {
    base: "vs-dark",
    inherit: true,
    rules: [],
    colors: {
      "editor.background": "#0d1117",
      "editorGutter.background": "#0d1117",
      "editor.lineHighlightBackground": "#141b25",
      "editorLineNumber.foreground": "#3d4856",
    },
  });
  boot();
});

async function boot() {
  const ws = await fetch("/api/workspace").then((r) => r.json());
  state.root = ws.root;
  $("workspace-name").textContent = ws.name;
  document.title = `${ws.name} — Dadhichi`;
  loadModels();
  $("model-select").addEventListener("change", async (e) => {
    const model = e.target.value;
    const res = await fetch("/api/model", {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ model }),
    });
    setStatus(res.ok ? `model → ${model}` : "model switch failed");
  });

  state.editor = monaco.editor.create($("editor"), {
    theme: "dadhichi-dark",
    automaticLayout: true,
    fontSize: 13,
    fontFamily: "Cascadia Code, Consolas, JetBrains Mono, monospace",
    minimap: { enabled: true },
    scrollBeyondLastLine: false,
    renderWhitespace: "selection",
    smoothScrolling: true,
  });
  state.editor.onDidChangeCursorPosition((e) => {
    $("cursor-pos").textContent = `Ln ${e.position.lineNumber}, Col ${e.position.column}`;
  });
  window.addEventListener("keydown", (e) => {
    if ((e.ctrlKey || e.metaKey) && e.key.toLowerCase() === "s") {
      e.preventDefault();
      saveActive();
    }
  });

  registerCompletionBridge();
  await loadTree("", $("tree"), 0);
  connectWs();
  wireBottomTabs();
  wireComposer();
  wireExplorerActions();
  wireFolderModal();
}

/* ---------------- open folder ---------------- */

const RECENT_KEY = "dadhichi.recentWorkspaces";

function recentWorkspaces() {
  try { return JSON.parse(localStorage.getItem(RECENT_KEY)) || []; }
  catch { return []; }
}

function rememberWorkspace(root) {
  const list = [root, ...recentWorkspaces().filter((r) => r !== root)].slice(0, 8);
  localStorage.setItem(RECENT_KEY, JSON.stringify(list));
}

function wireFolderModal() {
  $("workspace-name").addEventListener("click", () => {
    $("folder-modal").classList.remove("hidden");
    $("folder-path").value = state.root;
    renderRecent();
    browseFs(state.root);
  });
  $("folder-cancel").addEventListener("click", () =>
    $("folder-modal").classList.add("hidden")
  );
  $("folder-modal").addEventListener("click", (e) => {
    if (e.target === $("folder-modal")) $("folder-modal").classList.add("hidden");
  });
  $("folder-open").addEventListener("click", () => openWorkspace($("folder-path").value.trim()));
  $("folder-path").addEventListener("keydown", (e) => {
    if (e.key === "Enter") browseFs($("folder-path").value.trim());
  });
}

function renderRecent() {
  const box = $("folder-recent");
  box.innerHTML = "";
  const recents = recentWorkspaces().filter((r) => r !== state.root);
  if (!recents.length) return;
  const head = document.createElement("div");
  head.className = "fr-head";
  head.textContent = "Recent";
  box.appendChild(head);
  for (const r of recents) {
    const row = document.createElement("div");
    row.className = "fr-row";
    row.textContent = r;
    row.title = "Open this workspace";
    row.addEventListener("click", () => openWorkspace(r));
    box.appendChild(row);
  }
}

async function browseFs(path) {
  const res = await fetch(`/api/fs?path=${encodeURIComponent(path || "")}`);
  if (!res.ok) { setStatus("cannot browse that path"); return; }
  const { path: cur, parent, dirs } = await res.json();
  $("folder-path").value = cur;
  const list = $("folder-list");
  list.innerHTML = "";
  if (parent !== null && parent !== undefined) {
    const up = document.createElement("div");
    up.className = "fl-row up";
    up.textContent = "‹ up";
    up.addEventListener("click", () => browseFs(parent));
    list.appendChild(up);
  } else if (cur) {
    const roots = document.createElement("div");
    roots.className = "fl-row up";
    roots.textContent = "‹ drives";
    roots.addEventListener("click", () => browseFs(""));
    list.appendChild(roots);
  }
  for (const d of dirs) {
    const row = document.createElement("div");
    row.className = "fl-row";
    row.textContent = d.name;
    row.addEventListener("click", () => browseFs(d.path));
    list.appendChild(row);
  }
}

async function openWorkspace(root) {
  if (!root) return;
  setStatus(`opening ${root}…`);
  const res = await fetch("/api/workspace", {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ root }),
  });
  if (!res.ok) {
    setStatus(await res.text());
    return;
  }
  rememberWorkspace(root);
  // Every panel rebinds to the new root — a clean reload is the honest reset.
  location.reload();
}

/* ---------------- explorer actions ---------------- */

function wireExplorerActions() {
  $("refresh-tree").addEventListener("click", refreshTree);
  $("new-file").addEventListener("click", async () => {
    const name = prompt("New file (path relative to workspace):");
    if (!name) return;
    const res = await fetch("/api/file", {
      method: "PUT",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ path: name, text: "" }),
    });
    if (res.ok) {
      await refreshTree();
      openFile(name.replaceAll("\\", "/"));
    } else {
      setStatus(await res.text());
    }
  });
  $("new-folder").addEventListener("click", async () => {
    const name = prompt("New folder (path relative to workspace):");
    if (!name) return;
    const res = await fetch("/api/mkdir", {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ path: name }),
    });
    if (res.ok) refreshTree();
    else setStatus(await res.text());
  });
}

async function refreshTree() {
  state.treeCache.clear();
  const tree = $("tree");
  tree.innerHTML = "";
  await loadTree("", tree, 0);
}

/* ---------------- models ---------------- */

async function loadModels() {
  try {
    const { current, models } = await fetch("/api/models").then((r) => r.json());
    const sel = $("model-select");
    sel.innerHTML = "";
    for (const m of models) {
      const opt = document.createElement("option");
      opt.value = m;
      opt.textContent = m;
      sel.appendChild(opt);
    }
    sel.value = current;
  } catch {
    /* daemon unreachable — the select stays as-is */
  }
}

/* ---------------- kernel command bridge ---------------- */

async function dispatch(name, args) {
  const res = await fetch("/api/dispatch", {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ name, args: args || {} }),
  });
  if (!res.ok) throw new Error(await res.text());
  return res.json();
}

/* ---------------- integrations panel ---------------- */

let integrationsBusy = false;

async function refreshIntegrations() {
  if (integrationsBusy) return;
  integrationsBusy = true;
  const box = $("integrations");
  try {
    const [list, cat] = await Promise.all([
      dispatch("mcp.list"),
      dispatch("mcp.connectors"),
    ]);
    box.innerHTML = "";

    const head1 = document.createElement("div");
    head1.className = "int-head";
    head1.textContent = "Configured MCP servers";
    box.appendChild(head1);
    const servers = list.servers || [];
    if (!servers.length) {
      box.insertAdjacentHTML("beforeend",
        `<div class="problem-empty">None yet — add one from the catalogue below.</div>`);
    }
    for (const s of servers) {
      const row = document.createElement("div");
      row.className = "int-row";
      row.innerHTML = `
        <span class="int-dot ${s.connected ? "on" : ""}">●</span>
        <span class="int-name"></span>
        <span class="int-desc"></span>
        <span class="int-meta">${s.connected ? `${s.tool_count} tool(s)` : ""}</span>
        <button class="int-btn">${s.connected ? "Disconnect" : "Connect"}</button>`;
      row.querySelector(".int-name").textContent = s.name;
      row.querySelector(".int-desc").textContent = s.command || s.url || "";
      row.querySelector(".int-btn").addEventListener("click", async () => {
        setStatus(`${s.connected ? "disconnecting" : "connecting"} ${s.name}…`);
        try {
          await dispatch(s.connected ? "mcp.disconnect" : "mcp.connect", { server: s.name });
        } catch (e) { setStatus(String(e.message || e)); }
        integrationsBusy = false;
        refreshIntegrations();
      });
      box.appendChild(row);
    }

    const head2 = document.createElement("div");
    head2.className = "int-head";
    head2.textContent = "Add from catalogue";
    box.appendChild(head2);
    for (const c of cat.connectors || []) {
      const row = document.createElement("div");
      row.className = "int-row";
      row.innerHTML = `
        <span class="int-dot ${c.configured ? "on" : ""}">${c.configured ? "✓" : "+"}</span>
        <span class="int-name"></span>
        <span class="int-desc"></span>
        <span class="int-meta">${c.needs_secrets ? "needs secret" : ""}</span>
        <button class="int-btn" ${c.configured ? "disabled" : ""}>${c.configured ? "Added" : "Add"}</button>`;
      row.querySelector(".int-name").textContent = c.id;
      row.querySelector(".int-desc").textContent = c.description;
      const btn = row.querySelector(".int-btn");
      if (!c.configured) {
        btn.addEventListener("click", async () => {
          btn.disabled = true;
          btn.textContent = "Adding…";
          setStatus(`adding ${c.id}…`);
          try {
            await dispatch("mcp.add", { connector: c.id, connect: true });
            setStatus(`${c.id} added`);
          } catch (e) { setStatus(String(e.message || e)); }
          integrationsBusy = false;
          refreshIntegrations();
        });
      }
      box.appendChild(row);
    }
  } catch (e) {
    box.innerHTML = `<div class="problem-empty">integrations unavailable: ${escapeHtml(String(e.message || e))}</div>`;
  } finally {
    integrationsBusy = false;
  }
}

/* ---------------- explorer ---------------- */

async function loadTree(rel, container, depth) {
  let rows = state.treeCache.get(rel);
  if (!rows) {
    rows = await fetch(`/api/tree?path=${encodeURIComponent(rel)}`).then((r) => r.json());
    state.treeCache.set(rel, rows);
  }
  for (const row of rows) {
    const el = document.createElement("div");
    el.className = "node-row" + (row.is_dir ? " dir" : "");
    el.style.setProperty("--indent", `${12 + depth * 14}px`);
    el.innerHTML = `<span class="twist">${row.is_dir ? "▸" : ""}</span><span class="name"></span>`;
    el.querySelector(".name").textContent = row.name;
    container.appendChild(el);

    if (row.is_dir) {
      let expanded = false;
      let childBox = null;
      el.addEventListener("click", async () => {
        expanded = !expanded;
        el.querySelector(".twist").textContent = expanded ? "▾" : "▸";
        if (expanded && !childBox) {
          childBox = document.createElement("div");
          container.insertBefore(childBox, el.nextSibling);
          await loadTree(row.path, childBox, depth + 1);
        } else if (childBox) {
          childBox.classList.toggle("hidden", !expanded);
        }
      });
    } else {
      el.addEventListener("click", () => {
        document.querySelectorAll(".node-row.active").forEach((n) => n.classList.remove("active"));
        el.classList.add("active");
        openFile(row.path);
      });
    }
  }
}

/* ---------------- editor & tabs ---------------- */

function langUri(rel) {
  return monaco.Uri.file("/" + rel);
}

async function openFile(rel, revealLine) {
  let entry = state.models.get(rel);
  if (!entry) {
    const res = await fetch(`/api/file?path=${encodeURIComponent(rel)}`);
    if (!res.ok) {
      setStatus(`cannot open ${rel}`);
      return;
    }
    const { text } = await res.json();
    state.fileCache.set(rel, text);
    const model = monaco.editor.createModel(text, undefined, langUri(rel));
    entry = { model, savedVersion: model.getAlternativeVersionId() };
    model.onDidChangeContent(() => refreshDirty(rel));
    state.models.set(rel, entry);
    state.openOrder.push(rel);
  }
  state.active = rel;
  $("welcome").classList.add("hidden");
  state.editor.setModel(entry.model);
  applyMarkersFor(rel);
  if (revealLine) {
    state.editor.revealLineInCenter(revealLine);
    state.editor.setPosition({ lineNumber: revealLine, column: 1 });
  }
  state.editor.focus();
  renderTabs();
}

function closeFile(rel) {
  const entry = state.models.get(rel);
  if (!entry) return;
  entry.model.dispose();
  state.models.delete(rel);
  state.openOrder = state.openOrder.filter((p) => p !== rel);
  if (state.active === rel) {
    state.active = state.openOrder[state.openOrder.length - 1] || null;
    if (state.active) {
      state.editor.setModel(state.models.get(state.active).model);
    } else {
      state.editor.setModel(null);
      $("welcome").classList.remove("hidden");
    }
  }
  renderTabs();
}

function isDirty(rel) {
  const entry = state.models.get(rel);
  return !!entry && entry.model.getAlternativeVersionId() !== entry.savedVersion;
}

function refreshDirty(rel) {
  const tab = document.querySelector(`.tab[data-path="${CSS.escape(rel)}"]`);
  if (tab) tab.classList.toggle("is-dirty", isDirty(rel));
}

function renderTabs() {
  const strip = $("tabstrip");
  strip.innerHTML = "";
  for (const rel of state.openOrder) {
    const tab = document.createElement("div");
    tab.className = "tab" + (rel === state.active ? " active" : "") + (isDirty(rel) ? " is-dirty" : "");
    tab.dataset.path = rel;
    const name = rel.split("/").pop();
    tab.innerHTML = `<span class="dirty">●</span><span class="t-name"></span><span class="close">✕</span>`;
    tab.querySelector(".t-name").textContent = name;
    tab.title = rel;
    tab.addEventListener("click", (e) => {
      if (e.target.classList.contains("close")) closeFile(rel);
      else openFile(rel);
    });
    strip.appendChild(tab);
  }
}

/* Refresh an open buffer from disk after the agent (or anything else) changed
 * the file. A dirty buffer is never clobbered — the user is warned instead. */
async function reloadFromDisk(rel) {
  const entry = state.models.get(rel);
  if (!entry) return;
  if (isDirty(rel)) {
    setStatus(`⚠ ${rel} changed on disk but your buffer has unsaved edits — saving would overwrite the agent's changes`);
    return;
  }
  const res = await fetch(`/api/file?path=${encodeURIComponent(rel)}`);
  if (!res.ok) return;
  const { text } = await res.json();
  state.fileCache.set(rel, text);
  if (entry.model.getValue() !== text) {
    const view = state.active === rel ? state.editor.saveViewState() : null;
    entry.model.setValue(text);
    entry.savedVersion = entry.model.getAlternativeVersionId();
    if (view && state.active === rel) state.editor.restoreViewState(view);
    refreshDirty(rel);
    setStatus(`reloaded ${rel} — updated by the agent`);
  } else {
    entry.savedVersion = entry.model.getAlternativeVersionId();
  }
}

async function saveActive() {
  const rel = state.active;
  if (!rel) return;
  const entry = state.models.get(rel);
  const text = entry.model.getValue();
  const res = await fetch("/api/file", {
    method: "PUT",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ path: rel, text }),
  });
  if (res.ok) {
    entry.savedVersion = entry.model.getAlternativeVersionId();
    state.fileCache.set(rel, text);
    refreshDirty(rel);
    setStatus(`saved ${rel}`);
  } else {
    setStatus(`save failed: ${await res.text()}`);
  }
}

/* ---------------- completions bridge (LSP over WS) ---------------- */

const COMPLETION_LANGS = [
  "rust", "python", "go", "typescript", "javascript", "html", "css", "json",
  "java", "c", "cpp", "csharp", "ruby", "php",
];

function registerCompletionBridge() {
  for (const lang of COMPLETION_LANGS) {
    monaco.languages.registerCompletionItemProvider(lang, {
      triggerCharacters: [".", ":", "<", '"', "/"],
      provideCompletionItems: async (model, position) => {
        const rel = relOf(model);
        if (!rel || !state.wsOk) return { suggestions: [] };
        send({
          type: "completion",
          path: rel,
          text: model.getValue(),
          line: position.lineNumber - 1,
          col: position.column - 1,
        });
        const items = await new Promise((resolve) => {
          state.pendingCompletions.set(rel, resolve);
          setTimeout(() => {
            if (state.pendingCompletions.get(rel) === resolve) {
              state.pendingCompletions.delete(rel);
              resolve([]);
            }
          }, 4000);
        });
        const word = model.getWordUntilPosition(position);
        const range = new monaco.Range(
          position.lineNumber, word.startColumn, position.lineNumber, word.endColumn
        );
        return {
          suggestions: items.map((it) => ({
            label: it.label,
            insertText: it.insert || it.label,
            detail: it.detail || "",
            kind: monacoKind(it.kind),
            range,
          })),
        };
      },
    });
  }
}

function monacoKind(kind) {
  const K = monaco.languages.CompletionItemKind;
  switch ((kind || "").toLowerCase()) {
    case "fn": case "function": case "method": return K.Function;
    case "struct": case "class": return K.Class;
    case "var": case "variable": case "field": return K.Variable;
    case "mod": case "module": return K.Module;
    case "keyword": return K.Keyword;
    case "snippet": return K.Snippet;
    default: return K.Text;
  }
}

function relOf(model) {
  for (const [rel, entry] of state.models) {
    if (entry.model === model) return rel;
  }
  return null;
}

/* ---------------- websocket & events ---------------- */

function send(obj) {
  if (state.ws && state.ws.readyState === WebSocket.OPEN) {
    state.ws.send(JSON.stringify(obj));
  }
}

function connectWs() {
  const ws = new WebSocket(`ws://${location.host}/api/events`);
  state.ws = ws;
  ws.onopen = () => {
    state.wsOk = true;
    $("conn").className = "conn on";
    setStatus("connected");
  };
  ws.onclose = () => {
    state.wsOk = false;
    $("conn").className = "conn off";
    setStatus("disconnected — retrying…");
    setTimeout(connectWs, 1500);
  };
  ws.onmessage = (msg) => {
    let event;
    try { event = JSON.parse(msg.data); } catch { return; }
    handleEvent(event.topic, event.payload || {});
    logEvent(event.topic, event.payload);
  };
}

function handleEvent(topic, p) {
  switch (topic) {
    case "agent.status": {
      const status = p.status || "";
      if (["completed", "failed", "error", "idle", "cancelled"].includes(status)) setPhase("idle");
      else if (["planning", "replanning"].includes(status)) setPhase("thinking");
      else if (status === "running") setPhase("thinking");
      // Agent runs create and edit files — keep the explorer and every open
      // buffer honest (a stale buffer + Ctrl+S would undo the agent's work).
      if (status === "completed") {
        refreshTree();
        for (const rel of state.openOrder) reloadFromDisk(rel);
      }
      chatEvent(`● ${status}`, "plan");
      break;
    }
    case "agent.plan": {
      const steps = Array.isArray(p.steps) ? p.steps.length : "?";
      chatEvent(`▤ plan: ${steps} step(s)`, "plan");
      break;
    }
    case "agent.tool": {
      setPhase("running");
      state.lastToolCard = toolCard(p.tool || "tool", p.args || {});
      break;
    }
    case "agent.tool.result": {
      attachToolResult(p);
      // The agent changed a file: refresh its open buffer immediately, so a
      // later Ctrl+S can't clobber the agent's fix with stale editor text.
      if ((p.tool === "fs.write" || p.tool === "fs.edit") && p.result && p.result.path) {
        const written = String(p.result.path);
        const rel = matchOpenPath(written);
        if (rel) reloadFromDisk(rel);
        else if (p.tool === "fs.write") state.fileCache.delete(written);
      }
      break;
    }
    case "agent.tool.error": {
      attachToolError(p);
      break;
    }
    case "agent.steered": {
      chatEvent(`↪ steering delivered: ${trim(p.text || "", 160)}`, "deleg");
      break;
    }
    case "agent.lesson": {
      chatEvent(`☆ lesson kept for future runs: ${trim(p.lesson || "", 200)}`, "plan");
      break;
    }
    case "agent.message": {
      chatBubble(p.text || p.message || compact(p), "agent");
      break;
    }
    case "agent.delegated": {
      setPhase("spawned");
      chatEvent(`⇥ delegated → ${p.agent || p.subagent || "specialist"}`, "deleg");
      break;
    }
    case "agent.error":
    case "mock.error": {
      setPhase("idle");
      chatEvent(`✗ ${p.error || compact(p)}`, "err");
      break;
    }
    case "terminal.result": {
      chatEvent(trim(p.output ?? compact(p), 1200), "term");
      break;
    }
    case "terminal.error": {
      chatEvent(`✗ ${p.error || compact(p)}`, "err");
      break;
    }
    case "agent.approval": {
      approvalCard(p);
      break;
    }
    case "agent.approval.resolved": {
      resolveCard(p.id, p.decision);
      break;
    }
    case "lsp.diagnostics": {
      applyDiagnostics(p);
      break;
    }
    case "lsp.completion": {
      const rel = matchOpenPath(p.path || "");
      const resolve = rel && state.pendingCompletions.get(rel);
      if (resolve) {
        state.pendingCompletions.delete(rel);
        resolve(Array.isArray(p.items) ? p.items : []);
      }
      break;
    }
    case "lsp.status": {
      setStatus(String(p.message || ""));
      break;
    }
    case "model.changed": {
      const sel = $("model-select");
      if (p.model && sel.value !== p.model) {
        if (![...sel.options].some((o) => o.value === p.model)) {
          const opt = document.createElement("option");
          opt.value = p.model;
          opt.textContent = p.model;
          sel.appendChild(opt);
        }
        sel.value = p.model;
      }
      setStatus(`model → ${p.model || "?"}`);
      break;
    }
    case "mcp.connected":
    case "mcp.disconnected":
    case "mcp.added":
    case "mcp.error": {
      if (!$("integrations").classList.contains("hidden")) refreshIntegrations();
      if (topic === "mcp.error") chatEvent(`✗ mcp: ${p.error || compact(p)}`, "err");
      break;
    }
    default:
      break;
  }
}

/* ---------------- agent console ---------------- */

function wireComposer() {
  const input = $("goal");
  input.addEventListener("keydown", (e) => {
    if (e.key === "Enter" && !e.shiftKey) {
      e.preventDefault();
      const text = input.value.trim();
      if (!text) return;
      input.value = "";
      if (state.phase !== "idle") {
        // A run is in flight — steer it instead of starting another.
        chatBubble(`↪ ${text}`, "user");
        send({ type: "steer", text });
      } else {
        chatBubble(text, "user");
        setPhase("thinking");
        send({ type: "goal", goal: text });
      }
    }
  });
  $("stop-btn").addEventListener("click", () => {
    send({ type: "stop" });
    setStatus("stopping the run…");
  });
}

function chatBubble(text, who) {
  const el = document.createElement("div");
  el.className = `msg ${who}`;
  el.textContent = text;
  appendChat(el);
}

function chatEvent(text, cls) {
  const el = document.createElement("div");
  el.className = `evt ${cls || ""}`;
  el.textContent = text;
  appendChat(el);
}

function appendChat(el) {
  const chat = $("chat");
  const stick = chat.scrollTop + chat.clientHeight >= chat.scrollHeight - 24;
  chat.appendChild(el);
  if (stick) chat.scrollTop = chat.scrollHeight;
}

/* ---------------- tool cards & diffs ---------------- */

/* A short human handle for a tool call: the path, command, or query. */
function argPreview(tool, args) {
  const v = args.path || args.command || args.query || args.pattern || args.stack || "";
  return trim(String(v), 80);
}

function toolCard(tool, args) {
  const el = document.createElement("div");
  el.className = "tool-card";
  el.innerHTML = `
    <div class="tc-head">
      <span class="tc-status">…</span>
      <span class="tc-tool"></span>
      <span class="tc-preview"></span>
      <span class="tc-toggle">▸</span>
    </div>
    <div class="tc-body hidden"></div>`;
  el.querySelector(".tc-tool").textContent = tool;
  el.querySelector(".tc-preview").textContent = argPreview(tool, args);
  const body = el.querySelector(".tc-body");

  // Expandable detail: a diff for edits, raw args otherwise.
  let expanded = false;
  el.querySelector(".tc-head").addEventListener("click", () => {
    expanded = !expanded;
    el.querySelector(".tc-toggle").textContent = expanded ? "▾" : "▸";
    body.classList.toggle("hidden", !expanded);
    if (expanded && !body.dataset.filled) {
      body.dataset.filled = "1";
      fillToolDetail(body, tool, args);
    }
  });
  appendChat(el);
  return { el, tool, args, body };
}

function fillToolDetail(body, tool, args) {
  if (tool === "fs.write" && typeof args.content === "string") {
    const old = state.fileCache.get(String(args.path)) ?? "";
    mountDiff(body, String(args.path || "file.txt"), old, args.content);
    return;
  }
  if (tool === "fs.edit" && typeof args.find === "string") {
    mountDiff(body, String(args.path || "snippet.txt"), args.find, String(args.replace ?? ""));
    return;
  }
  const pre = document.createElement("pre");
  pre.className = "tc-raw";
  pre.textContent = trim(JSON.stringify(args, null, 2), 4000);
  body.appendChild(pre);
}

function attachToolResult(p) {
  const card = state.lastToolCard;
  if (!card || card.tool !== p.tool) {
    // A result with no matching card (e.g. the automatic code.check) gets a
    // compact line of its own.
    const label = p.tool === "code.check"
      ? checkSummary(p.result)
      : `✓ ${p.tool}: ${trim(compact(p.result), 220)}`;
    chatEvent(label, p.tool === "code.check" && (p.result?.count || 0) > 0 ? "err" : "ok");
    return;
  }
  card.el.querySelector(".tc-status").textContent = "✓";
  card.el.classList.add("ok");
  const r = p.result || {};
  const note = r.bytes != null ? `${r.bytes} bytes`
    : r.replacements != null ? `${r.replacements} replacement(s)`
    : r.status != null ? `exit ${r.status}`
    : r.count != null ? `${r.count} match(es)`
    : "";
  card.el.querySelector(".tc-preview").textContent =
    `${argPreview(card.tool, card.args)}${note ? "  ·  " + note : ""}`;
  // After a successful write, the new content is the next diff baseline.
  if (card.tool === "fs.write" && typeof card.args.content === "string" && r.path) {
    state.fileCache.set(String(r.path), card.args.content);
  }
  state.lastToolCard = null;
}

function checkSummary(result) {
  const count = (result && result.count) || 0;
  return count > 0
    ? `⚠ code.check: ${count} problem(s) — sent back to the agent to fix`
    : `✓ code.check: clean`;
}

function attachToolError(p) {
  const card = state.lastToolCard;
  if (card && card.tool === p.tool) {
    card.el.querySelector(".tc-status").textContent = "✗";
    card.el.classList.add("err");
    const msg = document.createElement("div");
    msg.className = "tc-error";
    msg.textContent = trim(String(p.error || "failed"), 300);
    card.el.appendChild(msg);
    state.lastToolCard = null;
  } else {
    chatEvent(`✗ ${p.tool}: ${trim(String(p.error || ""), 300)}`, "err");
  }
}

let diffCounter = 0;
function mountDiff(container, path, original, modified) {
  const host = document.createElement("div");
  host.className = "diff-host";
  container.appendChild(host);
  const id = ++diffCounter;
  const name = path.split("/").pop() || "file.txt";
  const orig = monaco.editor.createModel(original ?? "", undefined,
    monaco.Uri.file(`/dadhichi-diff/${id}/a/${name}`));
  const mod = monaco.editor.createModel(modified ?? "", undefined,
    monaco.Uri.file(`/dadhichi-diff/${id}/b/${name}`));
  const editor = monaco.editor.createDiffEditor(host, {
    readOnly: true,
    renderSideBySide: false,
    automaticLayout: true,
    minimap: { enabled: false },
    lineNumbers: "off",
    folding: false,
    scrollBeyondLastLine: false,
    theme: "dadhichi-dark",
  });
  editor.setModel({ original: orig, modified: mod });
}

function approvalCard(p) {
  // Session auto-allow: the user already trusted this tool — approve at once,
  // leave a compact audit line.
  if (p.tool && state.autoAllow.has(p.tool)) {
    send({ type: "approval", id: p.id, approve: true });
    chatEvent(`✓ auto-allowed ${p.tool} (${trim(p.summary || "", 120)})`, "ok");
    return;
  }
  realApprovalCard(p);
}

function realApprovalCard(p) {
  const el = document.createElement("div");
  el.className = "approval";
  el.dataset.id = p.id || "";
  el.innerHTML = `
    <div class="a-head">⚠ approval required — ${escapeHtml(p.permission || "")}</div>
    <div class="a-sum"></div>
    <div class="a-preview"></div>
    <div class="a-actions">
      <button class="allow">Allow</button>
      <button class="always" title="Auto-approve this tool for the rest of the session">Always (session)</button>
      <button class="deny">Deny</button>
      <span class="a-verdict"></span>
    </div>`;
  el.querySelector(".a-sum").textContent = p.summary || p.tool || "";

  // See exactly what you're approving: a diff for edits, not a truncated blob.
  const args = p.args || {};
  const preview = el.querySelector(".a-preview");
  if (p.tool === "fs.write" && typeof args.content === "string") {
    const old = state.fileCache.get(String(args.path)) ?? "";
    mountDiff(preview, String(args.path || "file.txt"), old, args.content);
  } else if (p.tool === "fs.edit" && typeof args.find === "string") {
    mountDiff(preview, String(args.path || "snippet.txt"), args.find, String(args.replace ?? ""));
  }

  el.querySelector(".allow").addEventListener("click", () =>
    send({ type: "approval", id: p.id, approve: true })
  );
  el.querySelector(".always").addEventListener("click", () => {
    if (p.tool) state.autoAllow.add(p.tool);
    send({ type: "approval", id: p.id, approve: true });
    setStatus(`${p.tool} auto-allowed for this session`);
  });
  el.querySelector(".deny").addEventListener("click", () =>
    send({ type: "approval", id: p.id, approve: false })
  );
  appendChat(el);
}

function resolveCard(id, decision) {
  const card = document.querySelector(`.approval[data-id="${CSS.escape(id || "")}"]`);
  if (card) {
    card.classList.add("resolved");
    card.querySelector(".a-verdict").textContent =
      decision === "approve" ? "✓ allowed" : "✗ denied";
  }
}

function setPhase(phase) {
  state.phase = phase;
  const el = $("agent-phase");
  el.className = `phase ${phase}`;
  $("phase-label").textContent = phase;
  const active = phase !== "idle";
  $("stop-btn").classList.toggle("hidden", !active);
  $("goal").placeholder = active
    ? "Message the running agent…  (Enter to send)"
    : "Describe a goal for the agent…  (Enter to run)";
}

/* ---------------- problems & markers ---------------- */

function normalizeUri(u) {
  let s = String(u || "");
  if (s.startsWith("file://")) s = s.slice(7);
  s = s.replaceAll("\\", "/").replaceAll("%20", " ");
  if (/^\/[a-zA-Z]:/.test(s)) s = s.slice(1);
  if (/^[A-Z]:/.test(s)) s = s[0].toLowerCase() + s.slice(1);
  return s;
}

function matchOpenPath(uriOrPath) {
  const n = normalizeUri(uriOrPath);
  for (const rel of state.models.keys()) {
    const nr = normalizeUri(rel);
    if (n === nr || n.endsWith("/" + nr)) return rel;
  }
  return null;
}

function applyDiagnostics(p) {
  const uri = normalizeUri(p.uri);
  const diags = (p.diagnostics || []).map((d) => ({
    line: ((d.range && d.range.start && d.range.start.line) || 0) + 1,
    severity: typeof d.severity === "string" ? d.severity : sevName(d.severity),
    message: d.message || "",
  }));
  if (diags.length) state.problems.set(uri, diags);
  else state.problems.delete(uri);
  renderProblems();
  const rel = matchOpenPath(p.uri);
  if (rel) applyMarkersFor(rel);
}

function sevName(n) {
  return { 1: "error", 2: "warning", 3: "information", 4: "hint" }[n] || "error";
}

function applyMarkersFor(rel) {
  const entry = state.models.get(rel);
  if (!entry) return;
  const nr = normalizeUri(rel);
  let diags = [];
  for (const [uri, list] of state.problems) {
    if (uri === nr || uri.endsWith("/" + nr)) diags = diags.concat(list);
  }
  const S = monaco.MarkerSeverity;
  monaco.editor.setModelMarkers(entry.model, "dadhichi", diags.map((d) => ({
    startLineNumber: d.line, startColumn: 1,
    endLineNumber: d.line, endColumn: entry.model.getLineMaxColumn(Math.min(d.line, entry.model.getLineCount())),
    message: d.message,
    severity: d.severity === "error" ? S.Error
      : d.severity === "warning" ? S.Warning
      : d.severity === "hint" ? S.Hint : S.Info,
  })));
}

function renderProblems() {
  const box = $("problems");
  box.innerHTML = "";
  let count = 0;
  let hasError = false;
  for (const [uri, diags] of state.problems) {
    for (const d of diags) {
      count += 1;
      if (d.severity === "error") hasError = true;
      const short = uri.split("/").slice(-2).join("/");
      const row = document.createElement("div");
      row.className = "problem-row";
      row.innerHTML = `<span class="sev ${d.severity}">${d.severity === "error" ? "✗" : "▲"}</span>
        <span class="p-msg"></span><span class="loc"></span>`;
      row.querySelector(".p-msg").textContent = d.message;
      row.querySelector(".loc").textContent = `${short}:${d.line}`;
      row.addEventListener("click", () => {
        const rel = matchOpenPath(uri) || relFromUri(uri);
        if (rel) openFile(rel, d.line);
      });
      box.appendChild(row);
    }
  }
  if (!count) {
    box.innerHTML = `<div class="problem-empty">No problems detected in synced files.</div>`;
  }
  const badge = $("problem-count");
  badge.textContent = String(count);
  badge.classList.toggle("hot", hasError);
}

/* A workspace-relative path from an absolute diagnostic uri, when possible. */
function relFromUri(uri) {
  const n = normalizeUri(uri);
  const root = normalizeUri(state.root);
  if (n.startsWith(root + "/")) return n.slice(root.length + 1);
  return null;
}

/* ---------------- bottom panel & misc ---------------- */

function wireBottomTabs() {
  document.querySelectorAll(".btab").forEach((btn) => {
    btn.addEventListener("click", () => {
      document.querySelectorAll(".btab").forEach((b) => b.classList.remove("active"));
      btn.classList.add("active");
      for (const id of ["problems", "integrations", "log"]) {
        $(id).classList.toggle("hidden", btn.dataset.tab !== id);
      }
      if (btn.dataset.tab === "integrations") refreshIntegrations();
    });
  });
}

function logEvent(topic, payload) {
  const box = $("log");
  const stick = box.scrollTop + box.clientHeight >= box.scrollHeight - 24;
  const line = document.createElement("div");
  line.className = "log-line";
  line.innerHTML = `<span class="topic"></span> <span class="body"></span>`;
  line.querySelector(".topic").textContent = topic;
  line.querySelector(".body").textContent = trim(compact(payload), 240);
  box.appendChild(line);
  while (box.childElementCount > 500) box.removeChild(box.firstChild);
  if (stick) box.scrollTop = box.scrollHeight;
}

let statusTimer = null;
function setStatus(text) {
  $("status-text").textContent = text;
  clearTimeout(statusTimer);
  statusTimer = setTimeout(() => { $("status-text").textContent = ""; }, 6000);
}

function compact(v) {
  if (v == null) return "";
  if (typeof v === "string") return v;
  try { return JSON.stringify(v); } catch { return String(v); }
}

function trim(s, n) {
  s = String(s);
  return s.length > n ? s.slice(0, n) + "…" : s;
}

function escapeHtml(s) {
  return String(s).replace(/[&<>"']/g, (c) => ({
    "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;",
  }[c]));
}
