#!/usr/bin/env bash
# End-to-end run of local turn capture against the real release binary and throwaway git repos.
#
#   e2e/local.sh                    uses target/release/baton, building it when missing
#   BATON_BIN=/path/baton e2e/local.sh
#   BATON_E2E_BIG_REPO=/path/to/a/large/repo   the tree copied for the latency run (git archive HEAD)
#   BATON_E2E_TURNS=100             turns timed in the latency run
#   BATON_E2E_QUIESCENCE_TRIES=50
#
# Everything lives under one temp dir: BATON_HOME, the git config, and every repo. The big repo is only
# read through `git archive`. Needs bash 5 (EPOCHREALTIME), git, tar, sha256sum, awk.

set -u
export LC_ALL=C

root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
exe=""
[[ ${OSTYPE:-} == msys* || ${OSTYPE:-} == cygwin* ]] && exe=".exe"
BIN=${BATON_BIN:-$root/target/release/baton$exe}
BIG_REPO=${BATON_E2E_BIG_REPO:-E:/Dev/Laravel/iris}
TURNS=${BATON_E2E_TURNS:-100}
TRIES=${BATON_E2E_QUIESCENCE_TRIES:-50}

if [[ ! -x $BIN ]]; then
  (cd "$root" && cargo build --release -q) || { echo "cargo build --release failed" >&2; exit 2; }
fi
if [[ -z ${EPOCHREALTIME:-} ]]; then
  echo "bash 5 is required (EPOCHREALTIME)" >&2
  exit 2
fi

# A path the native binary and git understand. Git Bash needs `cygpath`; elsewhere paths already are.
wp() { if command -v cygpath >/dev/null 2>&1; then cygpath -m "$1"; else printf '%s' "$1"; fi; }

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

kill_pid() {
  case ${OSTYPE:-} in
    msys* | cygwin*) taskkill //F //PID "$1" >/dev/null 2>&1 ;;
    *) kill -9 "$1" >/dev/null 2>&1 ;;
  esac
}

daemon_pid() { "$BIN" status 2>/dev/null | sed -n 's/^daemon pid \([0-9]*\).*/\1/p'; }

cleanup() {
  local pid
  if [[ -d $TMP/home ]]; then
    pid=$(daemon_pid)
    [[ -n $pid ]] && kill_pid "$pid"
    sleep 0.5
  fi
  rm -rf "$TMP"
}
trap cleanup EXIT

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

check() { # check "name" command...  : passes when the command succeeds
  local name=$1
  shift
  if "$@" >/dev/null 2>&1; then ok "$name"; else bad "$name"; fi
}

eq() { # eq "name" expected actual
  if [[ $2 == "$3" ]]; then ok "$1"; else bad "$1" "expected [$2], got [$3]"; fi
}

contains() { # contains "name" haystack needle
  if [[ $2 == *"$3"* ]]; then ok "$1"; else bad "$1" "missing [$3]"; fi
}

lacks() {
  if [[ $2 != *"$3"* ]]; then ok "$1"; else bad "$1" "found [$3]"; fi
}

sgit() { git --git-dir="$SIDE" "$@"; }

side_of() { echo "$BATON_HOME/data/projects/github.com+e2e+$1.git"; }

wait_ref_change() { # wait_ref_change file old_sha timeout_ms
  local file=$1 old=$2 deadline=$(($(now_us) + $3 * 1000)) sha
  while :; do
    sha=""
    { read -r sha <"$file"; } 2>/dev/null
    [[ -n $sha && $sha != "$old" ]] && return 0
    (($(now_us) > deadline)) && return 1
  done
}

ref_sha() {
  local sha=""
  { read -r sha <"$1"; } 2>/dev/null
  echo "$sha"
}

