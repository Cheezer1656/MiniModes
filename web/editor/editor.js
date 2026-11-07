// Minimodes script editor: file tree, Monaco, console and collaborators,
// talking to the lobby's /api. No build step; Monaco loads from jsDelivr.
"use strict";

const MONACO = "https://cdn.jsdelivr.net/npm/monaco-editor@0.52.2/min";
const POLL_MS = 10000;
const MAX_CONSOLE_LINES = 2000;
const MAX_TOTAL_BYTES = 2 << 20;
const ENTRY = "main.luau";
const PATH_RE = /^(?:[A-Za-z0-9_-][A-Za-z0-9_.-]*\/)*[A-Za-z0-9_-][A-Za-z0-9_.-]*\.luau?$/;

const $ = (id) => document.getElementById(id);
const el = (tag, props = {}, ...children) => {
  const node = Object.assign(document.createElement(tag), props);
  for (const child of children) node.append(child);
  return node;
};

const state = {
  session: null,
  game: null, // { id, name, role, owner }
  files: new Map(), // path -> { version, size, updated_by, updated_at }
  open: new Map(), // path -> { model, version, savedAlt, viewState, stale }
  tabs: [], // open paths, in tab order
  active: null,
  collapsed: new Set(),
  levels: new Set(["info", "warn", "error", "system"]),
  markers: new Map(), // path -> monaco markers from the console
};
let monaco = null;
let editor = null;

// ---- API ----------------------------------------------------------------

class ApiError extends Error {
  constructor(status, data) {
    super((data && data.error) || `request failed (${status})`);
    this.status = status;
    this.data = data;
  }
}

async function api(method, url, body) {
  const response = await fetch(url, {
    method,
    credentials: "same-origin",
    headers: { "X-Minimodes": "1", ...(body !== undefined ? { "Content-Type": "application/json" } : {}) },
    body: body !== undefined ? JSON.stringify(body) : undefined,
  });
  const text = await response.text();
  let data = null;
  try { data = text ? JSON.parse(text) : null; } catch { data = null; }
  if (!response.ok) {
    if (response.status === 401 && url !== "/api/session") showSignin("Your session ended.");
    throw new ApiError(response.status, data);
  }
  return data;
}

const gameUrl = (rest) => `/api/games/${state.game.id}${rest}`;
const fileUrl = (path) => gameUrl(`/files/${path.split("/").map(encodeURIComponent).join("/")}`);

// ---- Status and dialogs ---------------------------------------------------

let statusTimer = 0;
function status(text, isError = false, sticky = false) {
  const node = $("status");
  node.textContent = text;
  node.classList.toggle("error", isError);
  clearTimeout(statusTimer);
  if (!sticky) statusTimer = setTimeout(() => { node.textContent = ""; }, isError ? 8000 : 3000);
}

/// Shows the dialog and resolves with the value of the button pressed.
function dialog({ title, body = [], buttons, wide = false, onOpen }) {
  const dlg = $("dialog");
  dlg.classList.toggle("wide", wide);
  $("dialog-title").textContent = title;
  $("dialog-body").replaceChildren(...body);
  // Cancel-like buttons aren't submit buttons, so Enter picks the real action.
  $("dialog-buttons").replaceChildren(
    ...buttons.map((b) => el("button", {
      className: `btn ${b.kind || ""}`,
      value: b.value,
      textContent: b.label,
      type: b.value === "cancel" ? "button" : "submit",
      onclick: b.value === "cancel" ? () => dlg.close("cancel") : null,
    })),
  );
  return new Promise((resolve) => {
    dlg.onclose = () => resolve(dlg.returnValue || "cancel");
    dlg.returnValue = "";
    dlg.showModal();
    if (onOpen) onOpen(dlg);
  });
}

async function prompt(title, label, value, validate) {
  const input = el("input", { type: "text", value, spellcheck: false, autocomplete: "off" });
  const error = el("p", { className: "error-text" });
  const form = $("dialog-form");
  const check = (e) => {
    if (e.submitter && e.submitter.value !== "ok") return;
    const message = validate ? validate(input.value.trim()) : null;
    if (message) {
      e.preventDefault();
      error.textContent = message;
    }
  };
  form.addEventListener("submit", check);
  const choice = await dialog({
    title,
    body: [el("label", { className: "muted small", textContent: label }), input, error],
    buttons: [{ label: "Cancel", value: "cancel", kind: "ghost" }, { label: "OK", value: "ok", kind: "primary" }],
    onOpen: () => { input.focus(); input.select(); },
  });
  form.removeEventListener("submit", check);
  return choice === "ok" ? input.value.trim() : null;
}

const confirmDialog = async (title, text, okLabel = "OK", kind = "primary") =>
  (await dialog({
    title,
    body: [el("p", { textContent: text })],
    buttons: [{ label: "Cancel", value: "cancel", kind: "ghost" }, { label: okLabel, value: "ok", kind }],
  })) === "ok";

function validPath(path, except) {
  if (!PATH_RE.test(path) || path.split("/").some((p) => p.startsWith("."))) {
    return "Use letters, digits, _ - . and / for folders, ending in .luau";
  }
  if (path.length > 200) return "That path is too long";
  if (path !== except && state.files.has(path)) return "A file with that name already exists";
  return null;
}

function ago(unix) {
  const s = Math.max(0, Date.now() / 1000 - unix);
  if (s < 60) return "just now";
  if (s < 3600) return `${Math.floor(s / 60)} min ago`;
  if (s < 86400) return `${Math.floor(s / 3600)} h ago`;
  return new Date(unix * 1000).toLocaleDateString();
}

// ---- Startup --------------------------------------------------------------

function showSignin(message) {
  $("app").hidden = true;
  $("signin").hidden = false;
  if (message) $("signin-msg").textContent = message;
}

