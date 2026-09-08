#!/usr/bin/env node
// Manifest-to-source wiring checks.
//
// This file exists because of a real bug that shipped: `viewsContainers` declared
// a container with id "anomalift" while `views` registered the panel under the key
// "cx". VS Code silently attaches a view to nothing when the container is missing,
// so the extension's only UI never appeared — and `tsc` was perfectly happy,
// because none of this wiring is TypeScript. It is JSON strings on one side and
// string literals on the other, and nothing in the toolchain compares them.
//
// So: compare them here. Every check below corresponds to a way the manifest and
// the source can disagree while both remain individually valid. Plain Node, no
// dependencies, so it runs anywhere `npm run compile` already runs.

const fs = require("node:fs");
const path = require("node:path");

const root = __dirname;
const pkg = JSON.parse(fs.readFileSync(path.join(root, "package.json"), "utf8"));
const srcPath = path.join(root, "src", "extension.ts");
const src = fs.readFileSync(srcPath, "utf8");

const failures = [];
const checks = [];

function check(name, fn) {
  const before = failures.length;
  fn((msg) => failures.push(`${name}: ${msg}`));
  checks.push({ name, ok: failures.length === before });
}

/** All string literals passed to `fn(...)` as its first argument. */
function literalArgs(pattern) {
  const out = new Set();
  for (const m of src.matchAll(pattern)) {
    out.add(m[1]);
  }
  return out;
}

const contributes = pkg.contributes ?? {};

// Container ids VS Code provides itself; a view may legitimately target these
// without the extension declaring a container.
const BUILTIN_CONTAINERS = new Set(["explorer", "scm", "debug", "test"]);

const declaredContainers = new Set(BUILTIN_CONTAINERS);
for (const group of Object.values(contributes.viewsContainers ?? {})) {
  for (const container of group) {
    declaredContainers.add(container.id);
  }
}

const declaredViews = new Set();
for (const [containerKey, views] of Object.entries(contributes.views ?? {})) {
  for (const view of views) {
    declaredViews.add(view.id);
  }
  void containerKey;
}