payload() { # payload file cwd session [transcript]
  local t=""
  [[ -n ${4:-} ]] && t=",\"transcript_path\":\"$(wp "$4")\""
  printf '{"session_id":"%s","cwd":"%s","hook_event_name":"Stop"%s}' "$3" "$(wp "$2")" "$t" >"$1"
}

mkrepo() { # mkrepo dir name
  local dir=$1
  mkdir -p "$dir/src" "$dir/pkg/gen" "$dir/build"
  git init -q "$dir"
  git -C "$dir" remote add origin "https://github.com/E2E/$2.git"
  printf 'one\n' >"$dir/a.txt"
  printf 'fn main() {}\n' >"$dir/src/main.rs"
  printf 'build/\n*.log\n' >"$dir/.gitignore"
  printf 'gen/\n' >"$dir/pkg/.gitignore"
  printf 'keep\n' >"$dir/pkg/keep.txt"
  printf 'gen\n' >"$dir/pkg/gen/out.txt"
  printf 'obj\n' >"$dir/build/x.o"
  : >"$dir/empty.txt"
  printf 'bin\0ary\n' >"$dir/data.bin"
  git -C "$dir" add -A >/dev/null 2>&1
  git -C "$dir" commit -q -m "initial"
}

git_digest() { (cd "$1/.git" && find . -type f | sort | xargs sha256sum | sha256sum | cut -d' ' -f1); }

percentile() { # percentile file p   (values in microseconds, printed in ms)
  sort -n "$1" | awk -v p="$2" '{a[NR]=$1} END {i=int(p/100*NR+0.999999); if (i<1) i=1; if (i>NR) i=NR; printf "%.1f", a[i]/1000}'
}

# --------------------------------------------------------------------------------------------------
real_dirs=()
case ${OSTYPE:-} in
  msys* | cygwin*) real_dirs=("${LOCALAPPDATA:-}/baton" "${APPDATA:-}/baton") ;;
  darwin*) real_dirs=("$HOME/Library/Application Support/baton") ;;
  *) real_dirs=("${XDG_DATA_HOME:-$HOME/.local/share}/baton" "${XDG_CONFIG_HOME:-$HOME/.config}/baton") ;;
esac
real_before=""
for d in "${real_dirs[@]}"; do
  [[ -n $d ]] && real_before+="$d:$(stat -c %Y "$d" 2>/dev/null || stat -f %m "$d" 2>/dev/null || echo none);"
done

REPO="$TMP/repo"
mkrepo "$REPO" widgets
SECRET_KEY="AKIAIOSFODNN7EXAMPLE"
SECRET_LITERAL="hunter2-literal-secret-value"
printf '{"db":{"password":"%s","short":"abc"}}' "$SECRET_LITERAL" >"$TMP/values.json"
mkdir -p "$TMP/home/config"
printf '[redact]\nvalue_files = ["%s"]\n' "$(wp "$TMP")/values.json" >"$TMP/home/config/config.toml"
HEAD_SHA=$(git -C "$REPO" rev-parse HEAD)
DIGEST_BEFORE=$(git_digest "$REPO")
payload "$TMP/payload-widgets.json" "$REPO" sess-1 "$TMP/transcript.jsonl"
cat >"$TMP/transcript.jsonl" <<'EOF'
{"type":"user","message":{"role":"user","content":"tighten the parser"}}
{"type":"assistant","message":{"role":"assistant","model":"claude-opus-4-1","content":[{"type":"text","text":"done"}]}}
{"type":"user","message":{"role":"user","content":[{"type":"tool_result","content":"ok"}]}}
EOF