async function main() {
  try {
    state.session = await api("GET", "/api/session");
  } catch (e) {
    showSignin(e.status === 401 ? "You're not signed in." : e.message);
    return;
  }
  const games = state.session.games;
  if (!games.length) {
    showSignin("You don't have any games yet. Create one from My Games in the lobby.");
    return;
  }
  const wanted = Number(new URLSearchParams(location.search).get("game"));
  state.game = games.find((g) => g.id === wanted) || games[0];
  if (state.game.id !== wanted) history.replaceState(null, "", `?game=${state.game.id}`);

  const select = $("game-select");
  for (const g of games) {
    const label = g.role === "owner" ? g.name : `${g.name} (${g.owner})`;
    select.append(el("option", { value: g.id, textContent: label, selected: g.id === state.game.id }));
  }
  select.onchange = () => {
    if (dirtyPaths().length && !window.confirm("You have unsaved changes. Switch games anyway?")) {
      select.value = state.game.id;
      return;
    }
    window.onbeforeunload = null;
    location.search = `?game=${select.value}`;
  };
  $("who").textContent = state.session.player.name;
  document.title = `${state.game.name} · Minimodes Script Editor`;
  $("app").hidden = false;

  bindUi();
  connectConsole();
  await loadMonaco();
  await refreshFiles();
  const first = state.files.has(ENTRY) ? ENTRY : [...state.files.keys()][0];
  if (first) await openFile(first);
  updateView();
  setInterval(pollFiles, POLL_MS);
}

function loadMonaco() {
  window.MonacoEnvironment = {
    getWorkerUrl() {
      const source = `self.MonacoEnvironment={baseUrl:"${MONACO}/"};importScripts("${MONACO}/vs/base/worker/workerMain.js");`;
      return URL.createObjectURL(new Blob([source], { type: "text/javascript" }));
    },
  };
  window.require.config({ paths: { vs: `${MONACO}/vs` } });
  return new Promise((resolve, reject) => {
    window.require(["vs/editor/editor.main"], () => {
      monaco = window.monaco;
      setupLuau();
      const dark = () => window.matchMedia("(prefers-color-scheme: dark)").matches;
      editor = monaco.editor.create($("editor"), {
        theme: dark() ? "minimodes-dark" : "minimodes-light",
        automaticLayout: true,
        fontFamily: getComputedStyle(document.body).getPropertyValue("--mono"),
        fontSize: 13.5,
        tabSize: 4,
        insertSpaces: false,
        minimap: { enabled: false },
        scrollBeyondLastLine: false,
        renderWhitespace: "selection",
        fixedOverflowWidgets: true,
        model: null,
      });
      window.matchMedia("(prefers-color-scheme: dark)").addEventListener("change", () =>
        monaco.editor.setTheme(dark() ? "minimodes-dark" : "minimodes-light"),
      );
      editor.addCommand(monaco.KeyMod.CtrlCmd | monaco.KeyCode.KeyS, () => saveActive());
      editor.addCommand(monaco.KeyMod.CtrlCmd | monaco.KeyCode.Enter, () => run());
      editor.onDidChangeModelContent(() => renderDirty());
      resolve();
    }, reject);
  });
}

// ---- Files ------------------------------------------------------------------

async function refreshFiles() {
  const list = await api("GET", gameUrl("/files"));
  state.files = new Map(list.map((f) => [f.path, f]));
  renderTree();
}

/// Picks up other people's saves: reloads open files with no local edits,
/// flags the ones with edits, and updates the tree.
async function pollFiles() {
  if (document.hidden) return;
  let list;
  try {
    list = await api("GET", gameUrl("/files"));
  } catch {
    return;
  }
  const before = state.files;
  state.files = new Map(list.map((f) => [f.path, f]));
  for (const [path, open] of state.open) {
    const now = state.files.get(path);
    if (!now) {
      if (before.has(path)) open.stale = "deleted";
      continue;
    }
    if (now.version === open.version) continue;
    if (isDirty(path)) {
      open.stale = now.updated_by || "someone";
    } else {
      await reloadFromServer(path);
    }
  }
  renderTree();
  updateView();
}

async function reloadFromServer(path) {
  const open = state.open.get(path);
  const file = await api("GET", fileUrl(path));
  const view = state.active === path ? editor.saveViewState() : null;
  open.model.setValue(file.source);
  open.version = file.version;
  open.savedAlt = open.model.getAlternativeVersionId();
  open.stale = null;
  if (view) editor.restoreViewState(view);
}

const isDirty = (path) => {
  const open = state.open.get(path);
  return !!open && open.model.getAlternativeVersionId() !== open.savedAlt;
};
const dirtyPaths = () => [...state.open.keys()].filter(isDirty);

async function openFile(path) {
  if (!state.open.has(path)) {
    let file;
    try {
      file = await api("GET", fileUrl(path));
    } catch (e) {
      status(`Couldn't open ${path}: ${e.message}`, true);
      return;
    }
    const uri = monaco.Uri.from({ scheme: "file", path: `/${path}` });
    const model = monaco.editor.getModel(uri) || monaco.editor.createModel(file.source, "luau", uri);
    model.setValue(file.source);
    state.open.set(path, { model, version: file.version, savedAlt: model.getAlternativeVersionId(), viewState: null });
    applyMarkers(path);
  }
  if (!state.tabs.includes(path)) state.tabs.push(path);
  activate(path);
}

function activate(path) {
  if (state.active && state.open.has(state.active)) state.open.get(state.active).viewState = editor.saveViewState();
  state.active = path;
  const open = path && state.open.get(path);
  editor.setModel(open ? open.model : null);
  if (open && open.viewState) editor.restoreViewState(open.viewState);
  if (open) editor.focus();
  for (const dir of ancestors(path || "")) state.collapsed.delete(dir);
  $("sidebar").classList.remove("open");
  renderTree();
  updateView();
}

async function closeTab(path) {
  if (isDirty(path) && !(await confirmDialog("Discard changes?", `${path} has unsaved changes.`, "Discard", "danger"))) return;
  const open = state.open.get(path);
  state.open.delete(path);
  if (open) open.model.dispose();
  const i = state.tabs.indexOf(path);
  state.tabs.splice(i, 1);
  if (state.active === path) {
    state.active = null;
    activate(state.tabs[Math.min(i, state.tabs.length - 1)] || null);
  } else {
    updateView();
  }
}

