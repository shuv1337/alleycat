import { randomUUID } from "node:crypto";
import { EventEmitter } from "node:events";
import { readdirSync, statSync, unlinkSync, existsSync, readFileSync, rmSync, mkdirSync } from "node:fs";
import * as http2 from "node:http";
import { createServer } from "node:net";
import { userInfo, homedir, tmpdir } from "node:os";
import { extname, dirname, join, resolve, relative, isAbsolute, basename } from "node:path";
import { setDefaultCACertificates, getCACertificates } from "node:tls";
import { app, protocol, net, nativeImage, BrowserWindow, nativeTheme, ipcMain, dialog, shell, clipboard, Notification, Menu, utilityProcess } from "electron";
import contextMenu from "electron-context-menu";
import { execFile, execFileSync, spawnSync } from "node:child_process";
import { readdir, readFile, access } from "node:fs/promises";
import util from "node:util";
import Store from "electron-store";
import windowState from "electron-window-state";
import { fileURLToPath, pathToFileURL } from "node:url";
import log from "electron-log/main.js";
import { marked } from "marked";
import pkg from "electron-updater";
import { Effect, Deferred, Fiber } from "effect";
const execFilePromise = util.promisify(execFile);
const exists = (path) => access(path).then(() => true).catch(() => false);
function checkAppExists(appName) {
  if (process.platform === "win32") return true;
  if (process.platform === "linux") return true;
  return checkMacosApp(appName);
}
function resolveAppPath(appName) {
  if (process.platform !== "win32") return appName;
  return resolveWindowsAppPath(appName);
}
function wslPath(path, mode) {
  if (process.platform !== "win32") return path;
  const flag = mode === "windows" ? "-w" : "-u";
  try {
    if (path.startsWith("~")) {
      const suffix = path.slice(1);
      const cmd = `wslpath ${flag} "$HOME${suffix.replace(/"/g, '\\"')}"`;
      const output2 = execFileSync("wsl", ["-e", "sh", "-lc", cmd]);
      return output2.toString().trim();
    }
    const output = execFileSync("wsl", ["-e", "wslpath", flag, path]);
    return output.toString().trim();
  } catch (error) {
    throw new Error(`Failed to run wslpath: ${String(error)}`, { cause: error });
  }
}
async function checkMacosApp(appName) {
  const locations = [`/Applications/${appName}.app`, `/System/Applications/${appName}.app`];
  const home = process.env.HOME;
  if (home) locations.push(`${home}/Applications/${appName}.app`);
  for (const location of locations) {
    if (await exists(location)) return true;
  }
  return execFilePromise("which", [appName]).then(() => true).catch(() => false);
}
async function resolveWindowsAppPath(appName) {
  let output;
  try {
    output = await execFilePromise("where", [appName]).then((r) => r.stdout.toString());
  } catch {
    return null;
  }
  const paths = output.split(/\r?\n/).map((line) => line.trim()).filter((line) => line.length > 0);
  const hasExt = (path, ext) => extname(path).toLowerCase() === `.${ext}`;
  const exe = paths.find((path) => hasExt(path, "exe"));
  if (exe) return exe;
  const resolveCmd = async (path) => {
    const content = await readFile(path, "utf8");
    for (const token of content.split('"').map((value) => value.trim())) {
      const lower = token.toLowerCase();
      if (!lower.includes(".exe")) continue;
      const index = lower.indexOf("%~dp0");
      if (index >= 0) {
        const base = dirname(path);
        const suffix = token.slice(index + 5);
        const resolved = suffix.replace(/\//g, "\\").split("\\").filter((part) => part && part !== ".").reduce((current, part) => {
          if (part === "..") return dirname(current);
          return join(current, part);
        }, base);
        if (await exists(resolved)) return resolved;
      }
      if (await exists(token)) return token;
    }
    return null;
  };
  for (const path of paths) {
    if (hasExt(path, "cmd") || hasExt(path, "bat")) {
      const resolved = await resolveCmd(path);
      if (resolved) return resolved;
    }
    if (!extname(path)) {
      const cmd = `${path}.cmd`;
      if (await exists(cmd)) {
        const resolved = await resolveCmd(cmd);
        if (resolved) return resolved;
      }
      const bat = `${path}.bat`;
      if (await exists(bat)) {
        const resolved = await resolveCmd(bat);
        if (resolved) return resolved;
      }
    }
  }
  const key = appName.split("").filter((value) => /[a-z0-9]/i.test(value)).map((value) => value.toLowerCase()).join("");
  if (key) {
    for (const path of paths) {
      const dirs = [dirname(path), dirname(dirname(path)), dirname(dirname(dirname(path)))];
      for (const dir of dirs) {
        try {
          for (const entry of await readdir(dir)) {
            const candidate = join(dir, entry);
            if (!hasExt(candidate, "exe")) continue;
            const stem = entry.replace(/\.exe$/i, "");
            const name = stem.split("").filter((value) => /[a-z0-9]/i.test(value)).map((value) => value.toLowerCase()).join("");
            if (name.includes(key) || key.includes(name)) return candidate;
          }
        } catch {
          continue;
        }
      }
    }
  }
  return paths[0] ?? null;
}
const raw = "prod";
const CHANNEL = raw;
const SETTINGS_STORE = "opencode.settings";
const DEFAULT_SERVER_URL_KEY = "defaultServerUrl";
const WSL_ENABLED_KEY = "wslEnabled";
const UPDATER_ENABLED = app.isPackaged && CHANNEL !== "dev";
const cache = /* @__PURE__ */ new Map();
function getStore(name = SETTINGS_STORE) {
  const cached = cache.get(name);
  if (cached) return cached;
  const next = new Store({
    name,
    cwd: app.getPath("userData"),
    fileExtension: "",
    accessPropertiesByDotNotation: false
  });
  cache.set(name, next);
  return next;
}
const root = dirname(fileURLToPath(import.meta.url));
const rendererRoot = join(root, "../renderer");
const rendererProtocol = "oc";
const rendererHost = "renderer";
const clipboardWritePermission = "clipboard-sanitized-write";
protocol.registerSchemesAsPrivileged([
  {
    scheme: rendererProtocol,
    privileges: {
      secure: true,
      standard: true,
      supportFetchAPI: true
    }
  }
]);
let backgroundColor;
const titlebarThemes = /* @__PURE__ */ new WeakMap();
const titlebarHeight = 40;
function setBackgroundColor(color) {
  backgroundColor = color;
}
function iconsDir() {
  return app.isPackaged ? join(process.resourcesPath, "icons") : join(root, "../../resources/icons");
}
function iconPath() {
  const ext = process.platform === "win32" ? "ico" : "png";
  return join(iconsDir(), `icon.${ext}`);
}
function tone() {
  return nativeTheme.shouldUseDarkColors ? "dark" : "light";
}
function overlay(theme = {}, zoom = 1) {
  const mode = theme.mode ?? tone();
  return {
    color: "#00000000",
    symbolColor: mode === "dark" ? "white" : "black",
    height: Math.max(titlebarHeight, Math.round(titlebarHeight * zoom))
  };
}
function setTitlebar(win, theme = {}) {
  titlebarThemes.set(win, theme);
  updateTitlebar(win);
}
function updateTitlebar(win) {
  if (process.platform !== "win32") return;
  win.setTitleBarOverlay(overlay(titlebarThemes.get(win), win.webContents.getZoomFactor()));
}
function setDockIcon() {
  if (process.platform !== "darwin") return;
  const icon = nativeImage.createFromPath(join(iconsDir(), "dock.png"));
  if (!icon.isEmpty()) app.dock?.setIcon(icon);
}
function createMainWindow() {
  const state = windowState({
    defaultWidth: 1280,
    defaultHeight: 800
  });
  const mode = tone();
  const win = new BrowserWindow({
    x: state.x,
    y: state.y,
    width: state.width,
    height: state.height,
    show: false,
    autoHideMenuBar: true,
    title: "OpenCode",
    icon: iconPath(),
    backgroundColor,
    ...process.platform === "darwin" ? {
      titleBarStyle: "hidden",
      trafficLightPosition: { x: 12, y: 14 }
    } : {},
    ...process.platform === "win32" ? {
      frame: false,
      titleBarStyle: "hidden",
      titleBarOverlay: overlay({ mode })
    } : {},
    webPreferences: {
      preload: join(root, "../preload/index.js"),
      contextIsolation: true,
      nodeIntegration: false,
      sandbox: true
    }
  });
  allowClipboardWrite(win);
  win.webContents.session.webRequest.onBeforeSendHeaders((details, callback) => {
    const { requestHeaders } = details;
    upsertKeyValue(requestHeaders, "Access-Control-Allow-Origin", ["*"]);
    callback({ requestHeaders });
  });
  win.webContents.session.webRequest.onHeadersReceived((details, callback) => {
    const { responseHeaders = {} } = details;
    upsertKeyValue(responseHeaders, "Access-Control-Allow-Origin", ["*"]);
    upsertKeyValue(responseHeaders, "Access-Control-Allow-Headers", ["*"]);
    callback({ responseHeaders });
  });
  state.manage(win);
  loadWindow(win, "index.html");
  wireZoom(win);
  win.once("ready-to-show", () => {
    win.show();
  });
  return win;
}
function createLoadingWindow() {
  const mode = tone();
  const win = new BrowserWindow({
    width: 640,
    height: 480,
    resizable: false,
    center: true,
    show: true,
    autoHideMenuBar: true,
    icon: iconPath(),
    backgroundColor,
    ...process.platform === "darwin" ? { titleBarStyle: "hidden" } : {},
    ...process.platform === "win32" ? {
      frame: false,
      titleBarStyle: "hidden",
      titleBarOverlay: overlay({ mode })
    } : {},
    webPreferences: {
      preload: join(root, "../preload/index.js"),
      contextIsolation: true,
      nodeIntegration: false,
      sandbox: true
    }
  });
  allowClipboardWrite(win);
  loadWindow(win, "loading.html");
  return win;
}
function registerRendererProtocol() {
  if (protocol.isProtocolHandled(rendererProtocol)) return;
  protocol.handle(rendererProtocol, (request) => {
    const url = new URL(request.url);
    if (url.host !== rendererHost) {
      return new Response("Not found", { status: 404 });
    }
    const file = resolve(rendererRoot, `.${decodeURIComponent(url.pathname)}`);
    const rel = relative(rendererRoot, file);
    if (rel.startsWith("..") || isAbsolute(rel)) {
      return new Response("Not found", { status: 404 });
    }
    return net.fetch(pathToFileURL(file).toString());
  });
}
function loadWindow(win, html) {
  const devUrl = process.env.ELECTRON_RENDERER_URL;
  if (devUrl) {
    const url = new URL(html, devUrl);
    void win.loadURL(url.toString());
    return;
  }
  void win.loadURL(`${rendererProtocol}://${rendererHost}/${html}`);
}
function allowClipboardWrite(win) {
  win.webContents.session.setPermissionRequestHandler((webContents, permission, callback, details) => {
    callback(
      permission === clipboardWritePermission && isTrustedRendererUrl(details.requestingUrl) && webContents.id === win.webContents.id
    );
  });
  win.webContents.session.setPermissionCheckHandler((webContents, permission, requestingOrigin, details) => {
    if (permission !== clipboardWritePermission) return false;
    if (webContents && webContents.id !== win.webContents.id) return false;
    return isTrustedRendererUrl(details.requestingUrl) || isTrustedRendererUrl(requestingOrigin);
  });
}
function isTrustedRendererUrl(value) {
  if (!value || !URL.canParse(value)) return false;
  const url = new URL(value);
  if (url.protocol === `${rendererProtocol}:` && url.host === rendererHost) return true;
  const devUrl = process.env.ELECTRON_RENDERER_URL;
  if (!devUrl || !URL.canParse(devUrl)) return false;
  return url.origin === new URL(devUrl).origin;
}
function wireZoom(win) {
  win.webContents.setZoomFactor(1);
  win.webContents.on("zoom-changed", () => {
    win.webContents.setZoomFactor(1);
    updateTitlebar(win);
  });
}
function upsertKeyValue(obj, keyToChange, value) {
  const keyToChangeLower = keyToChange.toLowerCase();
  for (const key of Object.keys(obj)) {
    if (key.toLowerCase() === keyToChangeLower) {
      obj[key] = value;
      return;
    }
  }
  obj[keyToChange] = value;
}
const pickerFilters = (ext) => {
  if (!ext || ext.length === 0) return void 0;
  return [{ name: "Files", extensions: ext }];
};
function registerIpcHandlers(deps) {
  ipcMain.handle("kill-sidecar", () => deps.killSidecar());
  ipcMain.handle("await-initialization", (event) => {
    const send = (step) => event.sender.send("init-step", step);
    return deps.awaitInitialization(send);
  });
  ipcMain.handle("get-window-config", () => deps.getWindowConfig());
  ipcMain.handle("consume-initial-deep-links", () => deps.consumeInitialDeepLinks());
  ipcMain.handle("get-default-server-url", () => deps.getDefaultServerUrl());
  ipcMain.handle(
    "set-default-server-url",
    (_event, url) => deps.setDefaultServerUrl(url)
  );
  ipcMain.handle("get-wsl-config", () => deps.getWslConfig());
  ipcMain.handle("set-wsl-config", (_event, config) => deps.setWslConfig(config));
  ipcMain.handle("get-display-backend", () => deps.getDisplayBackend());
  ipcMain.handle(
    "set-display-backend",
    (_event, backend) => deps.setDisplayBackend(backend)
  );
  ipcMain.handle("parse-markdown", (_event, markdown) => deps.parseMarkdown(markdown));
  ipcMain.handle("check-app-exists", (_event, appName) => deps.checkAppExists(appName));
  ipcMain.handle(
    "wsl-path",
    (_event, path, mode) => deps.wslPath(path, mode)
  );
  ipcMain.handle("resolve-app-path", (_event, appName) => deps.resolveAppPath(appName));
  ipcMain.on("loading-window-complete", () => deps.loadingWindowComplete());
  ipcMain.handle("run-updater", (_event, alertOnFail) => deps.runUpdater(alertOnFail));
  ipcMain.handle("check-update", () => deps.checkUpdate());
  ipcMain.handle("install-update", () => deps.installUpdate());
  ipcMain.handle("set-background-color", (_event, color) => deps.setBackgroundColor(color));
  ipcMain.handle("store-get", (_event, name, key) => {
    try {
      const store = getStore(name);
      const value = store.get(key);
      if (value === void 0 || value === null) return null;
      return typeof value === "string" ? value : JSON.stringify(value);
    } catch {
      return null;
    }
  });
  ipcMain.handle("store-set", (_event, name, key, value) => {
    getStore(name).set(key, value);
  });
  ipcMain.handle("store-delete", (_event, name, key) => {
    getStore(name).delete(key);
  });
  ipcMain.handle("store-clear", (_event, name) => {
    getStore(name).clear();
  });
  ipcMain.handle("store-keys", (_event, name) => {
    const store = getStore(name);
    return Object.keys(store.store);
  });
  ipcMain.handle("store-length", (_event, name) => {
    const store = getStore(name);
    return Object.keys(store.store).length;
  });
  ipcMain.handle(
    "open-directory-picker",
    async (_event, opts) => {
      const result = await dialog.showOpenDialog({
        properties: ["openDirectory", ...opts?.multiple ? ["multiSelections"] : [], "createDirectory"],
        title: opts?.title ?? "Choose a folder",
        defaultPath: opts?.defaultPath
      });
      if (result.canceled) return null;
      return opts?.multiple ? result.filePaths : result.filePaths[0];
    }
  );
  ipcMain.handle(
    "open-file-picker",
    async (_event, opts) => {
      const result = await dialog.showOpenDialog({
        properties: ["openFile", ...opts?.multiple ? ["multiSelections"] : []],
        title: opts?.title ?? "Choose a file",
        defaultPath: opts?.defaultPath,
        filters: pickerFilters(opts?.extensions)
      });
      if (result.canceled) return null;
      return opts?.multiple ? result.filePaths : result.filePaths[0];
    }
  );
  ipcMain.handle(
    "save-file-picker",
    async (_event, opts) => {
      const result = await dialog.showSaveDialog({
        title: opts?.title ?? "Save file",
        defaultPath: opts?.defaultPath
      });
      if (result.canceled) return null;
      return result.filePath ?? null;
    }
  );
  ipcMain.on("open-link", (_event, url) => {
    void shell.openExternal(url);
  });
  ipcMain.handle("open-path", async (_event, path, app2) => {
    if (!app2) return shell.openPath(path);
    await new Promise((resolve2, reject) => {
      const [cmd, args] = process.platform === "darwin" ? ["open", ["-a", app2, path]] : [app2, [path]];
      execFile(cmd, args, (err) => err ? reject(err) : resolve2());
    });
  });
  ipcMain.handle("read-clipboard-image", () => {
    const image = clipboard.readImage();
    if (image.isEmpty()) return null;
    const buffer = image.toPNG().buffer;
    const size = image.getSize();
    return { buffer, width: size.width, height: size.height };
  });
  ipcMain.on("show-notification", (_event, title, body) => {
    new Notification({ title, body }).show();
  });
  ipcMain.handle("get-window-count", () => BrowserWindow.getAllWindows().length);
  ipcMain.handle("get-window-focused", (event) => {
    const win = BrowserWindow.fromWebContents(event.sender);
    return win?.isFocused() ?? false;
  });
  ipcMain.handle("set-window-focus", (event) => {
    const win = BrowserWindow.fromWebContents(event.sender);
    win?.focus();
  });
  ipcMain.handle("show-window", (event) => {
    const win = BrowserWindow.fromWebContents(event.sender);
    win?.show();
  });
  ipcMain.on("relaunch", () => {
    app.relaunch();
    app.exit(0);
  });
  ipcMain.handle("get-zoom-factor", (event) => event.sender.getZoomFactor());
  ipcMain.handle("set-zoom-factor", (event, factor) => {
    event.sender.setZoomFactor(factor);
    const win = BrowserWindow.fromWebContents(event.sender);
    if (!win) return;
    updateTitlebar(win);
  });
  ipcMain.handle("set-titlebar", (event, theme) => {
    const win = BrowserWindow.fromWebContents(event.sender);
    if (!win) return;
    setTitlebar(win, theme);
  });
}
function sendSqliteMigrationProgress(win, progress) {
  win.webContents.send("sqlite-migration-progress", progress);
}
function sendMenuCommand(win, id) {
  win.webContents.send("menu-command", id);
}
function sendDeepLinks(win, urls) {
  win.webContents.send("deep-link", urls);
}
const MAX_LOG_AGE_DAYS = 7;
let logger$2;
const getLogger = () => logger$2;
function initLogging() {
  log.transports.file.maxSize = 5 * 1024 * 1024;
  initConsoleTransport();
  cleanup();
  return logger$2 = log;
}
function cleanup() {
  const path = log.transports.file.getFile().path;
  const dir = dirname(path);
  const cutoff = Date.now() - MAX_LOG_AGE_DAYS * 24 * 60 * 60 * 1e3;
  for (const entry of readdirSync(dir)) {
    const file = join(dir, entry);
    try {
      const info = statSync(file);
      if (!info.isFile()) continue;
      if (info.mtimeMs < cutoff) unlinkSync(file);
    } catch {
      continue;
    }
  }
}
function initConsoleTransport() {
  const write = log.transports.console.writeFn.bind(log.transports.console);
  log.transports.console.writeFn = (options) => {
    try {
      write(options);
    } catch (err) {
      if (!isBrokenPipe(err)) throw err;
      log.transports.console.level = false;
    }
  };
}
function isBrokenPipe(err) {
  return typeof err === "object" && err !== null && "code" in err && err.code === "EPIPE";
}
const renderer = new marked.Renderer();
renderer.link = ({ href, title, text }) => {
  const titleAttr = title ? ` title="${title}"` : "";
  return `<a href="${href}"${titleAttr} class="external-link" target="_blank" rel="noopener noreferrer">${text}</a>`;
};
function parseMarkdown(input) {
  return marked(input, {
    renderer,
    breaks: false,
    gfm: true
  });
}
function createMenu(deps) {
  if (process.platform !== "darwin") return;
  const template = [
    {
      label: "OpenCode",
      submenu: [
        { role: "about" },
        {
          label: "Check for Updates...",
          enabled: UPDATER_ENABLED,
          click: () => deps.checkForUpdates()
        },
        {
          label: "Settings",
          accelerator: "Cmd+,",
          click: () => deps.trigger("settings.open")
        },
        {
          label: "Reload Webview",
          click: () => deps.reload()
        },
        {
          label: "Restart",
          click: () => deps.relaunch()
        },
        { type: "separator" },
        { role: "hide" },
        { role: "hideOthers" },
        { role: "unhide" },
        { type: "separator" },
        { role: "quit" }
      ]
    },
    {
      label: "File",
      submenu: [
        { label: "New Session", accelerator: "Shift+Cmd+S", click: () => deps.trigger("session.new") },
        { label: "Open Project...", accelerator: "Cmd+O", click: () => deps.trigger("project.open") },
        {
          label: "New Window",
          accelerator: "Cmd+Shift+N",
          click: () => createMainWindow()
        },
        { type: "separator" },
        { role: "close" }
      ]
    },
    {
      label: "Edit",
      submenu: [
        { role: "undo" },
        { role: "redo" },
        { type: "separator" },
        { role: "cut" },
        { role: "copy" },
        { role: "paste" },
        { role: "selectAll" }
      ]
    },
    {
      label: "View",
      submenu: [
        { label: "Toggle Sidebar", accelerator: "Cmd+B", click: () => deps.trigger("sidebar.toggle") },
        { label: "Toggle Terminal", accelerator: "Ctrl+`", click: () => deps.trigger("terminal.toggle") },
        { label: "Toggle File Tree", click: () => deps.trigger("fileTree.toggle") },
        { type: "separator" },
        { role: "reload" },
        { role: "toggleDevTools" },
        { type: "separator" },
        { role: "resetZoom" },
        { role: "zoomIn" },
        { role: "zoomOut" },
        { type: "separator" },
        { role: "togglefullscreen" }
      ]
    },
    {
      label: "Go",
      submenu: [
        { label: "Back", accelerator: "Cmd+[", click: () => deps.trigger("common.goBack") },
        { label: "Forward", accelerator: "Cmd+]", click: () => deps.trigger("common.goForward") },
        { type: "separator" },
        {
          label: "Previous Session",
          accelerator: "Option+Up",
          click: () => deps.trigger("session.previous")
        },
        {
          label: "Next Session",
          accelerator: "Option+Down",
          click: () => deps.trigger("session.next")
        },
        { type: "separator" },
        {
          label: "Previous Project",
          accelerator: "Cmd+Option+Up",
          click: () => deps.trigger("project.previous")
        },
        {
          label: "Next Project",
          accelerator: "Cmd+Option+Down",
          click: () => deps.trigger("project.next")
        }
      ]
    },
    { role: "windowMenu" },
    {
      label: "Help",
      submenu: [
        { label: "OpenCode Documentation", click: () => shell.openExternal("https://opencode.ai/docs") },
        { label: "Support Forum", click: () => shell.openExternal("https://discord.com/invite/opencode") },
        { type: "separator" },
        { type: "separator" },
        {
          label: "Share Feedback",
          click: () => shell.openExternal("https://github.com/anomalyco/opencode/issues/new?template=feature_request.yml")
        },
        {
          label: "Report a Bug",
          click: () => shell.openExternal("https://github.com/anomalyco/opencode/issues/new?template=bug_report.yml")
        }
      ]
    }
  ];
  Menu.setApplicationMenu(Menu.buildFromTemplate(template));
}
const TIMEOUT = 5e3;
function resolveUserShell(envShell, loginShell) {
  const resolvedLoginShell = loginShell && loginShell !== "unknown" ? loginShell : void 0;
  return envShell || resolvedLoginShell || "/bin/sh";
}
function getUserShell() {
  try {
    return resolveUserShell(process.env.SHELL, userInfo().shell);
  } catch {
    return resolveUserShell(process.env.SHELL, void 0);
  }
}
function parseShellEnv(out) {
  const env = {};
  for (const line of out.toString("utf8").split("\0")) {
    if (!line) continue;
    const ix = line.indexOf("=");
    if (ix <= 0) continue;
    env[line.slice(0, ix)] = line.slice(ix + 1);
  }
  return env;
}
function probe(shell2, mode) {
  const out = spawnSync(shell2, [mode, "-c", "env -0"], {
    stdio: ["ignore", "pipe", "ignore"],
    timeout: TIMEOUT,
    windowsHide: true
  });
  const err = out.error;
  if (err) {
    if (err.code === "ETIMEDOUT") return { type: "Timeout" };
    console.log(`[server] Shell env probe failed for ${shell2} ${mode}: ${err.message}`);
    return { type: "Unavailable" };
  }
  if (out.status !== 0) {
    console.log(`[server] Shell env probe exited with non-zero status for ${shell2} ${mode}`);
    return { type: "Unavailable" };
  }
  const env = parseShellEnv(out.stdout);
  if (Object.keys(env).length === 0) {
    console.log(`[server] Shell env probe returned empty env for ${shell2} ${mode}`);
    return { type: "Unavailable" };
  }
  return { type: "Loaded", value: env };
}
function isNushell(shell2) {
  const name = basename(shell2).toLowerCase();
  const raw2 = shell2.toLowerCase();
  return name === "nu" || name === "nu.exe" || raw2.endsWith("\\nu.exe");
}
function loadShellEnv(shell2) {
  const logger2 = getLogger();
  if (isNushell(shell2)) {
    logger2.log(`[server] Skipping shell env probe for nushell: ${shell2}`);
    return null;
  }
  const interactive = probe(shell2, "-il");
  if (interactive.type === "Loaded") {
    logger2.log(`[server] Loaded shell environment with -il (${Object.keys(interactive.value).length} vars)`);
    return interactive.value;
  }
  if (interactive.type === "Timeout") {
    logger2.log(`[server] Interactive shell env probe timed out: ${shell2}`);
    return null;
  }
  const login = probe(shell2, "-l");
  if (login.type === "Loaded") {
    logger2.log(`[server] Loaded shell environment with -l (${Object.keys(login.value).length} vars)`);
    return login.value;
  }
  logger2.log(`[server] Falling back to app environment: ${shell2}`);
  return null;
}
const SIDECAR_SERVICE_NAME = "opencode server";
const SIDECAR_START_STALL_TIMEOUT = 6e4;
const SIDECAR_STOP_TIMEOUT = 6e3;
function getDefaultServerUrl() {
  const value = getStore().get(DEFAULT_SERVER_URL_KEY);
  return typeof value === "string" ? value : null;
}
function setDefaultServerUrl(url) {
  if (url) {
    getStore().set(DEFAULT_SERVER_URL_KEY, url);
    return;
  }
  getStore().delete(DEFAULT_SERVER_URL_KEY);
}
function getWslConfig() {
  const value = getStore().get(WSL_ENABLED_KEY);
  return { enabled: typeof value === "boolean" ? value : false };
}
function setWslConfig(config) {
  getStore().set(WSL_ENABLED_KEY, config.enabled);
}
function preferAppEnv(userDataPath) {
  const shell2 = process.platform === "win32" ? null : getUserShell();
  Object.assign(process.env, {
    ...shell2 ? loadShellEnv(shell2) : null,
    OPENCODE_EXPERIMENTAL_ICON_DISCOVERY: "true",
    OPENCODE_EXPERIMENTAL_FILEWATCHER: "true",
    OPENCODE_CLIENT: "desktop",
    XDG_STATE_HOME: process.env.XDG_STATE_HOME ?? userDataPath
  });
}
async function spawnLocalServer(hostname, port, password, options) {
  const sidecar = join(dirname(fileURLToPath(import.meta.url)), "sidecar.js");
  const child = utilityProcess.fork(sidecar, [], {
    cwd: process.cwd(),
    env: createSidecarEnv(),
    serviceName: SIDECAR_SERVICE_NAME,
    stdio: "pipe"
  });
  let exited = false;
  const exit = defer();
  const onProcessGone = (_event, details) => {
    if (details.type !== "Utility" || details.name !== SIDECAR_SERVICE_NAME) return;
    options.onStderr?.(`utility process gone reason=${details.reason} exitCode=${details.exitCode}`);
  };
  app.on("child-process-gone", onProcessGone);
  child.once("exit", (code) => {
    exited = true;
    app.off("child-process-gone", onProcessGone);
    options.onExit?.(code);
    exit.resolve(code);
  });
  child.on("error", (error) => options.onStderr?.(`utility process error: ${serializeError(error).message}`));
  child.stdout?.on("data", (chunk) => options.onStdout?.(chunk.toString("utf8").trimEnd()));
  child.stderr?.on("data", (chunk) => options.onStderr?.(chunk.toString("utf8").trimEnd()));
  await new Promise((resolve2, reject) => {
    let done = false;
    let timeout;
    const fail = (error) => {
      if (done) return;
      done = true;
      cleanup2();
      reject(error);
    };
    const refreshTimeout = () => {
      clearTimeout(timeout);
      timeout = setTimeout(() => {
        fail(new Error(`Sidecar did not become ready within ${SIDECAR_START_STALL_TIMEOUT}ms: ${sidecar}`));
      }, SIDECAR_START_STALL_TIMEOUT);
    };
    const onMessage = (message) => {
      if (message.type === "sqlite") {
        refreshTimeout();
        options.onSqliteProgress?.(message.progress);
        return;
      }
      if (message.type === "ready") {
        if (done) return;
        done = true;
        cleanup2();
        resolve2();
        return;
      }
      if (message.type === "error") {
        fail(Object.assign(new Error(message.error.message), { stack: message.error.stack }));
      }
    };
    const onExit = (code) => {
      fail(new Error(`Sidecar exited before ready with code ${code}`));
    };
    const cleanup2 = () => {
      clearTimeout(timeout);
      child.off("message", onMessage);
      child.off("exit", onExit);
    };
    child.on("message", onMessage);
    child.on("exit", onExit);
    refreshTimeout();
    child.postMessage({
      type: "start",
      hostname,
      port,
      password,
      userDataPath: options.userDataPath,
      needsMigration: options.needsMigration
    });
  }).catch((error) => {
    if (!exited) child.kill();
    throw error;
  });
  const wait = (async () => {
    const url = `http://${hostname}:${port}`;
    let healthy = false;
    const gone = exit.promise.then((code) => {
      if (healthy) return;
      throw new Error(`Sidecar exited before health check passed with code ${code}`);
    });
    const ready = async () => {
      while (true) {
        await new Promise((resolve2) => setTimeout(resolve2, 100));
        if (await checkHealth(url, password)) {
          healthy = true;
          return;
        }
      }
    };
    await Promise.race([ready(), gone]);
  })();
  let stopping;
  return {
    listener: {
      stop: () => {
        if (stopping) return stopping;
        if (exited) return Promise.resolve();
        child.postMessage({ type: "stop" });
        stopping = Promise.race([
          exit.promise.then(() => void 0),
          delay(SIDECAR_STOP_TIMEOUT).then(() => {
            if (!exited) child.kill();
          })
        ]);
        return stopping;
      }
    },
    health: { wait }
  };
}
async function checkHealth(url, password) {
  let healthUrl;
  try {
    healthUrl = new URL("/global/health", url);
  } catch {
    return false;
  }
  const headers = new Headers();
  if (password) {
    const auth = Buffer.from(`opencode:${password}`).toString("base64");
    headers.set("authorization", `Basic ${auth}`);
  }
  try {
    const res = await fetch(healthUrl, {
      method: "GET",
      headers,
      signal: AbortSignal.timeout(3e3)
    });
    return res.ok;
  } catch {
    return false;
  }
}
function createSidecarEnv() {
  const env = Object.fromEntries(
    Object.entries(process.env).flatMap(([key, value]) => value === void 0 ? [] : [[key, String(value)]])
  );
  delete env.DEBUG;
  if (process.platform === "linux") delete env.LD_PRELOAD;
  return env;
}
function delay(ms) {
  return new Promise((resolve2) => setTimeout(resolve2, ms));
}
function serializeError(error) {
  if (error instanceof Error) return { message: error.message, stack: error.stack };
  return { message: String(error) };
}
function defer() {
  let resolve2;
  let reject;
  const promise = new Promise((res, rej) => {
    resolve2 = res;
    reject = rej;
  });
  return { promise, resolve: resolve2, reject };
}
const TAURI_MIGRATED_KEY = "tauriMigrated";
function tauriDir(id) {
  switch (process.platform) {
    case "darwin":
      return join(homedir(), "Library", "Application Support", id);
    case "win32":
      return join(process.env.APPDATA ?? join(homedir(), "AppData", "Roaming"), id);
    default:
      return join(process.env.XDG_DATA_HOME ?? join(homedir(), ".local", "share"), id);
  }
}
const TAURI_APP_IDS = {
  dev: "ai.opencode.desktop.dev",
  beta: "ai.opencode.desktop.beta",
  prod: "ai.opencode.desktop"
};
function tauriAppId() {
  return app.isPackaged ? TAURI_APP_IDS[CHANNEL] : "ai.opencode.desktop.dev";
}
function migrateFile(datPath, filename) {
  let data;
  try {
    data = JSON.parse(readFileSync(datPath, "utf-8"));
  } catch (err) {
    log.warn("tauri migration: failed to parse", filename, err);
    return;
  }
  const storeName = filename === "opencode.settings.dat" ? "opencode.settings" : filename;
  const target = getStore(storeName);
  const migrated = [];
  const skipped = [];
  for (const [key, value] of Object.entries(data)) {
    if (target.has(key)) {
      skipped.push(key);
      continue;
    }
    target.set(key, value);
    migrated.push(key);
  }
  log.log("tauri migration: migrated", filename, "→", storeName, { migrated, skipped });
}
function migrate() {
  if (getStore().get(TAURI_MIGRATED_KEY)) {
    log.log("tauri migration: already done, skipping");
    return;
  }
  const dir = tauriDir(tauriAppId());
  log.log("tauri migration: starting", { dir });
  if (!existsSync(dir)) {
    log.log("tauri migration: no tauri data directory found, nothing to migrate");
    getStore().set(TAURI_MIGRATED_KEY, true);
    return;
  }
  for (const filename of readdirSync(dir)) {
    if (!filename.endsWith(".dat")) continue;
    migrateFile(join(dir, filename), filename);
  }
  log.log("tauri migration: complete");
  getStore().set(TAURI_MIGRATED_KEY, true);
}
const logger$1 = initLogging();
const { autoUpdater } = pkg;
let downloadedUpdateVersion;
function setupAutoUpdater() {
  if (!UPDATER_ENABLED) return;
  autoUpdater.logger = logger$1;
  autoUpdater.channel = "latest";
  autoUpdater.allowPrerelease = false;
  autoUpdater.allowDowngrade = true;
  autoUpdater.autoDownload = false;
  autoUpdater.autoInstallOnAppQuit = false;
  logger$1.log("auto updater configured", {
    channel: autoUpdater.channel,
    allowPrerelease: autoUpdater.allowPrerelease,
    allowDowngrade: autoUpdater.allowDowngrade,
    currentVersion: app.getVersion()
  });
}
async function checkUpdate() {
  if (!UPDATER_ENABLED) return { updateAvailable: false };
  if (downloadedUpdateVersion) {
    logger$1.log("returning cached downloaded update", {
      version: downloadedUpdateVersion
    });
    return { updateAvailable: true, version: downloadedUpdateVersion };
  }
  logger$1.log("checking for updates", {
    currentVersion: app.getVersion(),
    channel: autoUpdater.channel,
    allowPrerelease: autoUpdater.allowPrerelease,
    allowDowngrade: autoUpdater.allowDowngrade
  });
  try {
    const result = await autoUpdater.checkForUpdates();
    const updateInfo = result?.updateInfo;
    logger$1.log("update metadata fetched", {
      releaseVersion: updateInfo?.version ?? null,
      releaseDate: updateInfo?.releaseDate ?? null,
      releaseName: updateInfo?.releaseName ?? null,
      files: updateInfo?.files?.map((file) => file.url) ?? []
    });
    const version = result?.updateInfo?.version;
    if (result?.isUpdateAvailable === false || !version) {
      logger$1.log("no update available", {
        reason: "provider returned no newer version"
      });
      return { updateAvailable: false };
    }
    logger$1.log("update available", { version });
    await autoUpdater.downloadUpdate();
    logger$1.log("update download completed", { version });
    downloadedUpdateVersion = version;
    return { updateAvailable: true, version };
  } catch (error) {
    logger$1.error("update check failed", error);
    return { updateAvailable: false, failed: true };
  }
}
async function installUpdate(killSidecar2) {
  if (!downloadedUpdateVersion) {
    logger$1.log("install update skipped", {
      reason: "no downloaded update ready"
    });
    return;
  }
  logger$1.log("installing downloaded update", {
    version: downloadedUpdateVersion
  });
  await killSidecar2();
  autoUpdater.quitAndInstall();
}
async function checkForUpdates(alertOnFail, killSidecar2) {
  if (!UPDATER_ENABLED) return;
  logger$1.log("checkForUpdates invoked", { alertOnFail });
  const result = await checkUpdate();
  if (!result.updateAvailable) {
    if (result.failed) {
      logger$1.log("no update decision", { reason: "update check failed" });
      if (!alertOnFail) return;
      await dialog.showMessageBox({
        type: "error",
        message: "Update check failed.",
        title: "Update Error"
      });
      return;
    }
    logger$1.log("no update decision", { reason: "already up to date" });
    if (!alertOnFail) return;
    await dialog.showMessageBox({
      type: "info",
      message: "You're up to date.",
      title: "No Updates"
    });
    return;
  }
  const response = await dialog.showMessageBox({
    type: "info",
    message: `Update ${result.version ?? ""} downloaded. Restart now?`,
    title: "Update Ready",
    buttons: ["Restart", "Later"],
    defaultId: 0,
    cancelId: 1
  });
  logger$1.log("update prompt response", {
    version: result.version ?? null,
    restartNow: response.response === 0
  });
  if (response.response === 0) {
    await installUpdate(killSidecar2);
  }
}
const APP_NAMES = {
  dev: "OpenCode Dev",
  beta: "OpenCode Beta",
  prod: "OpenCode"
};
const APP_IDS = {
  dev: "ai.opencode.desktop.dev",
  beta: "ai.opencode.desktop.beta",
  prod: "ai.opencode.desktop"
};
const TEST_ONBOARDING = process.env.OPENCODE_TEST_ONBOARDING === "1";
let logger;
let mainWindow = null;
let server = null;
const initEmitter = new EventEmitter();
let initStep = { phase: "server_waiting" };
const pendingDeepLinks = [];
function useEnvProxy() {
  try {
    ;
    http2.setGlobalProxyFromEnv();
  } catch (error) {
    logger.warn("failed to load proxy environment", error);
  }
}
function emitDeepLinks(urls) {
  if (urls.length === 0) return;
  pendingDeepLinks.push(...urls);
  if (mainWindow) sendDeepLinks(mainWindow, urls);
}
function setInitStep(step) {
  initStep = step;
  logger.log("init step", { step });
  initEmitter.emit("step", step);
}
async function killSidecar() {
  if (!server) return;
  const current = server;
  server = null;
  await current.stop();
}
function ensureLoopbackNoProxy() {
  const loopback = ["127.0.0.1", "localhost", "::1"];
  const upsert = (key) => {
    const items = (process.env[key] ?? "").split(",").map((value) => value.trim()).filter((value) => Boolean(value));
    for (const host of loopback) {
      if (items.some((value) => value.toLowerCase() === host)) continue;
      items.push(host);
    }
    process.env[key] = items.join(",");
  };
  upsert("NO_PROXY");
  upsert("no_proxy");
}
const main = Effect.gen(function* () {
  contextMenu({ showSaveImageAs: true, showLookUpSelection: false, showSearchWithGoogle: false });
  try {
    process.chdir(homedir());
  } catch {
  }
  process.env.OPENCODE_DISABLE_EMBEDDED_WEB_UI = "true";
  const appId = app.isPackaged ? APP_IDS[CHANNEL] : "ai.opencode.desktop.dev";
  const onboardingTestRoot = (() => {
    if (!TEST_ONBOARDING) return;
    const root2 = join(tmpdir(), `opencode-onboarding-${randomUUID()}`);
    rmSync(root2, { recursive: true, force: true });
    ["data", "config", "cache", "state", "desktop", "session"].forEach(
      (dir) => mkdirSync(join(root2, dir), { recursive: true })
    );
    process.env.OPENCODE_DB = ":memory:";
    process.env.XDG_DATA_HOME = join(root2, "data");
    process.env.XDG_CONFIG_HOME = join(root2, "config");
    process.env.XDG_CACHE_HOME = join(root2, "cache");
    process.env.XDG_STATE_HOME = join(root2, "state");
    return root2;
  })();
  app.setName(app.isPackaged ? APP_NAMES[CHANNEL] : "OpenCode Dev");
  app.setAppUserModelId(appId);
  app.setPath(
    "userData",
    onboardingTestRoot ? join(onboardingTestRoot, "desktop") : join(app.getPath("appData"), appId)
  );
  if (onboardingTestRoot) app.setPath("sessionData", join(onboardingTestRoot, "session"));
  logger = initLogging();
  try {
    setDefaultCACertificates([.../* @__PURE__ */ new Set([...getCACertificates("default"), ...getCACertificates("system")])]);
  } catch (error) {
    logger.warn("failed to load system certificates", error);
  }
  logger.log("app starting", {
    version: app.getVersion(),
    packaged: app.isPackaged,
    onboardingTest: Boolean(onboardingTestRoot)
  });
  ensureLoopbackNoProxy();
  useEnvProxy();
  app.commandLine.appendSwitch("proxy-bypass-list", "<-loopback>");
  if (!app.isPackaged) app.commandLine.appendSwitch("remote-debugging-port", "9222");
  if (!app.requestSingleInstanceLock()) {
    app.quit();
    return;
  }
  preferAppEnv(app.getPath("userData"));
  app.on("second-instance", (_event, argv) => {
    const urls = argv.filter((arg) => arg.startsWith("opencode://"));
    if (urls.length) {
      logger.log("deep link received via second-instance", { urls });
      emitDeepLinks(urls);
    }
    if (mainWindow) {
      mainWindow.show();
      mainWindow.focus();
    }
  });
  app.on("open-url", (event, url2) => {
    event.preventDefault();
    logger.log("deep link received via open-url", { url: url2 });
    emitDeepLinks([url2]);
  });
  app.on("before-quit", () => {
    void killSidecar();
  });
  app.on("will-quit", () => {
    void killSidecar();
  });
  for (const signal of ["SIGINT", "SIGTERM"]) {
    process.on(signal, () => {
      void killSidecar().finally(() => app.exit(0));
    });
  }
  const serverReady = Deferred.makeUnsafe();
  const loadingComplete = Deferred.makeUnsafe();
  registerIpcHandlers({
    killSidecar: () => killSidecar(),
    awaitInitialization: Effect.fnUntraced(
      function* (sendStep) {
        sendStep(initStep);
        const listener = (step) => sendStep(step);
        initEmitter.on("step", listener);
        try {
          logger.log("awaiting server ready");
          const res = yield* Deferred.await(serverReady);
          logger.log("server ready", { url: res.url });
          return res;
        } finally {
          initEmitter.off("step", listener);
        }
      },
      (e) => Effect.runPromise(e)
    ),
    getWindowConfig: () => ({ updaterEnabled: UPDATER_ENABLED }),
    consumeInitialDeepLinks: () => pendingDeepLinks.splice(0),
    getDefaultServerUrl: () => getDefaultServerUrl(),
    setDefaultServerUrl: (url2) => setDefaultServerUrl(url2),
    getWslConfig: () => Promise.resolve(getWslConfig()),
    setWslConfig: (config) => setWslConfig(config),
    getDisplayBackend: async () => null,
    setDisplayBackend: async () => void 0,
    parseMarkdown: async (markdown) => parseMarkdown(markdown),
    checkAppExists: (appName) => checkAppExists(appName),
    wslPath: async (path, mode) => wslPath(path, mode),
    resolveAppPath: async (appName) => resolveAppPath(appName),
    loadingWindowComplete: () => Deferred.doneUnsafe(loadingComplete, Effect.void),
    runUpdater: async (alertOnFail) => checkForUpdates(alertOnFail, killSidecar),
    checkUpdate: async () => checkUpdate(),
    installUpdate: async () => installUpdate(killSidecar),
    setBackgroundColor: (color) => setBackgroundColor(color)
  });
  yield* Effect.promise(() => app.whenReady());
  if (!TEST_ONBOARDING) migrate();
  app.setAsDefaultProtocolClient("opencode");
  registerRendererProtocol();
  setDockIcon();
  setupAutoUpdater();
  const needsMigration = (() => {
    if (process.env.OPENCODE_DB === ":memory:") return false;
    const xdg = process.env.XDG_DATA_HOME;
    const base = xdg && xdg.length > 0 ? xdg : join(homedir(), ".local", "share");
    return !existsSync(join(base, "opencode", "opencode.db"));
  })();
  let overlay2 = null;
  const port = yield* Effect.gen(function* () {
    const fromEnv = process.env.OPENCODE_PORT;
    if (fromEnv) {
      const parsed = Number.parseInt(fromEnv, 10);
      if (!Number.isNaN(parsed)) return parsed;
    }
    const res = yield* Deferred.make();
    const server2 = createServer();
    server2.on("error", (e) => Deferred.failSync(res, () => e));
    server2.listen(0, "127.0.0.1", () => {
      const address = server2.address();
      if (typeof address !== "object" || !address) {
        server2.close();
        Deferred.failSync(res, () => new Error("Failed to get port"));
        return;
      }
      const port2 = address.port;
      server2.close(() => Effect.runSync(Deferred.succeed(res, port2)));
    });
    return yield* Deferred.await(res);
  });
  const hostname = "127.0.0.1";
  const url = `http://${hostname}:${port}`;
  const password = randomUUID();
  const loadingTask = yield* Effect.gen(function* () {
    logger.log("sidecar connection started", { url });
    initEmitter.on("sqlite", (progress) => {
      setInitStep({ phase: "sqlite_waiting" });
      if (overlay2) sendSqliteMigrationProgress(overlay2, progress);
      if (mainWindow) sendSqliteMigrationProgress(mainWindow, progress);
    });
    ensureLoopbackNoProxy();
    useEnvProxy();
    logger.log("spawning sidecar", { url });
    const { listener, health } = yield* Effect.promise(
      () => spawnLocalServer(hostname, port, password, {
        needsMigration,
        userDataPath: app.getPath("userData"),
        onSqliteProgress: (progress) => initEmitter.emit("sqlite", progress),
        onStdout: (message) => logger.log("sidecar stdout", { message }),
        onStderr: (message) => logger.warn("sidecar stderr", { message }),
        onExit: (code) => logger.warn("sidecar exited", { code })
      })
    );
    server = listener;
    yield* Deferred.succeed(serverReady, {
      url,
      username: "opencode",
      password
    });
    yield* Effect.promise(() => health.wait).pipe(
      Effect.timeout("30 seconds"),
      Effect.catch(
        (e) => Effect.sync(() => {
          logger.error("sidecar health check failed", e.toString());
        })
      )
    );
    logger.log("loading task finished");
  }).pipe(Effect.forkChild);
  if (needsMigration) {
    const show = yield* loadingTask.pipe(
      Fiber.await,
      Effect.timeout("1 second"),
      Effect.as(false),
      Effect.catch(() => Effect.succeed(true))
    );
    if (show) {
      overlay2 = createLoadingWindow();
      yield* Effect.sleep("1 second");
    }
  }
  yield* Fiber.await(loadingTask);
  setInitStep({ phase: "done" });
  if (overlay2) yield* Deferred.await(loadingComplete);
  mainWindow = createMainWindow();
  if (mainWindow) {
    createMenu({
      trigger: (id) => mainWindow && sendMenuCommand(mainWindow, id),
      checkForUpdates: () => {
        void checkForUpdates(true, killSidecar);
      },
      reload: () => mainWindow?.reload(),
      relaunch: () => {
        void killSidecar().finally(() => {
          app.relaunch();
          app.exit(0);
        });
      }
    });
  }
  overlay2?.close();
});
Effect.runFork(main);