# --------------------------------------------------------------------------------------------------
section "hook with the daemon absent"
out=$("$BIN" hook claude <"$TMP/payload-widgets.json" 2>"$TMP/err")
eq "exit code is 0" 0 $?
eq "stdout is empty" "" "$out"
eq "stderr is empty" "" "$(cat "$TMP/err")"
check "the hook did not start a daemon (no data dir was created)" test ! -e "$TMP/home/data"
for variant in "no agent" "unknown agent" "garbage stdin" "oversized stdin"; do
  case $variant in
    "no agent") out=$("$BIN" hook </dev/null 2>"$TMP/err") ;;
    "unknown agent") out=$("$BIN" hook nonesuch <"$TMP/payload-widgets.json" 2>"$TMP/err") ;;
    "garbage stdin") out=$(printf '\xff\xfe not json' | "$BIN" hook claude 2>"$TMP/err") ;;
    "oversized stdin") out=$(head -c 3000000 /dev/zero | "$BIN" hook claude 2>"$TMP/err") ;;
  esac
  rc=$?
  if [[ $rc == 0 && -z $out && -z $(cat "$TMP/err") ]]; then ok "$variant: exit 0, no output"; else bad "$variant" "rc=$rc"; fi
done
section_end

section "daemon starts on demand without holding a shell substitution"
( out=$("$BIN" status 2>&1); echo "$out" >"$TMP/status.out"; touch "$TMP/status.done" ) &
for _ in $(seq 1 100); do [[ -e $TMP/status.done ]] && break; sleep 0.1; done
if [[ -e $TMP/status.done ]]; then ok "\$(baton status) returned while the daemon kept running"; else bad "\$(baton status) hung"; fi
contains "status reports the daemon" "$(cat "$TMP/status.out" 2>/dev/null)" "daemon pid"
section_end

section "enroll and baseline"
out=$("$BIN" enroll "$(wp "$REPO")" 2>&1)
contains "enroll succeeds" "$out" "enrolled"
contains "baseline counts the stored files" "$out" "(7 files)"
out=$("$BIN" enroll "$(wp "$REPO")" 2>&1)
contains "enrolling twice is harmless" "$out" "already enrolled"
SIDE=$(side_of widgets)
check "the side repo exists" test -f "$SIDE/HEAD"
section_end

section "hook with the daemon present"
out=$("$BIN" hook claude <"$TMP/payload-widgets.json" 2>"$TMP/err")
eq "exit code is 0" 0 $?
eq "stdout is empty" "" "$out"
eq "stderr is empty" "" "$(cat "$TMP/err")"
if wait_ref_change "$SIDE/refs/baton/sess-1" "" 5000; then ok "refs/baton/sess-1 appeared"; else bad "refs/baton/sess-1 never appeared"; fi
section_end

section "turn commit and trailers"
msg=$(sgit cat-file commit refs/baton/sess-1 2>&1)
contains "subject is the latest user prompt" "$msg" "tighten the parser"
contains "Baton-Thread" "$msg" "Baton-Thread: sess-1"
contains "Baton-Turn" "$msg" "Baton-Turn: 1"
contains "Baton-Agent" "$msg" "Baton-Agent: claude-code"
contains "Baton-Model from the transcript" "$msg" "Baton-Model: claude-opus-4-1"
contains "Baton-Machine" "$msg" "Baton-Machine: "
contains "Baton-Base is the user's HEAD" "$msg" "Baton-Base: $HEAD_SHA"
lacks "the first turn has no parent" "$msg" "parent "
printf 'two\n' >>"$REPO/a.txt"
out=$(cd "$REPO" && "$BIN" turn --thread sess-1 --message "second turn" --agent claude-code --check tests=0 --check lint=2 2>&1)
contains "baton turn prints thread@seq" "$out" "sess-1@2"
msg=$(sgit cat-file commit refs/baton/sess-1 2>&1)
contains "Baton-Turn 2" "$msg" "Baton-Turn: 2"
contains "Baton-Check tests" "$msg" "Baton-Check: tests=0"
contains "Baton-Check lint" "$msg" "Baton-Check: lint=2"
contains "the second turn has a parent" "$msg" "parent "
eq "the thread has two commits" 2 "$(sgit rev-list --count refs/baton/sess-1)"
log=$(cd "$REPO" && "$BIN" log sess-1 2>&1)
contains "log lists the changed path" "$log" "M a.txt"
section_end