function moduleTemplate(path) {
  if (path === ENTRY) {
    return [
      "-- The game starts here. Save, then press Run (Ctrl+Enter) to restart the draft's scripts.",
      "",
      "game.Players.PlayerAdded:Connect(function(player)",
      "\tplayer:SendMessage(\"Welcome to the game, \" .. player.Name .. \"!\")",
      "end)",
      "",
    ].join("\n");
  }
  const name = path.split("/").pop().replace(/\.luau?$/, "");
  const local = name === "init" ? "module" : name.replace(/[^A-Za-z0-9_]/g, "_");
  return `-- Load with require("${path.replace(/(\/init)?\.luau?$/, "")}")\nlocal ${local} = {}\n\nreturn ${local}\n`;
}

async function createFile(path) {
  try {
    await api("PUT", fileUrl(path), { source: moduleTemplate(path), base: 0 });
  } catch (e) {
    status(e.message, true);
    return;
  }
  await refreshFiles();
  await openFile(path);
}

async function newFile(folder = "") {
  const path = await prompt("New file", "Path, e.g. lib/util.luau", folder ? `${folder}/` : "", (p) => validPath(p));
  if (path) await createFile(path);
}

async function renameFile(from) {
  const to = await prompt("Rename", "New path", from, (p) => (p === from ? "Pick a different name" : validPath(p, from)));
  if (!to) return;
  if (isDirty(from)) await saveFile(from);
  try {
    await api("POST", gameUrl("/rename"), { from, to });
  } catch (e) {
    status(e.message, true);
    return;
  }
  const wasOpen = state.open.has(from);
  const tab = state.tabs.indexOf(from);
  if (wasOpen) {
    state.open.get(from).model.dispose();
    state.open.delete(from);
  }
  await refreshFiles();
  if (tab >= 0) {
    state.tabs[tab] = to;
    if (state.active === from) state.active = null;
    await openFile(to);
  }
  status(`Renamed to ${to}`);
}

async function deleteFile(path) {
  if (!(await confirmDialog("Delete file?", `Delete ${path}? You can restore it from its history later.`, "Delete", "danger"))) return;
  try {
    await api("DELETE", fileUrl(path));
  } catch (e) {
    if (e.status !== 404) {
      status(e.message, true);
      return;
    }
  }
  if (state.open.has(path)) {
    state.open.get(path).model.dispose();
    state.open.delete(path);
    const i = state.tabs.indexOf(path);
    state.tabs.splice(i, 1);
    if (state.active === path) {
      state.active = null;
      activate(state.tabs[Math.min(i, state.tabs.length - 1)] || null);
    }
  }
  await refreshFiles();
  updateView();
  status(`Deleted ${path}`);
}

/// Saves one file. Returns false if it wasn't saved.
async function saveFile(path, overwrite = false) {
  const open = state.open.get(path);
  if (!open) return true;
  const alt = open.model.getAlternativeVersionId();
  try {
    const result = await api("PUT", fileUrl(path), {
      source: open.model.getValue(),
      base: overwrite ? undefined : open.version,
    });
    open.version = result.version;
    open.savedAlt = alt;
    open.stale = null;
    const meta = state.files.get(path) || {};
    state.files.set(path, { ...meta, path, version: result.version, size: new Blob([open.model.getValue()]).size });
    renderTree();
    updateView();
    return true;
  } catch (e) {
    if (e.status !== 409) {
      status(`Couldn't save ${path}: ${e.message}`, true, true);
      return false;
    }
    const choice = await dialog({
      title: "Someone else saved first",
      body: [el("p", { textContent: `${e.message}. Keep your version, or load theirs and lose your changes?` })],
      buttons: [
        { label: "Cancel", value: "cancel", kind: "ghost" },
        { label: "Load theirs", value: "theirs" },
        { label: "Overwrite with mine", value: "mine", kind: "danger" },
      ],
    });
    if (choice === "mine") return saveFile(path, true);
    if (choice === "theirs") await reloadFromServer(path);
    renderDirty();
    return false;
  }
}

async function saveActive() {
  if (!state.active) return;
  if (await saveFile(state.active)) {
    status(`Saved ${state.active}`);
    if ($("auto-reload").checked) await reload();
  }
}

async function saveAll() {
  for (const path of dirtyPaths()) {
    if (!(await saveFile(path))) return false;
  }
  return true;
}

async function run() {
  if (await saveAll()) await reload();
}

async function reload() {
  try {
    const { running } = await api("POST", gameUrl("/reload"));
    if (running) {
      clearMarkers();
      status("Restarting scripts…");
    } else {
      status("Saved. The draft isn't open, so scripts start when someone joins it.", false);
    }
  } catch (e) {
    status(e.message, true);
  }
}

// ---- Rendering --------------------------------------------------------------

function ancestors(path) {
  const parts = path.split("/");
  return parts.slice(0, -1).map((_, i) => parts.slice(0, i + 1).join("/"));
}

