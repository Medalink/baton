#!/usr/bin/env bash
# End-to-end run of the plug-in slice against the real release binary, a real daemon, throwaway git
# repos, and stub plug-ins started under one temp dir. Nothing leaves 127.0.0.1.
#
#   e2e/plugins.sh                  uses target/release/baton, building it when missing
#   BATON_BIN=/path/baton e2e/plugins.sh
#
# Needs bash, git and a real python (3.11+) on PATH as `python`, `python3` or `py`.

set -u
export LC_ALL=C

root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
exe=""
[[ ${OSTYPE:-} == msys* || ${OSTYPE:-} == cygwin* ]] && exe=".exe"
BIN=${BATON_BIN:-$root/target/release/baton$exe}

if [[ ! -x $BIN ]]; then
  (cd "$root" && cargo build --release -q) || { echo "cargo build --release failed" >&2; exit 2; }
fi

wp() { if command -v cygpath >/dev/null 2>&1; then cygpath -m "$1"; else printf '%s' "$1"; fi; }

# `python3` is a Store stub on some Windows machines, so each candidate must run.
PY=""
for candidate in python python3 py; do
  found=$(command -v "$candidate" 2>/dev/null) || continue
  if "$found" -c 'import tomllib' >/dev/null 2>&1; then PY=$(wp "$found"); break; fi
done
[[ -n $PY ]] || { echo "python 3.11+ is required" >&2; exit 2; }

TMP=$(mktemp -d)
export BATON_HOME="$(wp "$TMP")/home"
export GIT_CONFIG_GLOBAL="$TMP/gitconfig"
export GIT_CONFIG_NOSYSTEM=1
export GIT_OPTIONAL_LOCKS=0
git config --file "$GIT_CONFIG_GLOBAL" user.name e2e
git config --file "$GIT_CONFIG_GLOBAL" user.email e2e@localhost
git config --file "$GIT_CONFIG_GLOBAL" init.defaultBranch main
git config --file "$GIT_CONFIG_GLOBAL" core.autocrlf false

pass=0
fail=0
declare -a REPORT=()
declare -A SECTION_MS=()
current=""
section_start=0
STARTED=$EPOCHREALTIME

kill_pid() {
  case ${OSTYPE:-} in
    msys* | cygwin*) taskkill //F //PID "$1" >/dev/null 2>&1 ;;
    *) kill -9 "$1" >/dev/null 2>&1 ;;
  esac
}

daemon_pid() { "$BIN" status 2>/dev/null | sed -n 's/^daemon pid \([0-9]*\).*/\1/p'; }