section "side repo integrity"
check "git fsck --strict passes" sgit fsck --strict --no-dangling
section_end

section "empty file round trip"
check "the empty blob is stored" sgit cat-file -e e69de29bb2d1d6434b8b29ae775ad8c2e48c5391
contains "empty.txt is in the tree" "$(sgit ls-tree -r refs/baton/sess-1)" "e69de29bb2d1d6434b8b29ae775ad8c2e48c5391	empty.txt"
rm "$REPO/empty.txt"
(cd "$REPO" && "$BIN" turn --thread sess-1 --message "drop the empty file" >/dev/null 2>&1)
lacks "a deleted file leaves the tree" "$(sgit ls-tree -r refs/baton/sess-1)" "empty.txt"
(cd "$REPO" && "$BIN" rewind sess-1@2 --apply >/dev/null 2>&1)
if [[ -f $REPO/empty.txt && ! -s $REPO/empty.txt ]]; then ok "rewind restored an empty file"; else bad "rewind did not restore empty.txt"; fi
(cd "$REPO" && "$BIN" turn --thread sess-1 --message "empty file is back" >/dev/null 2>&1)
section_end

section "ignore rules"
tree=$(sgit ls-tree -r --name-only refs/baton/sess-1)
contains "a tracked file is stored" "$tree" "pkg/keep.txt"
lacks "a path ignored by a nested .gitignore is not stored" "$tree" "pkg/gen/out.txt"
lacks "a path ignored by the root .gitignore is not stored" "$tree" "build/x.o"
lacks ".git is never stored" "$tree" ".git/"
printf 'later\n' >"$REPO/pkg/gen/later.txt"
printf 'later\n' >"$REPO/build/later.o"
printf 'noise\n' >"$REPO/debug.log"
printf 'visible\n' >"$REPO/pkg/visible.txt"
(cd "$REPO" && "$BIN" turn --thread sess-1 --message "after ignored writes" >/dev/null 2>&1)
tree=$(sgit ls-tree -r --name-only refs/baton/sess-1)
contains "a new visible file is stored" "$tree" "pkg/visible.txt"
lacks "a file written later under a nested-ignored dir is not stored" "$tree" "pkg/gen/later.txt"
lacks "a file written later under an ignored dir is not stored" "$tree" "build/later.o"
lacks "a *.log file is not stored" "$tree" "debug.log"
section_end

section "redaction"
printf 'AWS_KEY=%s\nDB=%s\nshort=abc\n' "$SECRET_KEY" "$SECRET_LITERAL" >"$REPO/notes.env"
(cd "$REPO" && "$BIN" turn --thread sess-1 --message "added key $SECRET_KEY and $SECRET_LITERAL" --model "claude-$SECRET_KEY" >/dev/null 2>&1)
blob=$(sgit cat-file -p refs/baton/sess-1:notes.env)
lacks "the AWS key is not in the blob" "$blob" "$SECRET_KEY"
lacks "the value-file literal is not in the blob" "$blob" "$SECRET_LITERAL"
contains "the blob carries a marker for the built-in rule" "$blob" "[baton:redacted:aws-access-key]"
contains "values under 8 characters are not literals" "$blob" "short=abc"
msg=$(sgit cat-file commit refs/baton/sess-1)
lacks "the AWS key is not in the subject or trailers" "$msg" "$SECRET_KEY"
lacks "the literal is not in the subject" "$msg" "$SECRET_LITERAL"
contains "the model trailer is redacted, not dropped" "$msg" "Baton-Model: claude-[baton:redacted:aws-access-key]"
sgit cat-file --batch-all-objects --batch >"$TMP/objects.dump"
check "no object in the side repo holds the AWS key" bash -c '! grep -aq -e "$0" "$1"' "$SECRET_KEY" "$TMP/objects.dump"
check "no object in the side repo holds the literal" bash -c '! grep -aq -e "$0" "$1"' "$SECRET_LITERAL" "$TMP/objects.dump"
rm -f "$TMP/objects.dump"
leaks=$(grep -rl -e "$SECRET_KEY" -e "$SECRET_LITERAL" "$BATON_HOME" 2>/dev/null | grep -v config.toml || true)
eq "no file under BATON_HOME other than the config holds a secret" "" "$leaks"
(cd "$REPO" && "$BIN" turn --thread "sess-$SECRET_KEY" --message x >"$TMP/leak.out" 2>&1)
contains "a thread id holding a secret is refused" "$(cat "$TMP/leak.out")" "secret"
section_end