function renderTree() {
  const tree = $("tree");
  const root = { dirs: new Map(), files: [] };
  for (const path of [...state.files.keys()].sort()) {
    let node = root;
    const parts = path.split("/");
    parts.slice(0, -1).forEach((part, i) => {
      if (!node.dirs.has(part)) node.dirs.set(part, { dirs: new Map(), files: [], path: parts.slice(0, i + 1).join("/") });
      node = node.dirs.get(part);
    });
    node.files.push(path);
  }
  const rows = [];
  const walk = (node, depth) => {
    for (const [name, dir] of [...node.dirs].sort(([a], [b]) => a.localeCompare(b))) {
      const collapsed = state.collapsed.has(dir.path);
      const row = el("div", { className: "tree-row", tabIndex: 0, title: dir.path },
        el("span", { className: "folder-icon", textContent: collapsed ? "▸" : "▾" }),
        el("span", { className: "name", textContent: name }),
        el("span", { className: "actions" }, actionButton("＋", "New file here", () => newFile(dir.path))),
      );
      row.style.setProperty("--depth", depth);
      row.onclick = () => {
        if (collapsed) state.collapsed.delete(dir.path); else state.collapsed.add(dir.path);
        renderTree();
      };
      rows.push(row);
      if (!collapsed) walk(dir, depth + 1);
    }
    for (const path of node.files) {
      const name = path.split("/").pop();
      const meta = state.files.get(path);
      const row = el("div", {
        className: `tree-row file${path === state.active ? " active" : ""}${path === ENTRY ? " entry" : ""}`,
        tabIndex: 0,
        title: `${path}\n${meta.size} bytes · saved ${ago(meta.updated_at || Date.now() / 1000)}${meta.updated_by ? ` by ${meta.updated_by}` : ""}`,
      },
        el("span", { className: "folder-icon", textContent: "" }),
        el("span", { className: "name", textContent: name }),
        el("span", { className: "dirty", textContent: isDirty(path) ? "●" : "" }),
        el("span", { className: "actions" },
          actionButton("✎", "Rename", () => renameFile(path)),
          actionButton("✕", "Delete", () => deleteFile(path)),
        ),
      );
      row.style.setProperty("--depth", depth);
      row.onclick = () => openFile(path);
      row.onkeydown = (e) => { if (e.key === "Enter") openFile(path); };
      rows.push(row);
    }
  };
  walk(root, 0);
  tree.replaceChildren(...rows);
  const total = [...state.files.values()].reduce((sum, f) => sum + f.size + f.path.length, 0);
  $("usage").textContent = `${(total / 1024).toFixed(total < 10240 ? 1 : 0)} / ${MAX_TOTAL_BYTES / 1024} KB`;
}

function actionButton(text, title, action) {
  return el("button", {
    className: "icon-btn", textContent: text, title, ariaLabel: title,
    onclick: (e) => { e.stopPropagation(); action(); },
  });
}

function renderTabs() {
  $("tabs").replaceChildren(...state.tabs.map((path) => {
    const tab = el("div", {
      className: `tab${path === state.active ? " active" : ""}`, role: "tab", title: path,
      onclick: () => activate(path),
      onauxclick: (e) => { if (e.button === 1) closeTab(path); },
    },
      el("span", { textContent: path.split("/").pop() }),
      el("span", { className: "mark", textContent: isDirty(path) ? "●" : "" }),
      el("span", { className: "close", textContent: "×", title: "Close", onclick: (e) => { e.stopPropagation(); closeTab(path); } }),
    );
    return tab;
  }));
}

function renderDirty() {
  renderTabs();
  for (const row of $("tree").querySelectorAll(".tree-row.file")) {
    const path = row.title.split("\n")[0];
    row.querySelector(".dirty").textContent = isDirty(path) ? "●" : "";
  }
  window.onbeforeunload = dirtyPaths().length ? (e) => { e.preventDefault(); return ""; } : null;
}

function updateView() {
  const hasFiles = state.files.size > 0;
  $("empty").hidden = !!state.active;
  $("empty-title").textContent = hasFiles ? "No file open" : "No scripts yet";
  $("empty-text").textContent = hasFiles ? "Pick a file from the sidebar." : "Games start at main.luau. Create it to get going.";
  $("create-main").hidden = state.files.has(ENTRY);
  $("editor").hidden = !state.active;
  $("history").disabled = !state.active;
  const open = state.active && state.open.get(state.active);
  const banner = $("banner");
  banner.hidden = !(open && open.stale);
  if (!banner.hidden) {
    const deleted = open.stale === "deleted";
    banner.replaceChildren(
      el("span", {
        textContent: deleted
          ? "Someone deleted this file. Saving puts it back."
          : `${open.stale} saved a newer version of this file. Saving will overwrite it.`,
      }),
      deleted ? "" : el("button", {
        className: "btn small",
        textContent: "Load theirs",
        onclick: async () => { await reloadFromServer(state.active); updateView(); },
      }),
    );
  }
  renderDirty();
}

// ---- Console ----------------------------------------------------------------

let consoleSocket = null;
let consoleRetry = 1000;

function connectConsole() {
  const proto = location.protocol === "https:" ? "wss:" : "ws:";
  const socket = new WebSocket(`${proto}//${location.host}${gameUrl("/console")}`);
  consoleSocket = socket;
  const dot = $("console-state");
  socket.onopen = () => {
    consoleRetry = 1000;
    dot.classList.add("on");
    dot.title = "Connected";
  };
  socket.onmessage = (event) => {
    const msg = JSON.parse(event.data);
    if (msg.type === "history") {
      $("console-lines").replaceChildren();
      msg.lines.forEach((line) => addLine(line, false));
    } else if (msg.type === "line") {
      addLine(msg, true);
    } else if (msg.type === "skipped") {
      addLine({ at: Date.now(), level: "system", text: `${msg.count} lines skipped (the console fell behind)` }, true);
    } else if (msg.type === "revoked") {
      showSignin("You no longer have access to this game.");
    }
  };
  socket.onclose = () => {
    dot.classList.remove("on");
    dot.title = "Disconnected; reconnecting…";
    if (consoleSocket !== socket || !$("signin").hidden) return;
    setTimeout(connectConsole, consoleRetry);
    consoleRetry = Math.min(consoleRetry * 2, 30000);
  };
}

const LOCATION_RE = /([A-Za-z0-9_\-./]+\.luau?):(\d+)/g;

function addLine(line, live) {
  const box = $("console-lines");
  const stick = box.scrollTop + box.clientHeight >= box.scrollHeight - 8;
  const text = el("span", { className: "text" });
  let last = 0;
  for (const match of line.text.matchAll(LOCATION_RE)) {
    text.append(line.text.slice(last, match.index));
    const [, path, lineNo] = match;
    text.append(el("a", { textContent: match[0], onclick: () => jump(path, Number(lineNo)) }));
    last = match.index + match[0].length;
  }
  text.append(line.text.slice(last));
  const time = new Date(line.at).toLocaleTimeString([], { hour12: false });
  const row = el("div", { className: `line ${line.level}` }, el("span", { className: "time", textContent: time }), text);
  row.hidden = !state.levels.has(line.level);
  box.append(row);
  while (box.childElementCount > MAX_CONSOLE_LINES) box.firstElementChild.remove();
  if (stick) box.scrollTop = box.scrollHeight;

  if (live && line.level === "system" && /scripts started|reloaded the scripts/.test(line.text)) clearMarkers();
  if (line.level === "error") markError(line.text);
}