cleanup() {
  local pid f
  if [[ -d $TMP/home ]]; then
    pid=$(daemon_pid)
    [[ -n $pid ]] && kill_pid "$pid"
  fi
  for f in "$TMP"/pids/*; do
    [[ -f $f ]] && kill_pid "$(cat "$f")"
  done
  sleep 0.5
  rm -rf "$TMP"
}
trap cleanup EXIT
mkdir -p "$TMP/pids" "$TMP/stubs" "$TMP/seen" "$TMP/marks" "$TMP/home/config"

now_us() { local t=$EPOCHREALTIME; echo "${t/./}"; }
section() {
  current=$1
  section_start=$(now_us)
  echo
  echo "== $1"
}
section_end() { SECTION_MS[$current]=$(((($(now_us)) - section_start) / 1000)); }
ok() {
  pass=$((pass + 1))
  echo "  ok   $1"
  REPORT+=("PASS  $current: $1")
}
bad() {
  fail=$((fail + 1))
  echo "  FAIL $1${2:+  ($2)}"
  REPORT+=("FAIL  $current: $1${2:+  ($2)}")
}
check() {
  local name=$1
  shift
  if "$@" >/dev/null 2>&1; then ok "$name"; else bad "$name"; fi
}
eq() { if [[ $2 == "$3" ]]; then ok "$1"; else bad "$1" "expected [$2], got [$3]"; fi; }
contains() { if [[ $2 == *"$3"* ]]; then ok "$1"; else bad "$1" "missing [$3]"; fi; }
lacks() { if [[ $2 != *"$3"* ]]; then ok "$1"; else bad "$1" "found [$3]"; fi; }

sgit() { MSYS_NO_PATHCONV=1 git --git-dir="$SIDE" "$@"; }
LOG="$BATON_HOME/data/daemon.log"

# ---- stub plug-ins ---------------------------------------------------------------------------------
cat >"$TMP/stubs/stub.py" <<'EOF'
import json, os, sys, time

mode = sys.argv[1]


def out(doc):
    sys.stdout.write(json.dumps(doc))
    sys.stdout.flush()


if mode == "hang":  # pid-file
    with open(sys.argv[2], "w") as f:
        f.write(str(os.getpid()))
    time.sleep(120)
    sys.exit(0)

data = sys.stdin.buffer.read()
if mode == "fail":
    sys.exit(3)
elif mode == "garbage":
    sys.stdout.write("this is { not json")
elif mode == "verdict":  # class, seen-file
    with open(sys.argv[3], "wb") as f:
        f.write(data)
    out({"schema": "baton.verdict.v1", "class": sys.argv[2],
         "findings": [{"kind": "stub", "score": 0.5, "evidence": [], "note": "from the stub"}]})
elif mode == "leaky":  # a classifier that echoes a secret it saw in the diff
    out({"schema": "baton.verdict.v1", "class": "check",
         "findings": [{"kind": "leak", "score": 0.5, "evidence": [], "note": "key " + sys.argv[2]}]})
elif mode == "notify":  # name, dir
    with open(os.path.join(sys.argv[3], sys.argv[2] + ".json"), "wb") as f:
        f.write(data)
elif mode == "decide":  # choice | human | outside
    ask = json.loads(data)
    if sys.argv[2] == "human":
        out({"schema": "baton.answer.v1", "ask_human": True, "reason": "needs a person"})
    elif sys.argv[2] == "outside":
        out({"schema": "baton.answer.v1", "choice": "zzz", "confidence": 0.9, "reason": "x"})
    else:
        out({"schema": "baton.answer.v1", "choice": sys.argv[2], "confidence": 0.82,
             "reason": "saw " + ask["question"]})
elif mode == "decide-seen":  # choice, seen-file
    with open(sys.argv[3], "wb") as f:
        f.write(data)
    out({"schema": "baton.answer.v1", "choice": sys.argv[2], "confidence": 0.8, "reason": "x"})
elif mode == "adapt":  # seen-file
    with open(sys.argv[2], "wb") as f:
        f.write(data)
    p = json.loads(data)
    if p.get("skip"):
        sys.stdout.write("null")
    else:
        out({"schema": "baton.turnreq.v1", "cwd": p["workdir"], "thread": p["run"],
             "subject": p["what"], "agent": "my-agent", "model": p.get("llm"),
             "checks": [{"name": "lint", "exit": 0}]})
elif mode == "lens":
    out({"lens": "lens from command for " + json.loads(data)["thread"]})
elif mode == "lensbig":  # 100000 three-byte characters
    out({"lens": "\u20ac" * 100000})
EOF
cat >"$TMP/stubs/endpoint_stub.py" <<'EOF'
# Stands in for an OpenAI-compatible endpoint and a webhook receiver. Bound to 127.0.0.1 only.
import http.server, json, os, sys

seen_dir = sys.argv[1]


class Handler(http.server.BaseHTTPRequestHandler):
    def log_message(self, *a):
        pass

    def reply(self, code, body):
        raw = json.dumps(body).encode()
        self.send_response(code)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(raw)))
        self.end_headers()
        self.wfile.write(raw)

    def do_POST(self):
        body = self.rfile.read(int(self.headers.get("Content-Length", 0)))
        if self.path == "/v1/chat/completions":
            if self.headers.get("Authorization") != "Bearer stub-key":
                return self.reply(401, {"error": "bad key"})
            with open(os.path.join(seen_dir, "openai-request.json"), "wb") as f:
                f.write(body)
            verdict = {"class": "react", "findings": [{"kind": "model_found", "score": 2.0,
                       "evidence": [{"path": "a.txt", "lines": [1, 1]}], "note": "the model says look"}]}
            return self.reply(200, {"choices": [{"message": {"content": "```json\n" + json.dumps(verdict) + "\n```"}}]})
        if self.path == "/hook":
            with open(os.path.join(seen_dir, "webhook-body.json"), "wb") as f:
                f.write(body)
            return self.reply(200, {"ok": True})
        self.reply(404, {})


server = http.server.HTTPServer(("127.0.0.1", 0), Handler)
with open(os.path.join(seen_dir, "port"), "w") as f:
    f.write(str(server.server_address[1]))
with open(os.path.join(seen_dir, "http.pid"), "w") as f:
    f.write(str(os.getpid()))
server.serve_forever()
EOF
STUB="$(wp "$TMP")/stubs/stub.py"
SEEN="$(wp "$TMP")/seen"
MARKS="$(wp "$TMP")/marks"

q() { printf "'%s'" "$1"; } # a TOML literal string
cmd() { # cmd arg... : a TOML array of literal strings
  local out="[" first=1 a
  for a in "$@"; do
    ((first)) || out+=", "
    out+=$(q "$a")
    first=0
  done
  printf '%s]' "$out"
}
stubcmd() { cmd "$PY" "$STUB" "$@"; }
config() { printf '%s\n' "$@" >"$TMP/home/config/config.toml"; }

jget() { # jget file python-expression-over-d
  "$PY" -I -c 'import json,sys; d=json.load(open(sys.argv[1], encoding="utf-8")); print(eval(sys.argv[2]))' "$1" "$2" 2>&1
}

# The HTTP stub and its environment come before the first baton command, which starts the daemon and
# so fixes the environment every plug-in the daemon runs inherits.
"$PY" "$(wp "$TMP")/stubs/endpoint_stub.py" "$SEEN" &
for _ in $(seq 1 100); do [[ -s $TMP/seen/port ]] && break; sleep 0.1; done
PORT=$(cat "$TMP/seen/port" 2>/dev/null)
[[ -n $PORT ]] || { echo "the HTTP stub did not start" >&2; exit 2; }
[[ -f $TMP/seen/http.pid ]] && cp "$TMP/seen/http.pid" "$TMP/pids/http"
export BATON_OPENAI_URL="http://127.0.0.1:$PORT/v1"
export BATON_OPENAI_MODEL="stub-model"
export BATON_OPENAI_KEY="stub-key"
export BATON_WEBHOOK_URL="http://127.0.0.1:$PORT/hook"

# ---- the repository --------------------------------------------------------------------------------
REPO="$TMP/repo"
mkdir -p "$REPO/db/migrate" "$REPO/docs"
git init -q "$REPO"
git -C "$REPO" remote add origin "https://github.com/E2E/plugins.git"
printf 'one\n' >"$REPO/a.txt"
printf 'create table users (id int, legacy_id int);\n' >"$REPO/db/migrate/0001_users.sql"
printf '{"lockfileVersion": 1}\n' >"$REPO/package-lock.json"
printf 'The migrations are one-way.\n' >"$REPO/docs/lens.md"
git -C "$REPO" add -A >/dev/null 2>&1
git -C "$REPO" commit -q -m initial
SIDE="$BATON_HOME/data/projects/github.com+e2e+plugins.git"

turn() { # turn thread message : records a turn through the daemon
  (cd "$REPO" && "$BIN" turn --thread "$1" --message "$2" 2>&1)
}
tip() { sgit rev-parse -q --verify "refs/baton/$1" 2>/dev/null; }
verdict_ref() { sgit rev-parse -q --verify "refs/baton-verdict/$1" 2>/dev/null; }
wait_for() { # wait_for timeout_ms 'shell expression' : polls until the expression succeeds
  local deadline=$(($(now_us) + $1 * 1000))
  while ! eval "$2" >/dev/null 2>&1; do
    (($(now_us) > deadline)) && return 1
    sleep 0.05
  done
}
vd() { (cd "$REPO" && "$BIN" verdict "$@" 2>&1); }
log_has() { grep -qF -- "$1" "$LOG" 2>/dev/null; }
wait_verdict() { wait_for 8000 "test -n \"\$(verdict_ref \"\$(tip $1)\")\""; }

# ---------------------------------------------------------------------------------------------------
section "enroll"
out=$("$BIN" enroll "$(wp "$REPO")" 2>&1)
contains "enroll succeeds" "$out" "enrolled"
section_end

section "default rules, with no plug-in configured"
config ""
printf 'ALTER TABLE users DROP COLUMN legacy_id;\n' >"$REPO/db/migrate/0042_drop_legacy.sql"
turn t-drop "drop the legacy column" >/dev/null
if wait_verdict t-drop; then ok "a verdict was stored for the turn"; else bad "no verdict for the column drop"; fi
v=$(vd t-drop@1 --json)
contains "the planted column drop is react" "$v" '"class": "react"'
contains "the finding names the rule" "$v" "dropped_table_or_column"
contains "the evidence is the migration" "$v" "db/migrate/0042_drop_legacy.sql"
contains "the evidence has the line" "$v" '"lines": ['
human=$(vd t-drop@1)
contains "baton verdict prints the class" "$human" "react"
contains "baton verdict prints file and line" "$human" "db/migrate/0042_drop_legacy.sql:1"

printf '{"lockfileVersion": 2}\n' >"$REPO/package-lock.json"
turn t-drop "bump the lockfile" >/dev/null
wait_for 8000 'test "$(vd t-drop@2 --json)" != ""  && vd t-drop@2 --json | grep -q class'
contains "a lockfile edit is check" "$(vd t-drop@2 --json)" '"class": "check"'

printf 'two\n' >>"$REPO/a.txt"
turn t-drop "touch a note" >/dev/null
wait_for 8000 'vd t-drop@3 --json | grep -q class'
contains "an ordinary edit is quiet" "$(vd t-drop@3 --json)" '"class": "quiet"'

list=$(cd "$REPO" && "$BIN" log --verdicts 2>&1)
contains "log --verdicts shows react" "$list" "react"
contains "log --verdicts shows check" "$list" "check"
contains "log --verdicts shows quiet" "$list" "quiet"
contains "log --verdicts names the thread and turn" "$list" "t-drop@3"
out=$(vd t-nothing@1)
rc=$?
if ((rc != 0)); then ok "a turn that was never recorded has no verdict (exit $rc)"; else bad "verdict for a missing turn exited 0"; fi
section_end

section "the turn document"
rm -f "$TMP/seen/doc.json"
config "[classify]" "mode = \"shadow\"" "command = $(stubcmd verdict check "$SEEN/doc.json")" "timeout = \"5s\"" \
  "[lens]" "file = \"docs/lens.md\""
printf 'let key = "AKIAIOSFODNN7EXAMPLE";\nlet two = 2;\n' >"$REPO/keys.js"
turn t-doc "add the key file" >/dev/null
wait_verdict t-doc
doc="$TMP/seen/doc.json"
eq "schema" "baton.turn.v1" "$(jget "$doc" 'd["schema"]')"
eq "thread" "t-doc" "$(jget "$doc" 'd["thread"]')"
eq "turn number" "1" "$(jget "$doc" 'd["turn"]')"
eq "the first turn is compared with Baton-Base: only the new file is listed" "keys.js:added:2:0" \
  "$(jget "$doc" '",".join(f["path"]+":"+f["status"]+":"+str(f["added"])+":"+str(f["removed"]) for f in d["files"] if f["path"]=="keys.js")')"
eq "files unchanged since Baton-Base are not listed" "0" "$(jget "$doc" 'len([f for f in d["files"] if f["path"]=="docs/lens.md"])')"
contains "the diff has the added line" "$(jget "$doc" 'd["diff"]')" "+let two = 2;"
lacks "the diff is redacted" "$(jget "$doc" 'd["diff"]')" "AKIAIOSFODNN7EXAMPLE"
contains "the diff carries the redaction marker" "$(jget "$doc" 'd["diff"]')" "[baton:redacted:aws-access-key]"
eq "diff_truncated is false for a small diff" "False" "$(jget "$doc" 'd["diff_truncated"]')"
eq "the lens file is passed on" "The migrations are one-way." "$(jget "$doc" 'd["lens"].strip()')"
eq "the base is the repository HEAD" "$(git -C "$REPO" rev-parse HEAD)" "$(jget "$doc" 'd["base"]')"
eq "the first turn has no parent" "None" "$(jget "$doc" 'd["parent"]')"

printf 'let three = 3;\n' >>"$REPO/keys.js"
turn t-doc "second turn" >/dev/null
wait_for 8000 'test "$(jget "$doc" "d[\"turn\"]")" = 2'
eq "the second turn is compared with its parent turn" "keys.js:modified:1:0" \
  "$(jget "$doc" '",".join(f["path"]+":"+f["status"]+":"+str(f["added"])+":"+str(f["removed"]) for f in d["files"])')"
eq "the parent is the first turn" "$(sgit rev-parse "refs/baton/t-doc~1")" "$(jget "$doc" 'd["parent"]')"
eq "the second diff holds only the second change" "+let three = 3;" \
  "$(jget "$doc" '[l for l in d["diff"].splitlines() if l.startswith("+") and not l.startswith("+++")][0]')"

big=$("$PY" -I -c 'print("\n".join("x%07d" % i for i in range(80000)))')
printf '%s\n' "$big" >"$REPO/big.txt"
turn t-big "a large file" >/dev/null
wait_for 8000 'test "$(jget "$doc" "d[\"subject\"]")" = "a large file"'
eq "a diff over 256 KiB is cut" "True" "$(jget "$doc" 'd["diff_truncated"]')"
eq "the cut diff is within the cap" "True" "$(jget "$doc" 'len(d["diff"].encode()) <= 262144')"
contains "the large file is still listed" "$(jget "$doc" '[f["path"] for f in d["files"]]')" "big.txt"
rm -f "$REPO/big.txt"

config "[classify]" "command = $(stubcmd verdict check "$SEEN/doc2.json")" "[lens]" "command = $(stubcmd lens)"
turn t-lens "lens from a command" >/dev/null
wait_verdict t-lens
eq "a lens command supplies the lens" "lens from command for t-lens" "$(jget "$TMP/seen/doc2.json" 'd["lens"]')"
section_end

section "shadow mode stores a verdict and runs no notifier"
rm -f "$MARKS"/*
config "[classify]" "mode = \"shadow\"" "command = $(stubcmd verdict check "$SEEN/shadow.json")" \
  "[[notify]]" "on = [\"check\", \"react\"]" "command = $(stubcmd notify shadow-a "$MARKS")"
printf 'shadow\n' >>"$REPO/a.txt"
turn t-shadow "shadow turn" >/dev/null
if wait_verdict t-shadow; then ok "the verdict was stored"; else bad "no verdict in shadow mode"; fi
sleep 1
check "the classifier ran" test -s "$TMP/seen/shadow.json"
eq "no notifier ran" "" "$(ls "$TMP/marks")"
section_end

section "live mode runs the matching notifiers only"
rm -f "$MARKS"/*
config "[classify]" "mode = \"live\"" "command = $(stubcmd verdict check "$SEEN/live.json")" \
  "[[notify]]" "on = [\"check\"]" "command = $(stubcmd notify on-check "$MARKS")" \
  "[[notify]]" "on = [\"react\"]" "command = $(stubcmd notify on-react "$MARKS")" \
  "[[notify]]" "on = [\"react\", \"check\"]" "command = $(stubcmd notify on-both "$MARKS")"
printf 'live\n' >>"$REPO/a.txt"
turn t-live "live turn" >/dev/null
wait_verdict t-live
wait_for 8000 'test -s "$TMP/marks/on-both.json"'
check "the notifier for check ran" test -s "$TMP/marks/on-check.json"
check "the notifier for react and check ran" test -s "$TMP/marks/on-both.json"
sleep 0.5
check "the notifier for react did not run" test ! -e "$TMP/marks/on-react.json"
eq "the notification carries the verdict class" "check" "$(jget "$TMP/marks/on-check.json" 'd["verdict"]["class"]')"
eq "the notification carries the turn" "t-live" "$(jget "$TMP/marks/on-check.json" 'd["turn"]["thread"]')"
contains "the notification carries the diff" "$(jget "$TMP/marks/on-check.json" 'd["turn"]["diff"]')" "+live"

rm -f "$MARKS"/*
config "[classify]" "mode = \"live\"" "command = $(stubcmd verdict quiet "$SEEN/live2.json")" \
  "[[notify]]" "on = [\"check\"]" "command = $(stubcmd notify on-check "$MARKS")"
printf 'live2\n' >>"$REPO/a.txt"
turn t-live2 "quiet turn" >/dev/null
wait_verdict t-live2
sleep 0.5
eq "a class no notifier names runs none" "" "$(ls "$TMP/marks")"

config "[classify]" "mode = \"off\"" "command = $(stubcmd verdict check "$SEEN/off.json")"
printf 'off\n' >>"$REPO/a.txt"
turn t-off "classification off" >/dev/null
sleep 1
check "mode off runs no classifier" test ! -e "$TMP/seen/off.json"
eq "mode off stores no verdict" "" "$(verdict_ref "$(tip t-off)")"
section_end

section "a classifier that fails leaves the turn recorded and no verdict"
for kind in hang fail garbage; do
  case $kind in
    hang) c=$(stubcmd hang "$(wp "$TMP")/pids/hang-$RANDOM") ; t='timeout = "300ms"' ;;
    fail) c=$(stubcmd fail); t='timeout = "5s"' ;;
    garbage) c=$(stubcmd garbage); t='timeout = "5s"' ;;
  esac
  rm -f "$MARKS"/*
  config "[classify]" "mode = \"live\"" "command = $c" "$t" \
    "[[notify]]" "on = [\"react\", \"check\", \"log\", \"quiet\"]" "command = $(stubcmd notify never "$MARKS")"
  printf '%s\n' "$kind" >>"$REPO/a.txt"
  began=$(now_us)
  out=$(turn "t-$kind" "$kind classifier")
  took=$((($(now_us) - began) / 1000))
  contains "$kind: baton turn succeeds" "$out" "t-$kind@1"
  check "$kind: the turn ref exists" test -n "$(tip "t-$kind")"
  short=$(tip "t-$kind")
  short=${short:0:8}
  if wait_for 8000 "log_has \"no verdict for $short\""; then ok "$kind: the daemon logged why there is no verdict"; else bad "$kind: no log line for $short"; fi
  eq "$kind: no verdict ref" "" "$(verdict_ref "$(tip "t-$kind")")"
  check "$kind: no notifier ran" test -z "$(ls "$TMP/marks")"
  if ((took < 3000)); then ok "$kind: the turn did not wait for the plug-in (${took} ms)"; else bad "$kind: baton turn took ${took} ms"; fi
done
# the hook: exit 0, ref moves, nothing printed, while the classifier hangs
config "[classify]" "command = $(stubcmd hang "$(wp "$TMP")/pids/hang-hook")" "timeout = \"300ms\""
printf '{"session_id":"t-hook","cwd":"%s","hook_event_name":"Stop"}' "$(wp "$REPO")" >"$TMP/hook.json"
printf 'hook\n' >>"$REPO/a.txt"
out=$("$BIN" hook claude <"$TMP/hook.json" 2>"$TMP/err")
rc=$?
eq "the hook exits 0 with a hanging classifier" 0 "$rc"
eq "the hook prints nothing" "" "$out$(cat "$TMP/err")"
if wait_for 5000 'test -n "$(tip t-hook)"'; then ok "the hook's turn was recorded"; else bad "the hook's turn was not recorded"; fi
eq "the hook's turn has no verdict" "" "$(verdict_ref "$(tip t-hook)")"
sleep 1
leftover=0
for f in "$TMP"/pids/hang-*; do
  [[ -f $f ]] || continue
  if tasklist //FI "PID eq $(cat "$f")" 2>/dev/null | grep -q "$(cat "$f")" || kill -0 "$(cat "$f")" 2>/dev/null; then leftover=$((leftover + 1)); fi
done
eq "a timed-out classifier is killed" 0 "$leftover"

config "[classify]" "command = $(stubcmd leaky AKIAIOSFODNN7EXAMPLE)"
printf 'leak\n' >>"$REPO/a.txt"
turn t-leak "classifier echoes a secret" >/dev/null
wait_verdict t-leak
v=$(vd t-leak@1 --json)
lacks "a secret in a command verdict is not stored" "$v" "AKIAIOSFODNN7EXAMPLE"
contains "it is replaced by the redaction marker" "$v" "[baton:redacted:aws-access-key]"
config "[classify]" "mode = \"loud\""
printf 'bad config\n' >>"$REPO/a.txt"
out=$(turn t-badcfg "a typo in the plug-in config")
contains "a bad plug-in config does not stop the turn" "$out" "t-badcfg@1"
eq "and stores no verdict" "" "$(verdict_ref "$(tip t-badcfg)")"
section_end

section "baton ask"
config "[decide]" "command = $(stubcmd decide b)" "timeout = \"5s\""
out=$("$BIN" ask "Keep the old endpoint?" --option a --option b --context "legacy clients" 2>"$TMP/err")
rc=$?
eq "a decider that chooses exits 0" 0 "$rc"
contains "it prints the choice" "$out" "choice: b"
contains "it prints the confidence" "$out" "confidence: 0.82"
contains "it prints the reason" "$out" "reason: saw Keep the old endpoint?"
config ""
out=$("$BIN" ask "Keep it?" --option a --option b 2>"$TMP/err")
rc=$?
eq "no decider exits 2" 2 "$rc"
contains "no decider prints ask a human" "$out" "ask a human"
config "[decide]" "command = $(stubcmd decide human)"
out=$("$BIN" ask "Keep it?" --option a --option b 2>"$TMP/err")
rc=$?
eq "ask_human exits 2" 2 "$rc"
contains "ask_human prints ask a human" "$out" "ask a human"
config "[decide]" "command = $(stubcmd decide outside)"
out=$("$BIN" ask "Keep it?" --option a --option b 2>"$TMP/err")
eq "a choice outside the options exits 2" 2 "$?"
config "[decide]" "command = $(stubcmd fail)"
"$BIN" ask "Keep it?" --option a --option b >/dev/null 2>&1
eq "a failing decider exits 2" 2 "$?"
config "[decide]" "command = $(stubcmd garbage)"
"$BIN" ask "Keep it?" --option a --option b >/dev/null 2>&1
eq "a decider that prints garbage exits 2" 2 "$?"
config "[decide]" "command = $(stubcmd hang "$(wp "$TMP")/pids/hang-decide")" "timeout = \"300ms\""
began=$(now_us)
"$BIN" ask "Keep it?" --option a --option b >/dev/null 2>&1
rc=$?
took=$((($(now_us) - began) / 1000))
eq "a decider that hangs exits 2" 2 "$rc"
if ((took < 3000)); then ok "and gives up at its timeout (${took} ms)"; else bad "the hanging decider took ${took} ms"; fi
config "[decide]" "command = $(stubcmd decide a)"
"$BIN" ask "Keep it?" --option a >/dev/null 2>&1
eq "a single option is a usage error, exit 1" 1 "$?"
"$BIN" ask "Keep it?" >/dev/null 2>&1
eq "a missing option is a usage error, exit 1, not 2" 1 "$?"
"$BIN" ask --help >/dev/null 2>&1
eq "--help exits 0" 0 "$?"
(cd "$TMP" && "$BIN" ask "Outside a repo?" --option a --option b >/dev/null 2>&1)
eq "ask works outside a repository" 0 "$?"
section_end

section "a command adapter maps a custom payload into a turn"
config "[agents.mine]" "command = $(stubcmd adapt "$SEEN/adapter.json")" "timeout = \"5s\""
printf 'adapter\n' >>"$REPO/a.txt"
printf '{"workdir":"%s","run":"t-custom","what":"custom payload turn","llm":"my-model"}' "$(wp "$REPO")" >"$TMP/custom.json"
out=$("$BIN" hook mine <"$TMP/custom.json" 2>&1)
eq "the hook exits 0 and prints nothing" "" "$out"
if wait_for 5000 'test -n "$(tip t-custom)"'; then ok "the custom thread was recorded"; else bad "no turn from the command adapter"; fi
msg=$(sgit cat-file commit refs/baton/t-custom 2>&1)
contains "the subject comes from the adapter" "$msg" "custom payload turn"
contains "the agent comes from the adapter" "$msg" "Baton-Agent: my-agent"
contains "the model comes from the adapter" "$msg" "Baton-Model: my-model"
contains "the checks come from the adapter" "$msg" "Baton-Check: lint=0"
contains "the adapter got the raw payload" "$(cat "$TMP/seen/adapter.json")" '"what":"custom payload turn"'
before=$(sgit for-each-ref refs/baton/ | wc -l | tr -d ' ')
printf '{"skip":true}' >"$TMP/skip.json"
"$BIN" hook mine <"$TMP/skip.json" >/dev/null 2>&1
sleep 0.7
eq "an adapter that prints null records no turn" "$before" "$(sgit for-each-ref refs/baton/ | wc -l | tr -d ' ')"
config "[agents.mine]" "command = $(stubcmd garbage)"
"$BIN" hook mine <"$TMP/custom.json" >/dev/null 2>&1
eq "a broken adapter still exits 0" 0 "$?"
sleep 0.7
eq "and records no turn" "$before" "$(sgit for-each-ref refs/baton/ | wc -l | tr -d ' ')"
section_end

section "built-in adapters"
config ""
printf 'codex\n' >>"$REPO/a.txt"
printf '{"session_id":"t-codex","cwd":"%s","hook_event_name":"Stop","model":"gpt-x","turn_id":"1","transcript_path":null,"stop_hook_active":false,"last_assistant_message":"Renamed the receiver.\\nAnd more."}' "$(wp "$REPO")" >"$TMP/codex.json"
"$BIN" hook codex <"$TMP/codex.json" >/dev/null 2>&1
if wait_for 5000 'test -n "$(tip t-codex)"'; then ok "a Codex Stop payload is recorded"; else bad "no Codex turn"; fi
msg=$(sgit cat-file commit refs/baton/t-codex 2>&1)
contains "codex: agent" "$msg" "Baton-Agent: codex"
contains "codex: model" "$msg" "Baton-Model: gpt-x"
contains "codex: subject is the start of the last message" "$msg" "Renamed the receiver."
printf 'opencode\n' >>"$REPO/a.txt"
printf '{"type":"session.idle","properties":{"sessionID":"t-opencode"},"directory":"%s","model":"anthropic/claude-x"}' "$(wp "$REPO")" >"$TMP/opencode.json"
"$BIN" hook opencode <"$TMP/opencode.json" >/dev/null 2>&1
if wait_for 5000 'test -n "$(tip t-opencode)"'; then ok "an opencode session.idle payload is recorded"; else bad "no opencode turn"; fi
msg=$(sgit cat-file commit refs/baton/t-opencode 2>&1)
contains "opencode: agent" "$msg" "Baton-Agent: opencode"
contains "opencode: model" "$msg" "Baton-Model: anthropic/claude-x"
printf '{"type":"session.created","properties":{"sessionID":"t-notidle"},"directory":"%s"}' "$(wp "$REPO")" >"$TMP/opencode2.json"
"$BIN" hook opencode <"$TMP/opencode2.json" >/dev/null 2>&1
printf 'pi\n' >>"$REPO/a.txt"
printf '{"type":"agent_settled","session_id":"t-pi","cwd":"%s","model":"some-model"}' "$(wp "$REPO")" >"$TMP/pi.json"
"$BIN" hook pi <"$TMP/pi.json" >/dev/null 2>&1
if wait_for 5000 'test -n "$(tip t-pi)"'; then ok "a Pi agent_settled payload is recorded"; else bad "no Pi turn"; fi
msg=$(sgit cat-file commit refs/baton/t-pi 2>&1)
contains "pi: agent" "$msg" "Baton-Agent: pi"
contains "pi: model" "$msg" "Baton-Model: some-model"
sleep 0.5
eq "an event that is not an end of turn records nothing" "" "$(tip t-notidle)"
printf '{"nothing":"useful"}' | "$BIN" hook codex >/dev/null 2>&1
eq "a payload that does not map exits 0" 0 "$?"
section_end

section "a repo config cannot name a command"
rm -f "$TMP/seen/repo-cmd.json"
config "[classify]" "mode = \"shadow\""
mkdir -p "$REPO/.baton"
cat >"$REPO/.baton/config.toml" <<EOF
[classify]
command = $(stubcmd verdict react "$SEEN/repo-cmd.json")
mode = "live"

[[notify]]
on = ["react", "check", "log", "quiet"]
command = $(stubcmd notify from-repo "$MARKS")

[agents.mine]
command = $(stubcmd verdict react "$SEEN/repo-agent.json")
EOF
rm -f "$MARKS"/*
printf 'repo cfg\n' >>"$REPO/a.txt"
sleep 0.3
out=$(turn t-repocfg "the repo names a command")
contains "the turn is recorded" "$out" "t-repocfg@1"
wait_verdict t-repocfg
sleep 0.7
check "the repo's classifier did not run" test ! -e "$TMP/seen/repo-cmd.json"
check "the repo's notifier did not run" test ! -e "$TMP/marks/from-repo.json"
contains "the daemon logged that the repo config was ignored" "$(cat "$LOG")" "which only the user config may; ignored"
check "the user's classifier (the default rules) still produced a verdict" test -n "$(verdict_ref "$(tip t-repocfg)")"
"$BIN" hook mine <"$TMP/custom.json" >/dev/null 2>&1
sleep 0.7
check "the repo's agent command did not run" test ! -e "$TMP/seen/repo-agent.json"
rm -rf "$REPO/.baton"
section_end

section "contrib scripts against a 127.0.0.1 stub"
CONTRIB="$(wp "$root")/contrib"
config "[classify]" "mode = \"live\"" "command = $(cmd "$PY" "$CONTRIB/classify-openai")" "timeout = \"15s\"" \
  "[[notify]]" "on = [\"react\"]" "command = $(cmd "$PY" "$CONTRIB/notify-webhook")" "timeout = \"10s\""
rm -f "$TMP/seen/openai-request.json" "$TMP/seen/webhook-body.json"
printf 'contrib\n' >>"$REPO/a.txt"
turn t-contrib "contrib classifier" >/dev/null
if wait_verdict t-contrib; then ok "classify-openai produced a stored verdict"; else bad "no verdict from classify-openai"; fi
v=$(vd t-contrib@1 --json)
contains "the model's class is used" "$v" '"class": "react"'
contains "the model's finding is kept" "$v" "model_found"
contains "an out-of-range score from the model is clamped to 1" "$v" '"score": 1.0'
req=$(cat "$TMP/seen/openai-request.json" 2>/dev/null)
contains "the request names the configured model" "$req" '"model": "stub-model"'
contains "the request carries the diff" "$req" "+contrib"
if wait_for 8000 'test -s "$TMP/seen/webhook-body.json"'; then ok "notify-webhook posted the notification"; else bad "no webhook POST"; fi
eq "the POST body is the notification" "react t-contrib" "$(jget "$TMP/seen/webhook-body.json" 'd["verdict"]["class"] + " " + d["turn"]["thread"]')"
out=$(echo '{}' | env -u BATON_OPENAI_URL "$PY" "$CONTRIB/classify-openai" 2>&1)
rc=$?
if ((rc != 0)); then ok "classify-openai without its URL exits non-zero (exit $rc)"; else bad "classify-openai without a URL exited 0"; fi
contains "and says which variable" "$out" "BATON_OPENAI_URL"
out=$(echo '{}' | env -u BATON_WEBHOOK_URL "$PY" "$CONTRIB/notify-webhook" 2>&1)
rc=$?
if ((rc != 0)); then ok "notify-webhook without its URL exits non-zero (exit $rc)"; else bad "notify-webhook without a URL exited 0"; fi
section_end

section "the Tailscale example"
example="$root/contrib/examples/tailscale-selfhost.toml"
shape=$("$PY" -I - "$(wp "$example")" <<'EOF'
import sys, tomllib
d = tomllib.load(open(sys.argv[1], "rb"))
allowed = {"machine": {"id"}, "server": {"url", "token", "token_file", "ca_file"}}
bad = [f"{t}.{k}" for t, ks in d.items() for k in ks if k not in allowed.get(t, set())]
assert not bad, bad
assert d["server"]["url"].startswith("wss://")
print("ok")
EOF
)
eq "the example holds only keys the user config accepts" "ok" "$shape"
placeholders=$(grep -Ei '([0-9]{1,3}\.){3}[0-9]{1,3}|ghp_|bearer|AKIA' "$example" | grep -vc '0\.0\.0\.0')
eq "the example holds no address or token" 0 "$placeholders"
section_end

section "a first turn leaves out what the store policy left out"
MAIN_REPO=$REPO
MAIN_SIDE=$SIDE
base_repo() { # base_repo name : a throwaway repo with a remote of its own; points REPO and SIDE at it
  REPO="$TMP/$1"
  mkdir -p "$REPO"
  git init -q "$REPO"
  git -C "$REPO" remote add origin "https://github.com/E2E/$1.git"
  SIDE="$BATON_HOME/data/projects/github.com+e2e+$1.git"
}
files_of() { jget "$1" '",".join(f["path"]+":"+f["status"]+":"+str(f["added"])+":"+str(f["removed"]) for f in d["files"])'; }

config "[snapshot]" "exclude = ['secrets/**']" "max_file_bytes = 1000" \
  "[classify]" "command = $(stubcmd verdict quiet "$SEEN/base-a.json")" "timeout = \"5s\""
base_repo basea
mkdir -p "$REPO/secrets"
printf 'plain-text-of-the-excluded-file\n' >"$REPO/secrets/key.txt"
for _ in $(seq 100); do echo SIZESKIPPED-CONTENT; done >"$REPO/skipped.txt"
printf 'unchanged\n' >"$REPO/kept.txt"
printf 'old\n' >"$REPO/changed.txt"
printf 'going away\n' >"$REPO/gone.txt"
printf '\0binary-v1' >"$REPO/bin.dat"
git -C "$REPO" add -A >/dev/null 2>&1
git -C "$REPO" commit -q -m initial
"$BIN" enroll "$(wp "$REPO")" >/dev/null 2>&1
printf 'edited-excluded-file\n' >"$REPO/secrets/key.txt"
echo SIZESKIPPED-CONTENT >>"$REPO/skipped.txt"
printf 'new\n' >"$REPO/changed.txt"
rm -f "$REPO/gone.txt"
printf '\0binary-v2' >"$REPO/bin.dat"
printf 'fresh\n' >"$REPO/new.txt"
turn t-base "the first turn" >/dev/null
wait_verdict t-base
doc="$TMP/seen/base-a.json"
eq "only the files that changed and that the policy stores are listed" \
  "bin.dat:modified:0:0,changed.txt:modified:1:1,gone.txt:removed:0:1,new.txt:added:1:0" "$(files_of "$doc")"
whole=$(cat "$doc")
lacks "an excluded file's text is not in the document" "$whole" "plain-text-of-the-excluded-file"
lacks "an excluded file's new text is not in the document" "$whole" "edited-excluded-file"
lacks "a size-skipped file's text is not in the document" "$whole" "SIZESKIPPED-CONTENT"
lacks "an excluded path is not listed" "$(jget "$doc" '[f["path"] for f in d["files"]]')" "secrets"
lacks "an unchanged file is not listed" "$(jget "$doc" '[f["path"] for f in d["files"]]')" "kept.txt"
contains "a deleted file still shows its lines" "$(jget "$doc" 'd["diff"]')" "-going away"
contains "a changed binary is reported as differing" "$(jget "$doc" 'd["diff"]')" "Binary files a/bin.dat and b/bin.dat differ"
lacks "a binary's bytes are not in the document (v1)" "$whole" "binary-v1"
lacks "a binary's bytes are not in the document (v2)" "$whole" "binary-v2"

config "[classify]" "command = $(stubcmd verdict quiet "$SEEN/base-b.json")" "timeout = \"5s\""
base_repo baseb
printf 'one\n' >"$REPO/a.txt"
yes 'a line of text that makes the file larger than the diff cap' | head -c 1572864 >"$REPO/big.txt"
yes $'a CRLF line of text that makes the file larger than the diff cap\r' | head -n 40000 >"$REPO/big-crlf.txt"
yes $'a short CRLF file\r' | head -n 3 >"$REPO/small-crlf.txt"
git -C "$REPO" -c core.autocrlf=true add -A >/dev/null 2>&1
git -C "$REPO" commit -q -m initial
"$BIN" enroll "$(wp "$REPO")" >/dev/null 2>&1
printf 'two\n' >"$REPO/a.txt"
printf 'beta\n' >"$REPO/b.txt"
turn t-base "the first turn" >/dev/null
wait_verdict t-base
eq "unchanged files are not listed: over 1 MiB, or checked out with CRLF over a blob with LF" \
  "a.txt:modified:1:1,b.txt:added:1:0" "$(files_of "$TMP/seen/base-b.json")"

config "[classify]" "command = $(stubcmd verdict quiet "$SEEN/excl.json")" "timeout = \"5s\""
base_repo excl
mkdir -p "$REPO/hide"
printf 'hidden-before-the-rule
' >"$REPO/hide/notes.txt"
printf 'one
' >"$REPO/a.txt"
git -C "$REPO" add -A >/dev/null 2>&1
git -C "$REPO" commit -q -m initial
"$BIN" enroll "$(wp "$REPO")" >/dev/null 2>&1
printf 'two
' >"$REPO/a.txt"
turn t-excl "before the rule" >/dev/null
wait_verdict t-excl
config "[snapshot]" "exclude = ['hide/**']" "[classify]" "command = $(stubcmd verdict quiet "$SEEN/excl.json")" "timeout = \"5s\""
printf 'three
' >"$REPO/a.txt"
turn t-excl "after the rule" >/dev/null
wait_for 8000 'test "$(jget "$TMP/seen/excl.json" "d[\"turn\"]")" = 2'
eq "a path excluded halfway through a thread is not reported as deleted" "a.txt:modified:1:1" "$(files_of "$TMP/seen/excl.json")"
lacks "and its text is not in the document" "$(cat "$TMP/seen/excl.json")" "hidden-before-the-rule"

REPO=$MAIN_REPO
SIDE=$MAIN_SIDE
section_end

section "a lens is cut at 64 KiB on a character boundary"
config "[classify]" "command = $(stubcmd verdict check "$SEEN/lens-cmd.json")" "[lens]" "command = $(stubcmd lensbig)"
printf 'lens one\n' >>"$REPO/a.txt"
turn t-lenscmd "a long lens from a command" >/dev/null
wait_verdict t-lenscmd
doc="$TMP/seen/lens-cmd.json"
eq "a command's lens of 100000 three-byte characters is 21845 characters" "21845" "$(jget "$doc" 'len(d["lens"])')"
eq "and 65535 bytes" "65535" "$(jget "$doc" 'len(d["lens"].encode())')"

"$PY" -I -c 'import sys; open(sys.argv[1], "w", encoding="utf-8", newline="").write("\u20ac" * 30000)' "$(wp "$REPO")/docs/long-lens.md"
config "[classify]" "command = $(stubcmd verdict check "$SEEN/lens-file.json")" "[lens]" "file = \"docs/long-lens.md\""
printf 'lens two\n' >>"$REPO/a.txt"
turn t-lensfile "a long lens file" >/dev/null
wait_verdict t-lensfile
doc="$TMP/seen/lens-file.json"
eq "a lens file cut mid-character is not longer than 64 KiB" "True" "$(jget "$doc" 'len(d["lens"].encode()) <= 65536')"
eq "and ends on a whole character" "False" "$(jget "$doc" '"\ufffd" in d["lens"]')"
eq "and keeps every whole character that fits" "21845" "$(jget "$doc" 'len(d["lens"])')"
rm -f "$REPO/docs/long-lens.md"

{ head -c 65530 /dev/zero | tr '\0' 'x'; printf ' AKIAIOSFODNN7EXAMPLE tail
'; } >"$REPO/docs/edge-lens.md"
config "[classify]" "command = $(stubcmd verdict check "$SEEN/lens-edge.json")" "[lens]" "file = \"docs/edge-lens.md\""
printf 'lens three
' >>"$REPO/a.txt"
turn t-lensedge "a secret on the 64 KiB mark" >/dev/null
wait_verdict t-lensedge
lens=$(jget "$TMP/seen/lens-edge.json" 'd["lens"]')
lacks "a secret on the cut of a lens file leaves no prefix of itself" "$lens" "AKIA"
rm -f "$REPO/docs/edge-lens.md"
section_end

section "baton ask passes the thread on"
config "[decide]" "command = $(stubcmd decide-seen remove "$SEEN/ask.json")"
ask_thread() { # ask_thread [VAR=value ...] -- [baton ask args] : what the decider received as "thread"
  rm -f "$TMP/seen/ask.json"
  local envs=()
  while [[ $1 != -- ]]; do envs+=("$1"); shift; done
  shift
  (cd "$REPO" && env -u BATON_THREAD ${envs[@]+"${envs[@]}"} "$BIN" ask "Keep the endpoint?" --option keep --option remove "$@" >/dev/null 2>&1)
  jget "$TMP/seen/ask.json" 'd["thread"]'
}
eq "without a thread the decider is given null" "None" "$(ask_thread --)"
eq "--thread is passed on" "sess-42" "$(ask_thread -- --thread sess-42)"
eq "BATON_THREAD is passed on" "sess-env" "$(ask_thread BATON_THREAD=sess-env --)"
eq "--thread wins over BATON_THREAD" "sess-flag" "$(ask_thread BATON_THREAD=sess-env -- --thread sess-flag)"
eq "an empty BATON_THREAD is no thread" "None" "$(ask_thread BATON_THREAD= --)"
help=$("$BIN" ask --help 2>&1)
contains "the command help names --thread" "$help" "--thread"
contains "the command help names BATON_THREAD" "$help" "BATON_THREAD"
section_end

section "the side repo"
check "git fsck --strict passes with verdict commits in it" sgit fsck --strict --no-dangling
refs=$(sgit for-each-ref --format='%(refname)' refs/baton-verdict/ | wc -l | tr -d ' ')
if ((refs > 10)); then ok "verdict refs exist ($refs)"; else bad "too few verdict refs ($refs)"; fi
lacks "turn refs are separate from verdict refs" "$(sgit for-each-ref --format='%(refname)' refs/baton/)" "baton-verdict"
sample=$(sgit for-each-ref --format='%(refname)' refs/baton-verdict/ | head -1)
eq "a verdict tree holds only verdict.json" "verdict.json" "$(sgit ls-tree --name-only "$sample")"
threads=$(cd "$REPO" && "$BIN" log 2>&1)
lacks "log does not list a verdict ref as a thread" "$threads" "baton-verdict"
eq "baton left no ref and no commit in the user's repository" "1 0" "$(git -C "$REPO" rev-list --all --count) $(git -C "$REPO" for-each-ref | grep -c baton)"
section_end

# ---------------------------------------------------------------------------------------------------
echo
echo "== summary"
for line in "${REPORT[@]}"; do [[ $line == FAIL* ]] && echo "$line"; done
for name in "${!SECTION_MS[@]}"; do printf '  %-62s %6s ms\n' "$name" "${SECTION_MS[$name]}"; done | sort
total=$((($(now_us) - ${STARTED/./}) / 1000))
if ((fail > 0)); then
  echo
  echo "== daemon log (last 60 lines)"
  tail -n 60 "$LOG" 2>/dev/null
fi
echo "checks: $pass passed, $fail failed in ${total} ms"
[[ $fail == 0 ]]