section "rewind --apply"
printf 'v1\n' >"$REPO/a.txt"
(cd "$REPO" && "$BIN" turn --thread rw --message "v1" >/dev/null 2>&1)
printf 'v2\n' >"$REPO/a.txt"
printf 'extra\n' >"$REPO/extra.txt"
(cd "$REPO" && "$BIN" turn --thread rw --message "v2" >/dev/null 2>&1)
out=$(cd "$REPO" && "$BIN" rewind rw@1 2>&1)
contains "the dry run names the file" "$out" "restore a.txt"
eq "the dry run changes nothing" "v2" "$(cat "$REPO/a.txt")"
(cd "$REPO" && "$BIN" rewind rw@1 --apply >/dev/null 2>&1)
eq "--apply restores the file" "v1" "$(cat "$REPO/a.txt")"
check "a file the turn does not have is not deleted" test -f "$REPO/extra.txt"
printf 'uncaptured\n' >"$REPO/a.txt"
out=$(cd "$REPO" && "$BIN" rewind rw@1 --apply 2>&1)
rc=$?
eq "an edit no snapshot has seen blocks the rewind" "uncaptured" "$(cat "$REPO/a.txt")"
if [[ $rc != 0 ]]; then ok "and the command fails"; else bad "and the command fails" "rc=0"; fi
contains "the refusal says why" "$out" "take a turn first"
(cd "$REPO" && "$BIN" turn --thread rw --message "captured" >/dev/null 2>&1)
section_end

section "the user's repo is untouched"
DIGEST_AFTER=$(git_digest "$REPO")
eq "every byte under .git is identical (index, HEAD, refs, objects, config)" "$DIGEST_BEFORE" "$DIGEST_AFTER"
eq "HEAD is the same commit" "$HEAD_SHA" "$(git -C "$REPO" rev-parse HEAD)"
eq "no baton ref in the user's repo" "" "$(git -C "$REPO" for-each-ref refs/baton)"
section_end

section "write immediately before the hook"
payload "$TMP/payload-q.json" "$REPO" quiesce
misses=0
for i in $(seq 1 "$TRIES"); do
  old=$(ref_sha "$SIDE/refs/baton/quiesce")
  printf 'value %s\n' "$i" >"$REPO/q.txt"
  "$BIN" hook claude <"$TMP/payload-q.json"
  if ! wait_ref_change "$SIDE/refs/baton/quiesce" "$old" 5000; then misses=$((misses + 1)); continue; fi
  got=$(sgit cat-file -p refs/baton/quiesce:q.txt 2>/dev/null)
  [[ $got == "value $i" ]] || misses=$((misses + 1))
done
echo "  quiescence misses: $misses of $TRIES"
eq "every write made just before the hook is in its turn" 0 "$misses"
QUIESCENCE_MISSES=$misses
section_end

