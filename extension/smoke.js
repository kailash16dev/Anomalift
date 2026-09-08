#!/usr/bin/env node
// Load the compiled extension for real, with a stub `vscode`, and run it.
//
// `npm run verify` compares the manifest to the source; this runs the code. The
// two catch different things. Nobody could previously claim more than "it
// compiles", because the only way to exercise `activate()` was to open a second
// VS Code window by hand — so nobody did it, and a detached sidebar shipped.
//
// The stub implements only what extension.ts touches. That is deliberate: if the
// extension starts using a new API, this file fails loudly with "not a function"
// rather than silently pretending, which is the whole point of a smoke test.

const Module = require("node:module");
const path = require("node:path");

const root = __dirname;
const pkg = require(path.join(root, "package.json"));
const entry = path.join(root, pkg.main);

// The extension's own repo doubles as the workspace, so binary discovery
// exercises the `target/{release,debug}` fallback rather than needing an install.
const workspace = path.resolve(root, "..");
const SESSIONS = 20;

const log = [];
const config = { path: "", sessions: SESSIONS, scanOnStartup: false };

class EventEmitter {
  constructor() { this.handlers = []; }
  get event() { return (h) => { this.handlers.push(h); return { dispose() {} }; }; }
  fire(v) { for (const h of this.handlers) h(v); }
  dispose() {}
}

const commands = new Map();
let provider;

const vscode = {
  TreeItem: class { constructor(label, collapsibleState) { this.label = label; this.collapsibleState = collapsibleState; } },
  EventEmitter,
  TreeItemCollapsibleState: { None: 0, Collapsed: 1, Expanded: 2 },
  StatusBarAlignment: { Left: 1, Right: 2 },
  ConfigurationTarget: { Global: 1 },
  ThemeIcon: class { constructor(id, color) { this.id = id; this.color = color; } },
  ThemeColor: class { constructor(id) { this.id = id; } },
  MarkdownString: class { constructor(value) { this.value = value; } },
  env: { clipboard: { writeText: async (t) => log.push(["clipboard", t]) } },
  workspace: {
    workspaceFolders: [{ uri: { fsPath: workspace }, name: path.basename(workspace), index: 0 }],
    getConfiguration: () => ({
      get: (k, d) => (config[k] !== undefined ? config[k] : d),
      update: async (k, v) => { config[k] = v; },
    }),
  },
  window: {
    createStatusBarItem: () => ({ show() { this.visible = true; }, hide() { this.visible = false; }, dispose() {} }),
    registerTreeDataProvider: (id, p) => { log.push(["view", id]); provider = p; return { dispose() {} }; },
    withProgress: (opts, task) => task({ report() {} }, {}),
    showErrorMessage: async (m) => log.push(["error", m]),
    showWarningMessage: async (m) => log.push(["warn", m]),
    showInformationMessage: async (m) => log.push(["info", m]),
    showInputBox: async () => undefined,
  },
  commands: {
    registerCommand: (id, fn) => { commands.set(id, fn); return { dispose() {} }; },
    executeCommand: async (id, ...args) => commands.get(id)?.(...args),
  },
};

const load = Module._load;
Module._load = function (request) {
  return request === "vscode" ? vscode : load.apply(this, arguments);
};

function assert(cond, msg) {
  if (!cond) { throw new Error(msg); }
}

(async () => {
  const ext = require(entry);
  const subscriptions = [];
  ext.activate({ subscriptions, extensionPath: root });
  console.log(`ok    activate() — ${subscriptions.length} disposables`);

  for (const { command } of pkg.contributes.commands) {
    assert(commands.has(command), `command "${command}" is in the manifest but not registered at runtime`);
  }
  console.log(`ok    all ${pkg.contributes.commands.length} manifest commands registered at runtime`);

  const views = Object.values(pkg.contributes.views).flat().map((v) => v.id);
  for (const id of views) {
    assert(log.some(([kind, v]) => kind === "view" && v === id), `no provider registered for view "${id}"`);
  }
  console.log(`ok    tree provider bound to ${views.join(", ")}`);

  await commands.get("anomalift.refresh")();

  const errors = log.filter(([k]) => k === "error");
  assert(errors.length === 0, `scan failed: ${JSON.stringify(errors)}`);

  if (log.some(([k, m]) => k === "warn" && /binary not found/.test(m))) {
    console.log("SKIP  scan — no anomalift binary on PATH or in target/{release,debug}; build it to cover this");
    ext.deactivate();
    return;
  }

  const items = provider.getChildren();
  console.log(`ok    refresh() rendered ${items.length} items from a real ${SESSIONS}-session scan`);

  // The binary reports the user's own prompts alongside each pattern. Nothing
  // that reaches a tree item may carry them, at any depth.
  assert(!/"queries"/.test(JSON.stringify(items)), "PRIVACY: `queries` reached a rendered tree item");
  console.log("ok    no `queries` field in any rendered item");

  const withRule = items.find((i) => i.contextValue === "anomaliftPatternWithRule");
  if (withRule) {
    await commands.get("anomalift.copyRule")(withRule);
    const copied = log.find(([k]) => k === "clipboard");
    assert(copied && copied[1].startsWith("- "), "copyRule wrote nothing usable to the clipboard");
    console.log(`ok    copyRule — ${copied[1].slice(0, 70)}…`);
  }

  ext.deactivate();
  console.log("\nsmoke test passed.");
})().catch((err) => {
  console.error(`FAIL  ${err.message}`);
  process.exit(1);
});