async function jump(path, lineNo) {
  if (!state.files.has(path)) return;
  await openFile(path);
  editor.revealLineInCenter(lineNo);
  editor.setPosition({ lineNumber: lineNo, column: 1 });
  editor.focus();
}

/// Underlines the first location an error names.
function markError(text) {
  const match = [...text.matchAll(LOCATION_RE)][0];
  if (!match || !state.files.has(match[1])) return;
  const [, path, lineNo] = match;
  const message = text.slice(match.index + match[0].length).replace(/^:\s*/, "").split("\n")[0] || text;
  const markers = state.markers.get(path) || [];
  markers.push({ severity: 8, message, startLineNumber: Number(lineNo), endLineNumber: Number(lineNo), startColumn: 1, endColumn: 1000 });
  state.markers.set(path, markers);
  applyMarkers(path);
}

function applyMarkers(path) {
  const open = state.open.get(path);
  if (open && monaco) monaco.editor.setModelMarkers(open.model, "console", state.markers.get(path) || []);
}

function clearMarkers() {
  const paths = [...state.markers.keys()];
  state.markers.clear();
  paths.forEach(applyMarkers);
}

// ---- History ----------------------------------------------------------------

async function showHistory() {
  const path = state.active;
  if (!path) return;
  let revisions;
  try {
    revisions = await api("GET", gameUrl(`/history/${path.split("/").map(encodeURIComponent).join("/")}`));
  } catch (e) {
    status(e.message, true);
    return;
  }
  const list = el("ul", { className: "list" });
  const diffBox = el("div", { className: "diff" });
  const wrap = el("div", { className: "history" }, list, diffBox);
  let diff = null;
  let chosen = null;
  const current = state.open.get(path).model;
  const original = monaco.editor.createModel("", "luau");
  const select = async (rev, item) => {
    for (const li of list.children) li.classList.remove("selected");
    item.classList.add("selected");
    const data = await api("GET", gameUrl(`/revisions/${rev.id}`));
    chosen = data.source;
    original.setValue(data.source ?? "");
  };
  revisions.forEach((rev, i) => {
    const item = el("li", {},
      el("span", { className: "grow", textContent: rev.deleted ? "Deleted" : `${rev.size} bytes` }),
      el("span", { className: "muted small", textContent: `${rev.updated_by || "?"} · ${ago(rev.updated_at)}${i === 0 ? " · latest" : ""}` }),
    );
    item.onclick = () => select(rev, item);
    list.append(item);
  });
  if (!revisions.length) list.append(el("li", { className: "muted", textContent: "No saved revisions yet." }));

  const choice = await dialog({
    title: `History of ${path}`,
    wide: true,
    body: [wrap, el("p", { className: "muted small", textContent: "Left: the selected revision. Right: your current text. Restoring puts the revision in the editor; save to keep it." })],
    buttons: [{ label: "Close", value: "cancel", kind: "ghost" }, { label: "Restore selected", value: "restore", kind: "primary" }],
    onOpen: () => {
      diff = monaco.editor.createDiffEditor(diffBox, { readOnly: true, automaticLayout: true, renderSideBySide: window.innerWidth > 760, minimap: { enabled: false } });
      diff.setModel({ original, modified: current });
      if (revisions.length) select(revisions[0], list.firstElementChild);
    },
  });
  if (diff) diff.dispose();
  original.dispose();
  if (choice === "restore" && chosen != null) {
    current.pushEditOperations([], [{ range: current.getFullModelRange(), text: chosen }], () => null);
    renderDirty();
    status("Restored into the editor. Save to keep it.");
  }
}

// ---- Collaborators ----------------------------------------------------------

async function showPeople() {
  let info;
  try {
    info = await api("GET", gameUrl("/collaborators"));
  } catch (e) {
    status(e.message, true);
    return;
  }
  const list = el("ul", { className: "list" });
  const error = el("p", { className: "error-text" });
  const me = state.session.player.uuid;
  const render = () => {
    list.replaceChildren(el("li", {}, el("span", { className: "grow", textContent: info.owner.name }), el("span", { className: "muted small", textContent: "owner" })));
    for (const c of info.collaborators) {
      const canRemove = info.can_manage || c.uuid === me;
      list.append(el("li", {},
        el("span", { className: "grow", textContent: c.name }),
        canRemove ? el("button", {
          type: "button", className: "btn ghost small", textContent: c.uuid === me ? "Leave" : "Remove",
          onclick: async () => {
            try {
              await api("DELETE", gameUrl(`/collaborators/${c.uuid}`));
            } catch (e) {
              error.textContent = e.message;
              return;
            }
            if (c.uuid === me) location.href = "/editor/";
            info.collaborators = info.collaborators.filter((x) => x.uuid !== c.uuid);
            render();
          },
        }) : "",
      ));
    }
  };
  render();
  const body = [
    el("p", { className: "muted", textContent: "Collaborators can build in the draft and edit its scripts." }),
    list,
  ];
  if (info.can_manage) {
    const input = el("input", { type: "text", placeholder: "Minecraft username", autocomplete: "off", spellcheck: false });
    const add = el("button", {
      type: "button", className: "btn primary", textContent: "Invite",
      onclick: async () => {
        const name = input.value.trim();
        if (!name) return;
        try {
          const added = await api("POST", gameUrl("/collaborators"), { name });
          info.collaborators.push(added);
          input.value = "";
          error.textContent = "";
          render();
        } catch (e) {
          error.textContent = e.message;
        }
      },
    });
    input.onkeydown = (e) => { if (e.key === "Enter") { e.preventDefault(); add.click(); } };
    body.push(el("div", { style: "display:flex;gap:8px;margin-top:12px" }, input, add));
    body.push(el("p", { className: "muted small", textContent: `They must have joined the server once. Up to ${info.max} collaborators.` }));
  }
  body.push(error);
  await dialog({ title: "People", body, buttons: [{ label: "Done", value: "cancel", kind: "primary" }] });
}