section "kill the daemon mid-turn"
kill_rounds=0
moved=0
unmoved=0
for delay in 0 0.05 0.1 0.2 0.4; do
  kill_rounds=$((kill_rounds + 1))
  thread="kill$kill_rounds"
  payload "$TMP/payload-k.json" "$REPO" "$thread"
  head -c 60000000 /dev/urandom >"$REPO/big.bin"
  want=$(sha256sum "$REPO/big.bin" | cut -d' ' -f1)
  "$BIN" hook claude <"$TMP/payload-k.json"
  [[ $delay != 0 ]] && sleep "$delay"
  kill_pid "$(daemon_pid)"
  sleep 0.3
  if [[ -n $(ref_sha "$SIDE/refs/baton/$thread") ]]; then moved=$((moved + 1)); else unmoved=$((unmoved + 1)); fi
  check "round $kill_rounds (delay $delay s): the side repo passes fsck right after the kill" sgit fsck --strict --no-dangling
  out=$(cd "$REPO" && "$BIN" turn --thread "$thread" --message "after restart" 2>&1)
  contains "round $kill_rounds: the next command restarts the daemon and records a turn" "$out" "$thread@"
  got=$(sgit cat-file -p "refs/baton/$thread:big.bin" | sha256sum | cut -d' ' -f1)
  eq "round $kill_rounds: the restarted daemon stored the whole file" "$want" "$got"
  eq "round $kill_rounds: no stale ref lock remains" "" "$(find "$SIDE/refs" -name '*.lock' 2>/dev/null)"
done
echo "  kill landed after the ref moved in $moved rounds and before it in $unmoved"
KILL_MOVED=$moved
KILL_UNMOVED=$unmoved
check "the side repo passes fsck at the end" sgit fsck --strict --no-dangling
rm -f "$REPO/big.bin"
section_end

section "latency on a large tree"
LAT_SUMMARY="skipped"
FLOOR_SUMMARY="skipped"
if [[ -d $BIG_REPO/.git ]]; then
  BIG="$TMP/big"
  mkdir -p "$BIG"
  git -C "$BIG_REPO" archive HEAD | tar -x -C "$BIG"
  git init -q "$BIG"
  git -C "$BIG" remote add origin "https://github.com/E2E/bigtree.git"
  files=$(find "$BIG" -type f -not -path '*/.git/*' | wc -l | tr -d ' ')
  echo "  copied $files files"
  began=$(now_us)
  out=$("$BIN" enroll "$(wp "$BIG")" 2>&1)
  enroll_ms=$((($(now_us) - began) / 1000))
  contains "the large tree enrolls" "$out" "enrolled"
  echo "  baseline: $out in ${enroll_ms} ms"
  SIDE=$(side_of bigtree)
  mapfile -t edit < <(cd "$BIG" && find . -name '*.php' -path './app/*' | head -3)
  payload "$TMP/payload-big.json" "$BIG" lat
  : >"$TMP/lat.us"
  : >"$TMP/hook.us"
  : >"$TMP/daemon.us"
  for i in $(seq 1 "$TURNS"); do
    for f in "${edit[@]}"; do printf 'line %s\n' "$i" >>"$BIG/$f"; done
    old=$(ref_sha "$SIDE/refs/baton/lat")
    t0=$(now_us)
    "$BIN" hook claude <"$TMP/payload-big.json"
    t1=$(now_us)
    if wait_ref_change "$SIDE/refs/baton/lat" "$old" 10000; then
      t2=$(now_us)
      echo $((t2 - t0)) >>"$TMP/lat.us"
      echo $((t1 - t0)) >>"$TMP/hook.us"
      echo $((t2 - t1)) >>"$TMP/daemon.us"
    else
      bad "turn $i never moved the ref"
    fi
    sleep 0.15
  done
  n=$(wc -l <"$TMP/lat.us" | tr -d ' ')
  eq "all $TURNS timed turns moved the ref" "$TURNS" "$n"
  LAT_SUMMARY="p50 $(percentile "$TMP/lat.us" 50) ms, p95 $(percentile "$TMP/lat.us" 95) ms, max $(percentile "$TMP/lat.us" 100) ms over $n turns on $files files"
  echo "  hook call to ref moved: $LAT_SUMMARY"
  echo "    of which the hook process (spawn to exit, from bash): p50 $(percentile "$TMP/hook.us" 50) ms, p95 $(percentile "$TMP/hook.us" 95) ms"
  echo "    and hook exit to ref moved: p50 $(percentile "$TMP/daemon.us" 50) ms, p95 $(percentile "$TMP/daemon.us" 95) ms, max $(percentile "$TMP/daemon.us" 100) ms"
  LAT_SUMMARY+="; hook exit to ref moved p50 $(percentile "$TMP/daemon.us" 50) ms, p95 $(percentile "$TMP/daemon.us" 95) ms"
  for field in sync commit ref save total; do
    med=$(grep -F ' turn lat@' "$BATON_HOME/data/daemon.log" | sed -n "s/.* $field=\([0-9.]*\)ms.*/\1/p" | sort -n | awk '{a[NR]=$1} END {print a[int((NR+1)/2)]}')
    echo "    daemon $field median ${med} ms"
  done
  : >"$TMP/floor.us"
  payload "$TMP/payload-none.json" "$TMP" nobody
  for _ in $(seq 1 30); do
    t0=$(now_us)
    "$BIN" hook claude <"$TMP/payload-none.json"
    echo $(($(now_us) - t0)) >>"$TMP/floor.us"
  done
  FLOOR_SUMMARY="p50 $(percentile "$TMP/floor.us" 50) ms, p95 $(percentile "$TMP/floor.us" 95) ms over 30 hook processes"
  echo "  hook process, frame to a running daemon: $FLOOR_SUMMARY"
  check "the large side repo passes fsck" sgit fsck --strict --no-dangling
