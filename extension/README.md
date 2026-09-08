# Anomalift — agent failure patterns

Finds the mistakes your coding agent makes **over and over**, and the one-line rules
that stop them.

Your agent fails, retries, and moves on. Nothing remembers, so the same flag conflict
or the same bad path comes back next week and costs the same tokens again.

## What it shows

A sidebar list of failures that recurred, newest scan first:

```
19×  Bash   git: --name-only and -s cannot be used together
13×  Grep   rg: unrecognized file type: tsx
11×  Read   EISDIR: illegal operation on a directory
 6×  Bash   no such file or directory: packages/web
```

*(Layout, not real output. What appears depends entirely on your own history.)*

Hover for the verbatim error and a suggested rule. Right-click to copy that rule for
`CLAUDE.md`.

## Which agents

**Claude Code** — both the CLI and the VS Code extension. They are the same program and
share `~/.claude/projects`, so one reader covers both.

Codex and Copilot are **not** supported. Codex stores sessions as zstd-compressed
rollouts plus a SQLite database, mid-migration between two formats; Copilot's chat
storage records file snapshots rather than tool calls and errors. Neither could be
verified against real data, and a parser written against a guessed format looks like it
works while silently dropping everything. When either becomes verifiable it is one file
— the clustering above it is already agent-agnostic.

## Requirements

The `anomalift` binary. The extension finds it on `PATH`, or in `target/release` and
`target/debug` when this repo is the workspace, or wherever `anomalift.path` points.

## Settings

| Key | Default | What it does |
| --- | --- | --- |
| `anomalift.path` | `""` | Path to the binary. Empty means search `PATH`, then the workspace build. |
| `anomalift.sessions` | `120` | How many recent sessions each scan reads. |
| `anomalift.scanOnStartup` | `false` | Scan when the window opens. Off by default. |

## What it does not do

- **No network.** Nothing is uploaded, there is no account, and there is no telemetry.
- **No writes to your config.** Rules are copied to the clipboard; you paste them. An
  extension editing `CLAUDE.md` from a context menu is too easy to trigger by accident.
- **No scanning unless asked.** `anomalift.scanOnStartup` is off by default, because
  reading a large transcript history unprompted would make the editor feel slow for a
  feature you did not invoke.
- **No showing your prompts.** The binary reports, alongside each pattern, the requests
  that were in flight when it fired — your own task descriptions, verbatim. The
  extension drops that field as it parses, before anything can render it. A sidebar is
  visible to anyone standing behind you.
- **No invented advice.** A pattern with no known remedy says *"no known fix — review
  this one yourself"* rather than guessing. A confidently wrong line in `CLAUDE.md`
  misleads the agent on every future turn.

## Thresholds, and why

A pattern must recur **3+ times across 2+ sessions** before it appears. Ten failures in
one afternoon is one bad afternoon; the same failure across separate sessions is a
habit, and only habits are worth a standing instruction.

Failures with no message — a bare `Exit code 1` — are excluded entirely. They are
typically the single largest bucket, and they carry no lesson.

## Developing

```sh
npm install
npm run verify   # compile, then check the manifest against the source, then run it
```

Three steps, because the first two are not enough on their own:

- `tsc` proves the TypeScript is well typed.
- `verify.js` compares the manifest to the source — view ids against container ids,
  contributed commands against `registerCommand`, settings against the code that reads
  them. None of that is TypeScript; it is JSON strings on one side and string literals
  on the other, and nothing in the toolchain compares them.
- `smoke.js` loads the compiled extension against a stub `vscode`, calls `activate()`,
  runs a real scan through the real binary, and asserts nothing private reaches the
  tree.

This exists because the extension once shipped with its sidebar registered under a
container id that did not exist. The only UI it has never appeared, and it compiled
cleanly the whole time.