// ---- UI wiring --------------------------------------------------------------

function bindUi() {
  $("save").onclick = saveActive;
  $("run").onclick = run;
  $("history").onclick = showHistory;
  $("people").onclick = showPeople;
  $("new-file").onclick = () => newFile();
  $("create-main").onclick = () => createFile(ENTRY);
  $("toggle-sidebar").onclick = () => $("sidebar").classList.toggle("open");
  $("console-clear").onclick = () => $("console-lines").replaceChildren();
  $("logout").onclick = async () => {
    if (dirtyPaths().length && !(await confirmDialog("Sign out?", "You have unsaved changes.", "Sign out", "danger"))) return;
    window.onbeforeunload = null;
    try { await api("POST", "/api/logout"); } catch { /* already gone */ }
    showSignin("Signed out.");
  };
  try {
    const saved = localStorage.getItem("mm-auto-reload");
    if (saved !== null) $("auto-reload").checked = saved === "1";
  } catch { /* storage unavailable */ }
  $("auto-reload").onchange = (e) => {
    try { localStorage.setItem("mm-auto-reload", e.target.checked ? "1" : "0"); } catch { /* ignore */ }
  };
  for (const box of document.querySelectorAll("#console input[data-level]")) {
    box.onchange = () => {
      if (box.checked) state.levels.add(box.dataset.level); else state.levels.delete(box.dataset.level);
      for (const row of $("console-lines").children) row.hidden = !state.levels.has([...row.classList][1]);
    };
  }
  document.addEventListener("keydown", (e) => {
    const mod = e.ctrlKey || e.metaKey;
    if (mod && e.key === "s") { e.preventDefault(); saveActive(); }
    if (mod && e.key === "Enter") { e.preventDefault(); run(); }
  });
  // Drag the console's top edge to resize it.
  const consoleBox = $("console");
  consoleBox.addEventListener("pointerdown", (e) => {
    if (e.offsetY > 4 || e.target !== consoleBox) return;
    const startY = e.clientY;
    const startH = consoleBox.getBoundingClientRect().height;
    const move = (m) => {
      const h = Math.min(window.innerHeight * 0.8, Math.max(80, startH + startY - m.clientY));
      $("app").style.setProperty("--console-h", `${h}px`);
    };
    const up = () => { window.removeEventListener("pointermove", move); window.removeEventListener("pointerup", up); };
    window.addEventListener("pointermove", move);
    window.addEventListener("pointerup", up);
  });
}

// ---- Luau language ----------------------------------------------------------

