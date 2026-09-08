# anomalift beta

`anomalift` reads the Claude Code transcripts already on your disk, finds tool
failures the agent keeps repeating, writes a few one-line rules into your
`CLAUDE.md`, and then measures whether those rules changed anything. That last
step is the open question, and it cannot be answered on one machine — which is
what this beta is for.

The output is one markdown file. It contains counts, rates and verdicts, and
nothing else — no paths, no queries, no error text. The
[exact contents are listed below](#what-is-in-the-report-and-what-is-not) so it
can be checked before it is sent.

Total effort: about five minutes of typing, spread over a month of ordinary
work.

---

## 1. Install

**With Node** — the route that works today:

```sh
npx anomalift
```

One prebuilt binary for your platform, no Rust, no compilation, no install
script. `npm install -g anomalift` to keep it around.

**Direct download** — needs the repository to be public, so these will not
resolve while it is private:

```sh
# macOS / Linux
curl -fsSL https://raw.githubusercontent.com/kailash16dev/Anomalift/main/install.sh | sh

# Windows (PowerShell)
irm https://raw.githubusercontent.com/kailash16dev/Anomalift/main/install.ps1 | iex
```

Both installers verify the download against the published SHA-256 and refuse to
install on a mismatch, or when no checksum has been published at all. Set
`ANOMALIFT_SKIP_CHECKSUM=1` if you want to override that; nothing else will.

They differ in where they put things, because the platforms differ:

- **`install.sh`** drops one binary in `~/.local/bin` (override with
  `ANOMALIFT_BIN_DIR`) and does not touch your shell profile. If that directory
  is not on your `PATH` it prints the line to add and leaves the editing to you.
- **`install.ps1`** installs to `%LOCALAPPDATA%\Programs\anomalift` and *does*
  add that directory to your user `PATH` — user scope only, never machine scope.
  A new shell picks it up.

Check it worked:

```sh
anomalift --version
```

### The binaries are unsigned. Here is what that means.

There is no Apple Developer certificate and no Windows code-signing certificate
behind these builds, so the binaries are unsigned. Both operating systems will
say so, in slightly alarming language.

**macOS.** Anything downloaded gets a `com.apple.quarantine` attribute, and the
first run then produces a dialog saying Apple cannot verify the developer.
`install.sh` strips that attribute from the binary it just downloaded, so if you
used the curl command above **you will not see this dialog at all**. That is the
same trust decision already made by piping the script into `sh`, made explicit
rather than deferred to a popup.

If you download the binary by hand from the releases page instead, you will see
it. The fix:

```sh
xattr -d com.apple.quarantine ~/Downloads/anomalift-macos-arm64
chmod +x ~/Downloads/anomalift-macos-arm64
```

(Use `anomalift-macos-x86_64` on an Intel Mac.) The alternative is right-click →
Open in Finder, then Open again in the dialog, which does the same thing through
the GUI.

**Windows.** SmartScreen will show "Windows protected your PC" for an unsigned
executable downloaded from the internet. Click **More info**, then **Run
anyway**. From PowerShell, the equivalent is:

```powershell
Unblock-File .\anomalift-windows-x86_64.exe
```

If either of those is more trust than you want to extend, build it yourself —
`cargo build --release` — and skip the installers entirely.

---

## 2. Use your agent normally for two to three weeks

Do nothing else. `anomalift` learns from sessions you have already run, so the
first thing it needs is history. If you have been using Claude Code for a few
weeks already, you have enough right now and can skip straight to step 3.

You can look at what it has found whenever you like:

```sh
anomalift
```

Reading transcripts is all this does. It writes nothing except a small cache
under `.anomalift/`.

A pattern only counts as recurring if it happened **3 or more times across 2 or
more sessions**. Failures with no message (`Exit code 1` on its own) and
failures you caused by rejecting a tool call are dropped entirely — neither
teaches the agent anything.

---

## 3. `anomalift apply`

Run this from the directory containing the `CLAUDE.md` you want the rules in.

```sh
anomalift apply
```

**It edits your `CLAUDE.md`.** Specifically:

- It shows you the full diff and asks for a `y` before writing anything. Answer
  anything else and nothing happens. If stdin is not a terminal it refuses
  outright rather than assuming yes.
- It copies your file to `CLAUDE.md.anomalift-backup-<timestamp>` before
  writing, every time.
- It only ever touches the region between `<!-- anomalift:begin -->` and
  `<!-- anomalift:end -->`. Everything outside those markers is preserved byte
  for byte — it does not reflow, reorder or reformat your file.
- If those markers look damaged — a begin with no end, two blocks, an end before
  a begin — **it refuses and exits without writing.** It will tell you which
  problem it found and ask you to fix or delete the markers by hand. There is no
  safe guess about what you meant in a file you wrote yourself.
- Markers inside a fenced code block are ignored, so documenting `anomalift` in
  your own `CLAUDE.md` will not cause it to rewrite your example.

Use `anomalift apply --dry-run` first if you want to see the diff without being
asked to commit to it.

Expect fewer rules than the scan reported. It only writes advice where the fix
is unambiguous; recurring patterns with no known fix are reported but never
written, because a confidently wrong line in `CLAUDE.md` misleads the agent on
every single turn. Those unruled patterns are kept as a **control group** —
they are the comparison that tells a working rule apart from ordinary drift, and
they are why the report at the end is worth anything.

---

## 4. Keep working

Another two to three weeks of normal use. This is the measurement window, and
there is no way to shorten it: the tool needs to see the agent attempt those
same commands again, after the rules were in context.

If you run `anomalift effect` during this period it will mostly say
`NO EVIDENCE`. That is expected and correct — it means the agent has not tried
the thing again yet, not that the rule failed. Do not read anything into an
early run.

---

## 5. `anomalift share`

From the same directory you ran `apply` in:

```sh
anomalift share
```

It writes `anomalift-report.md` in the current directory and prints a summary:

```
  wrote anomalift-report.md
  N ruled patterns · M controls
  no paths, queries or error text - safe to send as-is
```

Nothing is uploaded. There is no network code in this binary at all — its whole
dependency list is `anyhow`, `clap`, `serde` and `serde_json`. The file sits on
your disk until you decide to send it.

---

## 6. Send the file back

Open `anomalift-report.md`, read it — it is short — and send it back however is
convenient. If anything in it looks like it should not be leaving the machine,
say so instead of sending it: that is a redaction bug, and it is worth more as a
bug report than as data.

---

## What is in the report, and what is not

Redaction here is an **allow-list**, not a deny-list. Only fields known to be
safe are emitted. A deny-list would have to anticipate every way private text
can appear in an error message, and it would be wrong the first time someone's
stack trace contained a client name.

**What the file contains, in full:**

- the `anomalift` version, and your operating system as one bare word —
  `macos`, `linux` or `windows`
- how many sessions were scanned, and how many days they span
- a two-row table: patterns that got a rule vs. control patterns that did not,
  with the count in each arm and the median before→after change in percentage
  points, plus the difference between the two
- one row per measured pattern, in two tables (ruled and control), each row
  being: a truncated command shape, failures/attempts before, failures/attempts
  after, both as percentages, a verdict word (`works`, `unproven`,
  `no evidence`, `no improvement`), and a p-value

**What it does not contain:**

- **no file paths** — not absolute, not relative, not fragments
- **no queries or task descriptions** — nothing you typed, no ticket titles
- **no error text** — not the raw message, not the normalised signature
- **no repository or project names**
- **no usernames**
- **no machine identifiers** — no hostname, no home directory, no serial

**The command shapes, precisely**, because that is the only field derived from
anything you typed. Each is cut down to the program name. A second word survives
only for fifteen programs that take subcommands (`git`, `npm`, `cargo`,
`docker`, `kubectl`, `gh` and similar), and only when that word appears in the
enumerated list of *that program's* known subcommands:

| what was run | what appears in the report |
|---|---|
| `git show --name-only -s HEAD` | `git show` |
| `npm install` | `npm install` |
| `cat SidePanelPageLayout.tsx` | `cat` |
| `node scripts/deploy-acme.js` | `node` |
| `python train_client_model.py` | `python` |
| `git SomePrivateBranch` | `git` |
| `./deploy-acme.sh` | `(command)` |
| a `Read` that failed | `Read` |

The allow-list exists because the earlier approach — filtering out anything that
looked argument-shaped — let `cat SidePanelPageLayout` through in a real report.
Nothing about that word looks unsafe; it has no slash and no dot. There is no
rule that separates a subcommand from a filename by appearance, so only known
subcommand programs keep a second word and everything else is cut to one.

That is stricter than it sounds. `git internal-name` does not survive as
written — `internal-name` is not one of git's enumerated subcommands, so the
shape is cut to `git`. An unknown program name does not survive either:
`acmectl deploy` becomes `(command)`, because an unrecognised binary is very
often named after the company that wrote it.

What a second word can still be is a real subcommand you happened to run —
`git checkout`, `docker build`. That is the intended output. Skim the command
column before sending the file anyway; anything that survives which should not
have is worth reporting.

The report also states its own null result. If the ruled patterns did not fall
further than the untouched controls, the file says so in those words, without
being asked. That is deliberate: a report that can only be read one way is not
evidence.

---

## If something goes wrong

**Undo everything:**

```sh
anomalift forget
```

Removes the `anomalift` block from `CLAUDE.md` and nothing else, leaving the
file identical to how it was before `apply` — including trailing whitespace.
(One documented exception: a file that had no trailing newline at all will have
gained one.) It shows you the diff and asks first, and it backs the file up the
same way `apply` does.

`forget` also clears the frozen baselines in `.anomalift/learned.json`. That is
intentional — once the rules are out of the agent's context, any later drop in
failures cannot be attributed to them, so there is nothing left to measure and
the tool should not pretend otherwise.

**Restore by hand:** every write, by `apply` and by `forget` alike, copies the
previous file to `CLAUDE.md.anomalift-backup-<unix-timestamp>` in the same
directory first. If anything looks wrong, that file is your `CLAUDE.md` exactly
as it was seconds earlier. Nothing ever deletes these; clean them up whenever
you like.

**"refusing to touch CLAUDE.md: ..."** — the markers are damaged. Open the file,
delete the `<!-- anomalift:begin ... -->` / `<!-- anomalift:end -->` lines and
anything between them, and run the command again. It is refusing on purpose;
guessing here is how a tool eats a file you wrote by hand.

**"no anomalift block in CLAUDE.md to take a cutoff from"** — you ran `effect`
before `apply`. Run `apply` first.

**`share` reports zero patterns** — you are probably not in the directory you
ran `apply` from. The frozen baselines live in `./.anomalift/learned.json`,
beside the `CLAUDE.md` they were written for.

**The report says "Not enough measured patterns yet"** — that is the honest
answer and it is fine to send. It means the after-window is still too thin. Give
it another week and run `anomalift share` again.

**Uninstall:** delete the binary (`rm ~/.local/bin/anomalift`), run
`anomalift forget` first if you want your `CLAUDE.md` back, and delete the
`.anomalift/` directory. Nothing else on your machine was touched.

---
