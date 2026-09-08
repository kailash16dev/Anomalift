// anomalift — surfacing repeated agent failures inside the editor.
//
// The extension deliberately does no parsing of its own. It shells out to
// `anomalift --json` and renders the result. There is exactly one implementation
// of signature normalisation and clustering, in Rust, and it is the one that was
// tested against real transcripts; a second implementation here would drift from
// it and produce two different answers to the same question.
//
// Nothing is scanned unless asked. Reading every transcript on a large history
// is not free, and doing it unprompted at window open would make the editor feel
// slow for a feature the user did not invoke.

import { exec } from "node:child_process";
import * as fs from "node:fs";
import * as os from "node:os";
import * as path from "node:path";
import { promisify } from "node:util";
import * as vscode from "vscode";

const run = promisify(exec);

/**
 * Exactly the fields of the binary's JSON that this extension is allowed to see.
 *
 * The binary also emits `queries` — the user's own task descriptions, verbatim,
 * for the requests that were in flight when the failure happened. Those are the
 * most private thing in a transcript, and a sidebar is a shoulder-surfable
 * surface. Omitting the field from this type is not enough on its own, because
 * `JSON.parse` returns it regardless and a cast would smuggle it through; see
 * `sanitize` below, which drops it at the boundary so no later code *can* render
 * it by accident.
 */
interface Pattern {
  signature: string;
  tool: string;
  count: number;
  sessions: number;
  examples: string[];
  first_seen: number;
  last_seen: number;
  /** Supplied by the binary, not recomputed here. */
  rule: string | null;
  recurring: boolean;
}

/**
 * Copy across only the safe fields, field by field.
 *
 * A spread with a delete would be shorter and wrong: it keeps whatever new
 * fields the binary grows next, and the next private field would leak the day it
 * ships. An allowlist fails closed instead — an unknown field is simply not
 * carried, and adding one here is a deliberate act.
 */
function sanitize(raw: unknown): Pattern[] {
  if (!Array.isArray(raw)) {
    throw new Error("expected a JSON array of patterns");
  }
  return raw.map((p: Record<string, unknown>) => ({
    signature: String(p.signature ?? ""),
    tool: String(p.tool ?? ""),
    count: Number(p.count ?? 0),
    sessions: Number(p.sessions ?? 0),
    examples: Array.isArray(p.examples) ? p.examples.map(String) : [],
    first_seen: Number(p.first_seen ?? 0),
    last_seen: Number(p.last_seen ?? 0),
    rule: typeof p.rule === "string" ? p.rule : null,
    recurring: p.recurring === true,
  }));
}

/**
 * Locate the binary: explicit setting, then PATH, then a workspace build.
 *
 * The workspace fallback exists so the extension is usable while developing anomalift
 * itself, without installing it first.
 */
async function findBinary(): Promise<string | undefined> {
  const configured = vscode.workspace.getConfiguration("anomalift").get<string>("path");
  if (configured) {
    return fs.existsSync(configured) ? configured : undefined;
  }
  try {
    await run("anomalift --version");
    return "anomalift";
  } catch {
    // Not on PATH; fall through to a local build.
  }
  for (const folder of vscode.workspace.workspaceFolders ?? []) {
    for (const profile of ["release", "debug"]) {
      const candidate = path.join(folder.uri.fsPath, "target", profile, "anomalift");
      if (fs.existsSync(candidate)) {
        return candidate;
      }
    }
  }
  return undefined;
}

function quote(p: string): string {
  return p === "anomalift" ? p : `"${p}"`;
}

/**
 * Where to run the scan from.
 *
 * Not cosmetic: a scan also refreshes the `.anomalift/patterns.json` cache that
 * `anomalift mcp` reads, and the binary resolves that path relative to the
 * working directory. The extension host's own cwd is an implementation detail of
 * how the editor was launched — `/` when VS Code is opened from the Dock — so
 * inheriting it would scatter caches outside any project. The first workspace
 * folder is the directory the user thinks of as "here"; with no folder open,
 * home is at least writable and predictable.
 */
function scanCwd(): string {
  return vscode.workspace.workspaceFolders?.[0]?.uri.fsPath ?? os.homedir();
}

class PatternItem extends vscode.TreeItem {
  constructor(
    public readonly pattern: Pattern,
    public readonly rule: string | undefined,
  ) {
    super(`${pattern.count}×  ${pattern.tool}`, vscode.TreeItemCollapsibleState.None);

    // The signature is the useful label, but it is long; the description slot
    // truncates gracefully and the tooltip carries the verbatim error, which is
    // what lets someone judge the pattern rather than trust the clusterer.
    this.description = stripToolPrefix(pattern.signature, pattern.tool);
    this.tooltip = new vscode.MarkdownString(
      [
        `**${pattern.count} failures** across **${pattern.sessions} sessions**`,
        "",
        rule ? `**Suggested rule**\n\n${rule}` : "_No known fix — review this one yourself._",
        "",
        "**Example**",
        "```",
        (pattern.examples[0] ?? "").slice(0, 400),
        "```",
      ].join("\n"),
    );
    this.iconPath = new vscode.ThemeIcon(
      pattern.count >= 10 ? "error" : "warning",
      new vscode.ThemeColor(pattern.count >= 10 ? "errorForeground" : "editorWarning.foreground"),
    );
    // Only patterns with a rule get the copy action, so the menu never offers
    // to copy advice that does not exist.
    this.contextValue = rule ? "anomaliftPatternWithRule" : "anomaliftPattern";
  }
}