const manifestCommands = new Set((contributes.commands ?? []).map((c) => c.command));
const registeredCommands = literalArgs(/registerCommand\s*\(\s*["'`]([^"'`]+)["'`]/g);
const providedViews = literalArgs(/register(?:TreeDataProvider|WebviewViewProvider)\s*\(\s*["'`]([^"'`]+)["'`]/g);

// --- 1. The defect that shipped -------------------------------------------
check("views attach to a real container", (fail) => {
  for (const key of Object.keys(contributes.views ?? {})) {
    if (!declaredContainers.has(key)) {
      fail(
        `contributes.views["${key}"] has no matching viewsContainers id ` +
          `(declared: ${[...declaredContainers].filter((c) => !BUILTIN_CONTAINERS.has(c)).join(", ") || "none"}). ` +
          `The view will never appear.`,
      );
    }
  }
});

check("every declared container holds at least one view", (fail) => {
  const viewKeys = new Set(Object.keys(contributes.views ?? {}));
  for (const group of Object.values(contributes.viewsContainers ?? {})) {
    for (const container of group) {
      if (!viewKeys.has(container.id)) {
        fail(`viewsContainer "${container.id}" contributes no views; it renders as an empty icon.`);
      }
    }
  }
});

check("container icons exist on disk", (fail) => {
  for (const group of Object.values(contributes.viewsContainers ?? {})) {
    for (const container of group) {
      if (container.icon && !fs.existsSync(path.join(root, container.icon))) {
        fail(`viewsContainer "${container.id}" icon ${container.icon} is missing.`);
      }
    }
  }
});

// --- 2. Commands ----------------------------------------------------------
check("manifest commands are registered in source", (fail) => {
  for (const command of manifestCommands) {
    if (!registeredCommands.has(command)) {
      fail(`"${command}" is in contributes.commands but never registerCommand'd; the palette entry throws.`);
    }
  }
});

check("registered commands are declared in the manifest", (fail) => {
  for (const command of registeredCommands) {
    if (!manifestCommands.has(command)) {
      fail(`"${command}" is registered in extension.ts but absent from contributes.commands; it is unreachable from the UI.`);
    }
  }
});

check("menus and welcome views reference known commands and views", (fail) => {
  for (const [menu, entries] of Object.entries(contributes.menus ?? {})) {
    for (const entry of entries) {
      if (entry.command && !manifestCommands.has(entry.command)) {
        fail(`menus.${menu} references unknown command "${entry.command}".`);
      }
      const viewRef = /view\s*==\s*([\w.-]+)/.exec(entry.when ?? "");
      if (viewRef && !declaredViews.has(viewRef[1])) {
        fail(`menus.${menu} "when" references unknown view "${viewRef[1]}".`);
      }
    }
  }
  for (const welcome of contributes.viewsWelcome ?? []) {
    if (!declaredViews.has(welcome.view)) {
      fail(`viewsWelcome targets unknown view "${welcome.view}".`);
    }
    for (const m of (welcome.contents ?? "").matchAll(/command:([\w.-]+)/g)) {
      if (!manifestCommands.has(m[1])) {
        fail(`viewsWelcome for "${welcome.view}" links unknown command "${m[1]}".`);
      }
    }
  }
});

// --- 3. Views the source provides ----------------------------------------
check("tree data providers match declared views", (fail) => {
  for (const view of providedViews) {
    if (!declaredViews.has(view)) {
      fail(`extension.ts provides data for "${view}", which the manifest does not declare.`);
    }
  }
  for (const view of declaredViews) {
    if (!providedViews.has(view)) {
      fail(`view "${view}" is declared but nothing in extension.ts provides its content.`);
    }
  }
});

// --- 4. Configuration -----------------------------------------------------
check("configuration keys are read by the source", (fail) => {
  const sections = literalArgs(/getConfiguration\s*\(\s*["'`]([^"'`]+)["'`]/g);
  const names = literalArgs(/\.(?:get|update)\s*(?:<[^>]*>)?\s*\(\s*["'`]([^"'`]+)["'`]/g);
  for (const key of Object.keys(contributes.configuration?.properties ?? {})) {
    const dot = key.indexOf(".");
    const section = key.slice(0, dot);
    const name = key.slice(dot + 1);
    if (!sections.has(section)) {
      fail(`"${key}" is declared but getConfiguration("${section}") never appears in extension.ts.`);
    } else if (!names.has(name)) {
      fail(`"${key}" is declared but never read: no .get("${name}") in extension.ts.`);
    }
  }
});

// --- 5. Privacy ------------------------------------------------------------
// The binary's JSON carries a `queries` field: the user's own task descriptions,
// verbatim. Nothing in the editor may render it. Parsing copies fields by
// allowlist rather than spreading, so any appearance of the name in the source is
// either a leak or a comment about one.
check("the private `queries` field is never touched", (fail) => {
  const withoutComments = src
    .replace(/\/\*[\s\S]*?\*\//g, "")
    .replace(/(^|[^:])\/\/.*$/gm, "$1");
  if (/\bqueries\b/.test(withoutComments)) {
    fail("extension.ts references `queries` outside a comment. That field holds the user's prompts.");
  }
});

// --- 6. Dead branding and stale invocations --------------------------------
// The tool was called `cx` under a previous name; every surviving reference is a
// user-visible lie, and `cx learn` was a command line the binary no longer accepts.
check("no leftover `cx` branding", (fail) => {
  for (const file of ["package.json", "README.md", path.join("src", "extension.ts")]) {
    const full = path.join(root, file);
    // Report rather than throw: a missing README is itself a shipping defect,
    // and a stack trace here would hide every other check's result.
    if (!fs.existsSync(full)) {
      fail(`${file} is missing.`);
      continue;
    }
    const text = fs.readFileSync(full, "utf8");
    const hit = /(^|[^\w-])cx($|[^\w-])/.exec(text);
    if (hit) {
      const line = text.slice(0, hit.index).split("\n").length;
      fail(`${file}:${line} still refers to "cx".`);
    }
  }
  if (pkg.publisher === "context-os") {
    fail(`publisher "context-os" is dead branding from an abandoned project.`);
  }
});

check("the scan is invoked as bare `anomalift --json`", (fail) => {
  if (/\$\{quote\(bin\)\}\s+learn\b/.test(src) || /anomalift\s+learn\b/.test(src.replace(/\/\/.*$/gm, ""))) {
    fail("extension.ts still shells out to a `learn` subcommand; bare `anomalift --json` is the scan.");
  }
  if (!/\$\{quote\(bin\)\}[^`]*--json/.test(src)) {
    fail("no `--json` invocation of the binary found; the extension cannot parse a human report.");
  }
});

// --- 7. Packaging ----------------------------------------------------------
check("entry point exists", (fail) => {
  if (!fs.existsSync(path.join(root, pkg.main))) {
    fail(`main "${pkg.main}" does not exist. Run \`npm run compile\` first.`);
  }
});

for (const { name, ok } of checks) {
  console.log(`${ok ? "ok  " : "FAIL"}  ${name}`);
}

if (failures.length > 0) {
  console.error(`\n${failures.length} problem(s):`);
  for (const f of failures) {
    console.error(`  - ${f}`);
  }
  process.exit(1);
}

console.log(`\n${checks.length} checks passed.`);