else
  echo "  skipped: $BIG_REPO is not a git repo (set BATON_E2E_BIG_REPO)"
fi
section_end

section "first ref update per new side repo and thread"
log="$BATON_HOME/data/daemon.log"
first_refs=$(grep -E ' turn [^ ]+@1 ' "$log" | sed -n 's/.*ref=\([0-9.]*\)ms.*/\1/p' | sort -n | tail -1)
slow=$(sed -n 's/.* turn [^ ]*@[0-9]* .*ref=\([0-9.]*\)ms.*/\1/p' "$log" | awk '$1 > 1000' | wc -l | tr -d ' ')
echo "  slowest first ref update: ${first_refs:-n/a} ms; ref updates over 1000 ms: $slow"
eq "no ref update stalled for over a second" 0 "$slow"
STALL="slowest first ref update ${first_refs:-n/a} ms; $slow ref updates over 1 s"
section_end

section "the real data and config dirs are untouched"
real_after=""
for d in "${real_dirs[@]}"; do
  [[ -n $d ]] && real_after+="$d:$(stat -c %Y "$d" 2>/dev/null || stat -f %m "$d" 2>/dev/null || echo none);"
done
eq "no baton directory outside BATON_HOME was created or modified" "$real_before" "$real_after"
eq "the user's .git is still byte-identical after every scenario" "$DIGEST_BEFORE" "$(git_digest "$REPO")"
section_end

# --------------------------------------------------------------------------------------------------
echo
echo "== summary"
for line in "${REPORT[@]}"; do [[ $line == FAIL* ]] && echo "$line"; done
for name in "${!SECTION_MS[@]}"; do printf '  %-62s %6s ms\n' "$name" "${SECTION_MS[$name]}"; done | sort
echo "checks: $pass passed, $fail failed"
echo "quiescence misses: $QUIESCENCE_MISSES of $TRIES"
echo "kill rounds: $kill_rounds (ref moved before the kill in $KILL_MOVED, not yet in $KILL_UNMOVED)"
echo "latency: $LAT_SUMMARY"
echo "hook floor: $FLOOR_SUMMARY"
echo "ref stall: $STALL"
[[ $fail == 0 ]]