function stripToolPrefix(signature: string, tool: string): string {
  const prefix = `${tool}: `;
  return signature.startsWith(prefix) ? signature.slice(prefix.length) : signature;
}

class PatternProvider implements vscode.TreeDataProvider<PatternItem> {
  private readonly changed = new vscode.EventEmitter<void>();
  readonly onDidChangeTreeData = this.changed.event;
  private items: PatternItem[] = [];

  getTreeItem(e: PatternItem): vscode.TreeItem {
    return e;
  }

  getChildren(): PatternItem[] {
    return this.items;
  }

  async refresh(status: vscode.StatusBarItem): Promise<void> {
    const bin = await findBinary();
    if (!bin) {
      // Actionable rather than a bare failure: the overwhelmingly likely cause
      // is that anomalift is not installed, and the user can fix that.
      const choice = await vscode.window.showWarningMessage(
        "anomalift binary not found on PATH or in this workspace.",
        "Set path",
      );
      if (choice === "Set path") {
        await vscode.commands.executeCommand("anomalift.configure");
      }
      return;
    }

    const sessions = vscode.workspace.getConfiguration("anomalift").get<number>("sessions", 120);
    let patterns: Pattern[];
    try {
      patterns = await vscode.window.withProgress(
        { location: { viewId: "anomalift.patterns" }, title: "Scanning sessions…" },
        async () => {
          // Bare `anomalift` *is* the scan; there is no `learn` verb to type.
          // (One survives hidden in the CLI for old muscle memory, but calling a
          // hidden alias from a program is how an extension breaks on the day it
          // is finally removed.)
          //
          // Transcript histories reach hundreds of megabytes; the default 1MB
          // buffer truncates the JSON and yields a parse error that looks like
          // a bug in anomalift rather than a limit here.
          const { stdout } = await run(`${quote(bin)} --sessions ${sessions} --json`, {
            cwd: scanCwd(),
            maxBuffer: 64 * 1024 * 1024,
          });
          return sanitize(JSON.parse(stdout));
        },
      );
    } catch (err) {
      vscode.window.showErrorMessage(`anomalift scan failed: ${(err as Error).message.slice(0, 300)}`);
      return;
    }

    const recurring = patterns.filter((p) => p.recurring);
    this.items = recurring.map((p) => new PatternItem(p, p.rule ?? undefined));
    this.changed.fire();

    const clustered = patterns.reduce((n, p) => n + p.count, 0);
    const repeated = recurring.reduce((n, p) => n + p.count, 0);
    const pct = clustered > 0 ? Math.round((repeated / clustered) * 100) : 0;

    if (recurring.length === 0) {
      status.hide();
      vscode.window.showInformationMessage(
        clustered === 0
          ? "anomalift: no failures with a usable message. Nothing to learn from yet."
          : `anomalift: ${clustered} failures, none repeated across sessions yet.`,
      );
      return;
    }

    status.text = `$(warning) ${recurring.length} repeated failures`;
    status.tooltip = `${pct}% of clustered failures recur — ${repeated} of ${clustered}`;
    status.command = "anomalift.refresh";
    status.show();
  }
}

export function activate(context: vscode.ExtensionContext): void {
  const provider = new PatternProvider();
  const status = vscode.window.createStatusBarItem(vscode.StatusBarAlignment.Right, 100);

  context.subscriptions.push(
    status,
    vscode.window.registerTreeDataProvider("anomalift.patterns", provider),
    vscode.commands.registerCommand("anomalift.refresh", () => provider.refresh(status)),
    vscode.commands.registerCommand("anomalift.configure", async () => {
      const value = await vscode.window.showInputBox({
        prompt: "Absolute path to the anomalift binary",
        placeHolder: "/usr/local/bin/anomalift",
        validateInput: (v) =>
          !v || fs.existsSync(v) ? undefined : "No file at that path",
      });
      if (value !== undefined) {
        await vscode.workspace
          .getConfiguration("anomalift")
          .update("path", value, vscode.ConfigurationTarget.Global);
        await provider.refresh(status);
      }
    }),
    vscode.commands.registerCommand("anomalift.copyRule", async (item: PatternItem) => {
      if (!item?.rule) {
        return;
      }
      // Copy rather than write. CLAUDE.md is the user's project config, and an
      // extension editing it from a context menu is too easy to do by accident.
      await vscode.env.clipboard.writeText(`- ${item.rule}`);
      vscode.window.showInformationMessage("Rule copied — paste it into CLAUDE.md.");
    }),
  );

  if (vscode.workspace.getConfiguration("anomalift").get<boolean>("scanOnStartup", false)) {
    void provider.refresh(status);
  }
}

export function deactivate(): void {
  // Nothing to tear down: no watchers, no daemon, no network.
}