function setupLuau() {
  monaco.languages.register({ id: "luau", extensions: [".luau", ".lua"], aliases: ["Luau"] });
  monaco.languages.setLanguageConfiguration("luau", {
    comments: { lineComment: "--", blockComment: ["--[[", "]]"] },
    brackets: [["{", "}"], ["[", "]"], ["(", ")"]],
    autoClosingPairs: [
      { open: "{", close: "}" }, { open: "[", close: "]" }, { open: "(", close: ")" },
      { open: '"', close: '"', notIn: ["string"] }, { open: "'", close: "'", notIn: ["string"] },
      { open: "`", close: "`", notIn: ["string"] },
    ],
    surroundingPairs: [{ open: "{", close: "}" }, { open: "(", close: ")" }, { open: '"', close: '"' }, { open: "'", close: "'" }],
    indentationRules: {
      increaseIndentPattern: /^\s*((\b(else|function|then|do|repeat)\b((?!\b(end|until)\b).)*)|(\{\s*)|(\(\s*))$/,
      decreaseIndentPattern: /^\s*((\b(elseif|else|end|until)\b)|(\})|(\)))/,
    },
    onEnterRules: [],
  });
  monaco.languages.setMonarchTokensProvider("luau", {
    defaultToken: "",
    keywords: [
      "and", "break", "continue", "do", "else", "elseif", "end", "false", "for", "function", "if", "in",
      "local", "nil", "not", "or", "repeat", "return", "then", "true", "until", "while", "type", "export", "typeof",
    ],
    builtins: ["game", "task", "Vector3", "Item", "Region", "print", "warn", "require", "time", "error", "assert",
      "pairs", "ipairs", "next", "select", "tostring", "tonumber", "pcall", "xpcall", "setmetatable", "getmetatable",
      "rawget", "rawset", "rawequal", "rawlen", "unpack", "math", "string", "table", "coroutine", "os", "utf8", "bit32", "buffer", "self"],
    operators: ["+", "-", "*", "/", "//", "%", "^", "#", "==", "~=", "<=", ">=", "<", ">", "=", "+=", "-=", "*=", "/=", "//=", "%=", "^=", "..=", "..", "...", "->", "::"],
    symbols: /[=><~?:&|+\-*/^%#.]+/,
    escapes: /\\(?:[abfnrtvz\\"'`{]|x[0-9A-Fa-f]{2}|u\{[0-9A-Fa-f]+\}|\d{1,3})/,
    tokenizer: {
      root: [
        [/--\[(=*)\[/, { token: "comment", next: "@longcomment.$1" }],
        [/--.*$/, "comment"],
        [/\[(=*)\[/, { token: "string", next: "@longstring.$1" }],
        [/[a-zA-Z_]\w*/, { cases: { "@keywords": "keyword", "@builtins": "type.identifier", "@default": "identifier" } }],
        [/[{}()[\]]/, "@brackets"],
        [/@symbols/, { cases: { "@operators": "operator", "@default": "" } }],
        [/0[xX][0-9a-fA-F_]+/, "number.hex"],
        [/0[bB][01_]+/, "number"],
        [/\d[\d_]*(\.[\d_]*)?([eE][-+]?\d+)?/, "number"],
        [/\.\d[\d_]*([eE][-+]?\d+)?/, "number"],
        [/"/, { token: "string.quote", next: "@string.\"" }],
        [/'/, { token: "string.quote", next: "@string.'" }],
        [/`/, { token: "string.quote", next: "@interp" }],
        [/[;,]/, "delimiter"],
      ],
      string: [
        [/[^\\"']+/, "string"],
        [/@escapes/, "string.escape"],
        [/\\./, "string.escape.invalid"],
        [/["']/, { cases: { "$#==$S2": { token: "string.quote", next: "@pop" }, "@default": "string" } }],
      ],
      interp: [
        [/[^\\`{]+/, "string"],
        [/@escapes/, "string.escape"],
        [/\{/, { token: "delimiter.bracket", next: "@interpExpr" }],
        [/`/, { token: "string.quote", next: "@pop" }],
      ],
      interpExpr: [
        [/\}/, { token: "delimiter.bracket", next: "@pop" }],
        { include: "root" },
      ],
      longstring: [
        [/[^\]]+/, "string"],
        [/\](=*)\]/, { cases: { "$1==$S2": { token: "string", next: "@pop" }, "@default": "string" } }],
        [/\]/, "string"],
      ],
      longcomment: [
        [/[^\]]+/, "comment"],
        [/\](=*)\]/, { cases: { "$1==$S2": { token: "comment", next: "@pop" }, "@default": "comment" } }],
        [/\]/, "comment"],
      ],
    },
  });
  const colors = (dark) => ({
    base: dark ? "vs-dark" : "vs",
    inherit: true,
    rules: [
      { token: "keyword", foreground: dark ? "ff7b72" : "cf222e" },
      { token: "type.identifier", foreground: dark ? "79c0ff" : "0550ae" },
      { token: "string", foreground: dark ? "a5d6ff" : "0a3069" },
      { token: "string.escape", foreground: dark ? "79c0ff" : "0550ae" },
      { token: "number", foreground: dark ? "f2cc60" : "953800" },
      { token: "comment", foreground: dark ? "8b949e" : "6e7781", fontStyle: "italic" },
      { token: "operator", foreground: dark ? "ff7b72" : "cf222e" },
    ],
    colors: {
      "editor.background": dark ? "#161a20" : "#ffffff",
      "editorGutter.background": dark ? "#161a20" : "#ffffff",
      "editor.lineHighlightBackground": dark ? "#1c2129" : "#f3f5f7",
    },
  });
  monaco.editor.defineTheme("minimodes-dark", colors(true));
  monaco.editor.defineTheme("minimodes-light", colors(false));

  fetch("/editor/minimodes.d.luau")
    .then((r) => r.text())
    .then((text) => { apiTypes = parseDefinitions(text); })
    .catch(() => {});
  monaco.languages.registerCompletionItemProvider("luau", { triggerCharacters: [".", ":"], provideCompletionItems: complete });
  monaco.languages.registerHoverProvider("luau", { provideHover: hover });
}

// ---- API types from minimodes.d.luau ----------------------------------------

let apiTypes = { types: {}, globals: {} };

/// A small reader for the definition file's shapes: `export type X = {…}`,
/// `declare x: {…}` and `declare function f(…): T`. Fields keep the comment
/// lines above them as docs.
function parseDefinitions(source) {
  const types = {};
  const globals = {};
  const declRe = /(?:^|\n)((?:--[^\n]*\n)*)(export type (\w+)(?:<[^>]*>)?\s*=|declare (\w+)\s*:|declare function (\w+))/g;
  let match;
  while ((match = declRe.exec(source))) {
    const doc = cleanDoc(match[1]);
    const start = declRe.lastIndex;
    if (match[5]) {
      const end = source.indexOf("\n", start);
      globals[match[5]] = { type: `function${source.slice(start, end).trim()}`, doc, fn: true };
      continue;
    }
    const rest = source.slice(start);
    const body = readType(rest);
    const parsed = body.trim().startsWith("{") ? parseTable(body.trim()) : null;
    if (match[3]) types[match[3]] = { fields: parsed, alias: parsed ? null : body.trim(), doc };
    else globals[match[4]] = { type: body.trim(), fields: parsed, doc };
    declRe.lastIndex = start + body.length;
  }
  return { types, globals };
}

function cleanDoc(lines) {
  return lines.split("\n").map((l) => l.replace(/^--\s?/, "")).filter(Boolean).join(" ");
}

/// The text of one type expression: up to the end of a balanced `{…}` or line.
function readType(text) {
  let depth = 0;
  for (let i = 0; i < text.length; i++) {
    const c = text[i];
    if (c === "-" && text[i + 1] === "-" && depth > 0) {
      const nl = text.indexOf("\n", i);
      i = nl < 0 ? text.length : nl;
      continue;
    }
    if (c === "-" && text[i + 1] === ">") { i++; continue; }
    if ("{(<".includes(c)) depth++;
    else if ("})>".includes(c)) {
      depth--;
      if (depth === 0 && c === "}") return text.slice(0, i + 1);
    } else if (c === "\n" && depth === 0 && text.slice(0, i).trim()) return text.slice(0, i);
  }
  return text;
}

function parseTable(body) {
  const inner = body.slice(1, -1);
  const fields = {};
  let depth = 0;
  let current = "";
  let doc = [];
  const flush = () => {
    const m = /^\s*(\w+)\s*:\s*([\s\S]*?)\s*$/.exec(current);
    if (m) {
      const type = m[2];
      const isTable = type.startsWith("{");
      fields[m[1]] = {
        type,
        doc: doc.join(" "),
        // Overloads are written `((self: …) -> …) & (…)`.
        method: /^\(*\s*self\b/.test(type),
        fn: type.startsWith("(") || type.startsWith("<"),
        fields: isTable ? parseTable(type) : null,
      };
    }
    current = "";
    doc = [];
  };
  for (const rawLine of inner.split("\n")) {
    const line = rawLine.trim();
    if (line.startsWith("--")) {
      // Nested tables keep their comments for their own fields' docs.
      if (depth === 0) doc.push(line.replace(/^--\s?/, "")); else current += `${line}\n`;
      continue;
    }
    for (let i = 0; i < line.length; i++) {
      const c = line[i];
      if (c === "-" && line[i + 1] === ">") { current += "->"; i++; continue; }
      if (c === "-" && line[i + 1] === "-") break;
      if ("{(<".includes(c)) depth++;
      if ("})>".includes(c)) depth--;
      if (c === "," && depth === 0) { flush(); continue; }
      current += c;
    }
    current += "\n";
  }
  if (current.trim()) flush();
  return fields;
}

/// The type name a type expression refers to: `Signal<Player>?` → Signal.
function typeName(type) {
  const m = /^\s*(\w+)/.exec(type || "");
  return m ? m[1] : null;
}

function membersOf(type) {
  if (!type) return null;
  if (type.fields) return type.fields;
  const name = typeName(type.type || type.alias);
  return name && apiTypes.types[name] ? apiTypes.types[name].fields : null;
}

/// Guesses the type of a local from its declaration or its name.
function guessLocal(model, name, depth = 0) {
  if (depth > 3) return null;
  const text = model.getValue();
  const esc = name.replace(/[^\w]/g, "");
  const annotated = new RegExp(`\\b${esc}\\s*:\\s*(\\w+)`).exec(text);
  if (annotated && apiTypes.types[annotated[1]]) return { type: annotated[1] };
  const assigned = new RegExp(`local\\s+${esc}\\s*=\\s*([\\w.:]+)\\s*[.(]`).exec(text);
  if (assigned) {
    const ctor = assigned[1].replace(/[.:]new$/, "");
    if (apiTypes.types[ctor]) return { type: ctor };
    const resolved = resolveChain(assigned[1].split(/[.:]/), model, depth + 1);
    if (resolved && resolved.fn) {
      const ret = /->\s*\(?\s*(\w+)/.exec(resolved.type);
      if (ret && apiTypes.types[ret[1]]) return { type: ret[1] };
    }
  }
  const byName = Object.keys(apiTypes.types).find((t) => t.toLowerCase() === name.toLowerCase()
    || (t === "BlockEvent" && /^(event|ev|e|block)$/i.test(name))
    || (t === "Player" && /^(plr|p|target|killer)$/i.test(name)));
  return byName ? { type: byName } : null;
}

function resolveChain(parts, model, depth = 0) {
  let current = apiTypes.globals[parts[0]] || guessLocal(model, parts[0], depth);
  for (const part of parts.slice(1)) {
    const members = membersOf(current);
    current = members && members[part];
    if (!current) return null;
  }
  return current;
}

function chainBefore(model, position) {
  const line = model.getLineContent(position.lineNumber).slice(0, position.column - 1);
  const m = /([A-Za-z_]\w*(?:\s*[.:]\s*[A-Za-z_]\w*)*)\s*([.:])\s*(\w*)$/.exec(line);
  if (!m) return null;
  return { parts: m[1].split(/\s*[.:]\s*/), sep: m[2], partial: m[3] };
}

function complete(model, position) {
  const word = model.getWordUntilPosition(position);
  const range = { startLineNumber: position.lineNumber, endLineNumber: position.lineNumber, startColumn: word.startColumn, endColumn: word.endColumn };
  const K = monaco.languages.CompletionItemKind;
  const chain = chainBefore(model, position);
  if (chain) {
    const members = membersOf(resolveChain(chain.parts, model));
    if (!members) return { suggestions: [] };
    const suggestions = Object.entries(members)
      .filter(([, f]) => (chain.sep === ":" ? f.method : !f.method))
      .map(([name, f]) => ({
        label: name,
        kind: f.method || f.fn ? K.Method : /^Signal\b/.test(f.type) ? K.Event : K.Property,
        detail: f.type.replace(/\s+/g, " "),
        documentation: f.doc,
        insertText: name,
        range,
      }));
    return { suggestions };
  }
  const suggestions = Object.entries(apiTypes.globals).map(([name, g]) => ({
    label: name,
    kind: g.fn ? K.Function : K.Module,
    detail: g.fn ? g.type : "Minimodes API",
    documentation: g.doc,
    insertText: name,
    range,
  }));
  const snippets = [
    ["PlayerAdded", "game.Players.PlayerAdded:Connect(function(player)\n\t$0\nend)", "Run code for each player who joins"],
    ["function", "function ${1:name}(${2})\n\t$0\nend", "Function"],
    ["local function", "local function ${1:name}(${2})\n\t$0\nend", "Local function"],
    ["for in pairs", "for ${1:key}, ${2:value} in pairs(${3:t}) do\n\t$0\nend", "Loop over a table"],
    ["for in ipairs", "for ${1:i}, ${2:value} in ipairs(${3:list}) do\n\t$0\nend", "Loop over a list"],
    ["while wait", "while true do\n\t$0\n\ttask.wait(1)\nend", "Repeat every second"],
  ];
  for (const [label, body, doc] of snippets) {
    suggestions.push({
      label, kind: K.Snippet, documentation: doc, insertText: body, range,
      insertTextRules: monaco.languages.CompletionItemInsertTextRule.InsertAsSnippet,
    });
  }
  return { suggestions };
}

function hover(model, position) {
  const word = model.getWordAtPosition(position);
  if (!word) return null;
  const at = { lineNumber: position.lineNumber, column: word.startColumn };
  const chain = chainBefore(model, at);
  let info;
  let name = word.word;
  if (chain) {
    const members = membersOf(resolveChain(chain.parts, model));
    info = members && members[word.word];
    name = `${chain.parts.join(".")}${chain.sep}${word.word}`;
  } else {
    info = apiTypes.globals[word.word];
  }
  if (!info) return null;
  const type = (info.type || "").replace(/\s+/g, " ");
  return {
    range: { startLineNumber: position.lineNumber, endLineNumber: position.lineNumber, startColumn: word.startColumn, endColumn: word.endColumn },
    contents: [{ value: "```luau\n" + `${name}: ${type.length > 300 ? type.slice(0, 300) + "…" : type}` + "\n```" }, ...(info.doc ? [{ value: info.doc }] : [])],
  };
}

main().catch((e) => {
  console.error(e);
  status(`The editor failed to start: ${e.message}`, true, true);
});
