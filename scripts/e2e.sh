#!/usr/bin/env bash
# End-to-end test for anomalift, in an isolated HOME outside the project.
#
# Unit tests exercise functions; this exercises the binary a user actually runs,
# against synthetic transcripts whose correct answers are known by construction.
#
# Half of these cases are designed to FAIL if the tool is too eager. A tool that
# finds a pattern in everything is worse than useless, so "correctly found
# nothing" is asserted as strictly as "correctly found something".

set -uo pipefail
BIN="${1:?usage: e2e.sh /path/to/anomalift}"
# Absolute, because cases cd into a throwaway HOME to test cwd-relative output.
# A relative path silently stops resolving there, and the failure looks like a
# bug in the tool rather than in this script.
case "$BIN" in
  /*) ;;
  *) BIN="$(cd "$(dirname "$BIN")" && pwd)/$(basename "$BIN")" ;;
esac
[ -x "$BIN" ] || { echo "not executable: $BIN" >&2; exit 1; }
# Throwaway HOMEs go to a temp dir, never beside the script: this lives in the
# repo now, and a test that litters the working tree will end up committed.
ROOT="$(mktemp -d 2>/dev/null || mktemp -d -t anomalift-e2e)"
trap 'rm -rf "$ROOT"' EXIT INT TERM
PASS=0; FAIL=0

ok()   { printf "  \033[32mPASS\033[0m  %s\n" "$1"; PASS=$((PASS+1)); }
bad()  { printf "  \033[31mFAIL\033[0m  %s\n     expected: %s\n     got:      %s\n" "$1" "$2" "$3"; FAIL=$((FAIL+1)); }
check(){ [ "$2" = "$3" ] && ok "$1" || bad "$1" "$2" "$3"; }

# Fresh isolated HOME per case: no leakage between cases, none from the real machine.
new_home() {
  H="$ROOT/home-$1"; rm -rf "$H"; mkdir -p "$H/.claude/projects/proj" "$H/work"
  printf '# Project notes\n\nAlways run tests.\n' > "$H/work/CLAUDE.md"
  echo "$H"
}

# A session with `count` identical failing Bash calls.
session() { # file, command, error, count
  local f="$1" cmd="$2" err="$3" n="$4"
  : > "$f"
  printf '{"type":"user","message":{"content":"do the thing"}}\n' >> "$f"
  for i in $(seq 1 "$n"); do
    printf '{"type":"assistant","message":{"content":[{"type":"tool_use","id":"c%s","name":"Bash","input":{"command":%s}}]}}\n' "$i" "$(printf '%s' "$cmd" | python3 -c 'import json,sys;print(json.dumps(sys.stdin.read()))')" >> "$f"
    printf '{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"c%s","is_error":true,"content":%s}]}}\n' "$i" "$(printf '%s' "$err" | python3 -c 'import json,sys;print(json.dumps(sys.stdin.read()))')" >> "$f"
  done
}

GITERR="Exit code 128
fatal: options '--name-only', '--name-status', '--check', and '-s' cannot be used together"

echo
echo "SHOULD FIND A PATTERN"
echo

# 1. The canonical case: 3 failures across 2 sessions clears both thresholds.
H=$(new_home 1)
session "$H/.claude/projects/proj/a.jsonl" "git show --name-only -s HEAD" "$GITERR" 2
session "$H/.claude/projects/proj/b.jsonl" "git show --name-only -s HEAD~1" "$GITERR" 1
n=$(HOME="$H" "$BIN" --sessions 10 --json 2>/dev/null | python3 -c 'import json,sys;print(sum(1 for p in json.load(sys.stdin) if p["recurring"]))')
check "recurring pattern found (3 failures / 2 sessions)" "1" "$n"

# 2. It must be attributed to the command that failed, not a sibling.
H=$(new_home 2)
for s in a b c; do
cat > "$H/.claude/projects/proj/$s.jsonl" <<EOF
{"type":"user","message":{"content":"fix flags"}}
{"type":"assistant","message":{"content":[{"type":"tool_use","id":"A","name":"Bash","input":{"command":"git show --name-only -s HEAD"}},{"type":"tool_use","id":"B","name":"Bash","input":{"command":"ls /tmp"}}]}}
{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"A","is_error":true,"content":"Exit code 128\nfatal: options '--name-only', '--name-status', '--check', and '-s' cannot be used together"}]}}
EOF
sleep 0.02; done
HOME="$H" "$BIN" apply --file "$H/work/CLAUDE.md" --sessions 10 --yes >/dev/null 2>&1
key=$(python3 -c "import json;d=json.load(open('$H/work/.anomalift/learned.json'));print(d['rules'][0]['opportunity_keys'][0] if d['rules'] else 'none')" 2>/dev/null)
check "parallel batch: blamed the failing call, not its sibling" "Bash:git show" "$key"

echo
echo "SHOULD FIND NOTHING  (a tool that always finds something is useless)"
echo

# 3. Below the occurrence threshold.
H=$(new_home 3)
session "$H/.claude/projects/proj/a.jsonl" "git show --name-only -s HEAD" "$GITERR" 2
n=$(HOME="$H" "$BIN" --sessions 10 --json 2>/dev/null | python3 -c 'import json,sys;print(sum(1 for p in json.load(sys.stdin) if p["recurring"]))')
check "2 failures is below the 3+ threshold" "0" "$n"

# 4. Enough failures, but all in one session: a bad afternoon, not a habit.
H=$(new_home 4)
session "$H/.claude/projects/proj/a.jsonl" "git show --name-only -s HEAD" "$GITERR" 9
n=$(HOME="$H" "$BIN" --sessions 10 --json 2>/dev/null | python3 -c 'import json,sys;print(sum(1 for p in json.load(sys.stdin) if p["recurring"]))')
check "9 failures in ONE session is not a habit" "0" "$n"

# 5. Failures with no message teach nothing and must not cluster.
H=$(new_home 5)
for s in a b c; do session "$H/.claude/projects/proj/$s.jsonl" "make build" "Exit code 1" 3; sleep 0.02; done
n=$(HOME="$H" "$BIN" --sessions 10 --json 2>/dev/null | python3 -c 'import json,sys;print(len(json.load(sys.stdin)))')
check "bare 'Exit code 1' produces no pattern at all" "0" "$n"

# 6. A user rejecting a tool call is not the agent's mistake.
H=$(new_home 6)
for s in a b c; do session "$H/.claude/projects/proj/$s.jsonl" "rm -rf /tmp/x" "The user doesn't want to proceed with this tool use." 3; sleep 0.02; done
n=$(HOME="$H" "$BIN" --sessions 10 --json 2>/dev/null | python3 -c 'import json,sys;print(len(json.load(sys.stdin)))')
check "user rejection is not learned from" "0" "$n"

# 7. No transcripts at all must not crash.
H=$(new_home 7)
out=$(HOME="$H" "$BIN" --sessions 10 2>&1); rc=$?
check "empty machine exits 0" "0" "$rc"
echo "$out" | grep -q "No agent transcripts found" && ok "empty machine explains itself" || bad "empty machine explains itself" "explanation" "$out"

echo
echo "MUST NOT INVENT ADVICE"
echo

# 8. The same words from a different tool must not inherit git's fix.
H=$(new_home 8)
for s in a b c; do session "$H/.claude/projects/proj/$s.jsonl" "tar -cz -x file" "Exit code 2
tar: options '-c' and '-x' cannot be used together" 1; sleep 0.02; done
rule=$(HOME="$H" "$BIN" --sessions 10 --json 2>/dev/null | python3 -c 'import json,sys
r=[p for p in json.load(sys.stdin) if p["recurring"]]
print(r[0]["rule"] if r and r[0]["rule"] else "none")' 2>/dev/null)
check "tar error does NOT get the git show rule" "none" "$rule"

echo
echo "APPLY / FORGET SAFETY"
echo

# 9. forget restores the file byte for byte, and clears the store with it.
H=$(new_home 9)
for s in a b c; do session "$H/.claude/projects/proj/$s.jsonl" "git show --name-only -s HEAD" "$GITERR" 1; sleep 0.02; done
cp "$H/work/CLAUDE.md" "$H/work/CLAUDE.md.orig"
HOME="$H" "$BIN" apply  --file "$H/work/CLAUDE.md" --sessions 10 --yes >/dev/null 2>&1
applied=$(grep -c 'anomalift:begin' "$H/work/CLAUDE.md")
check "apply wrote exactly one block" "1" "$applied"
HOME="$H" "$BIN" apply  --file "$H/work/CLAUDE.md" --sessions 10 --yes >/dev/null 2>&1
check "applying twice does not stack blocks" "1" "$(grep -c 'anomalift:begin' "$H/work/CLAUDE.md")"
HOME="$H" "$BIN" forget --file "$H/work/CLAUDE.md" --yes >/dev/null 2>&1
diff -q "$H/work/CLAUDE.md" "$H/work/CLAUDE.md.orig" >/dev/null \
  && ok "forget restores the file byte for byte" \
  || bad "forget restores the file byte for byte" "identical" "differs"
left=$(python3 -c "import json;print(len(json.load(open('$H/work/.anomalift/learned.json'))['rules']))" 2>/dev/null || echo 0)
check "forget clears the frozen baselines (no ghost rules)" "0" "$left"

# 10. A file that already ends in a blank line keeps it.
H=$(new_home 10)
for s in a b c; do session "$H/.claude/projects/proj/$s.jsonl" "git show --name-only -s HEAD" "$GITERR" 1; sleep 0.02; done
printf '# Notes\n\nAlways run tests.\n\n' > "$H/work/CLAUDE.md"
cp "$H/work/CLAUDE.md" "$H/work/CLAUDE.md.orig"
HOME="$H" "$BIN" apply  --file "$H/work/CLAUDE.md" --sessions 10 --yes >/dev/null 2>&1
HOME="$H" "$BIN" forget --file "$H/work/CLAUDE.md" --yes >/dev/null 2>&1
diff -q "$H/work/CLAUDE.md" "$H/work/CLAUDE.md.orig" >/dev/null \
  && ok "a trailing blank line survives apply+forget" \
  || bad "a trailing blank line survives apply+forget" "identical" "differs"

echo
echo "EFFECT MUST NOT FLATTER ITSELF"
echo

# 11. Abandoning a command is not a cure.
H=$(new_home 11)
for s in a b c; do session "$H/.claude/projects/proj/$s.jsonl" "git show --name-only -s HEAD" "$GITERR" 1; sleep 0.02; done
HOME="$H" "$BIN" apply --file "$H/work/CLAUDE.md" --sessions 10 --yes >/dev/null 2>&1
sleep 1.1
cat > "$H/.claude/projects/proj/later.jsonl" <<'EOF'
{"type":"user","message":{"content":"unrelated work"}}
{"type":"assistant","message":{"content":[{"type":"tool_use","id":"z","name":"Read","input":{"file_path":"/tmp/f"}}]}}
EOF
v=$(HOME="$H" "$BIN" effect --file "$H/work/CLAUDE.md" --sessions 20 --json 2>/dev/null | python3 -c 'import json,sys
r=json.load(sys.stdin); print(r[0]["verdict"] if r else "none")' 2>/dev/null)
check "never retried => no_evidence, not a cure" "no_evidence" "$v"
t=$(HOME="$H" "$BIN" effect --file "$H/work/CLAUDE.md" --sessions 20 --json 2>/dev/null | python3 -c 'import json,sys
print(sum(x["tokens_saved"] for x in json.load(sys.stdin)))' 2>/dev/null)
check "and claims zero tokens saved" "0" "$t"

echo
echo "MUST NOT DAMAGE A HAND-WRITTEN CLAUDE.md"
echo

# 12. Markers inside a fenced code block are documentation, not a block.
H=$(new_home 12)
for s in a b c; do session "$H/.claude/projects/proj/$s.jsonl" "git show --name-only -s HEAD" "$GITERR" 1; sleep 0.02; done
printf '# Notes\n\nIt writes a block like:\n\n```markdown\n<!-- anomalift:begin 2026-01-01 -->\n- an example rule\n<!-- anomalift:end -->\n```\n\nKeep this.\n' > "$H/work/CLAUDE.md"
HOME="$H" "$BIN" apply --file "$H/work/CLAUDE.md" --sessions 10 --yes >/dev/null 2>&1
grep -q 'an example rule' "$H/work/CLAUDE.md" \
  && ok "a documented example inside a fence survives apply" \
  || bad "a documented example inside a fence survives apply" "preserved" "destroyed"

# 13. A stray marker is refused, not guessed at.
H=$(new_home 13)
for s in a b c; do session "$H/.claude/projects/proj/$s.jsonl" "git show --name-only -s HEAD" "$GITERR" 1; sleep 0.02; done
printf '# Notes\n\n<!-- anomalift:begin 2026-01-01 -->\n\nUser kept this line.\n' > "$H/work/CLAUDE.md"
cp "$H/work/CLAUDE.md" "$H/work/orig"
HOME="$H" "$BIN" apply  --file "$H/work/CLAUDE.md" --sessions 10 --yes >/dev/null 2>&1
HOME="$H" "$BIN" forget --file "$H/work/CLAUDE.md" --yes >/dev/null 2>&1
diff -q "$H/work/CLAUDE.md" "$H/work/orig" >/dev/null \
  && ok "an unbalanced marker is refused, file untouched" \
  || bad "an unbalanced marker is refused, file untouched" "identical" "modified"

echo
echo "THE SHARED REPORT MUST NOT LEAK"
echo

# 14. share writes a report with nothing identifying in it.
H=$(new_home 14)
for s in a b c; do
cat > "$H/.claude/projects/proj/$s.jsonl" <<'EOF'
{"type":"user","message":{"content":"[ACME-99] migrate the billing service for BigClient"}}
{"type":"assistant","message":{"content":[{"type":"tool_use","id":"A","name":"Bash","input":{"command":"cat /Users/someone/acme-secrets/config.ts"}}]}}
{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"A","is_error":true,"content":"Exit code 1\ncat: /Users/someone/acme-secrets/config.ts: No such file or directory"}]}}
EOF
sleep 0.02; done
HOME="$H" "$BIN" apply --file "$H/work/CLAUDE.md" --sessions 10 --yes >/dev/null 2>&1
( cd "$H/work" && HOME="$H" "$BIN" share --file "$H/work/CLAUDE.md" --sessions 20 >/dev/null 2>&1 )
R="$H/work/anomalift-report.md"
if [ -f "$R" ]; then
  leaks=$(grep -oE "ACME-99|BigClient|acme-secrets|/Users/|someone|config\.ts" "$R" | sort -u | tr '\n' ' ')
  [ -z "$leaks" ] && ok "share report leaks nothing identifying" || bad "share report leaks nothing identifying" "(nothing)" "$leaks"
  grep -q "Control patterns" "$R" && ok "share report includes the control arm" || bad "share report includes the control arm" "control section" "missing"
else
  bad "share writes a report" "anomalift-report.md" "not created"
fi

echo
printf "  %d passed, %d failed\n\n" "$PASS" "$FAIL"
[ "$FAIL" -eq 0 ]
