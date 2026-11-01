#!/usr/bin/env bash
# End-to-end run of `baton report` against the real release binary and throwaway git repos.
#
#   e2e/report.sh                   uses target/release/baton, building it when missing
#   BATON_BIN=/path/baton e2e/report.sh
#   BATON_E2E_THREADS=300           threads in the scale run
#   BATON_E2E_TURNS=10              turns per thread in the scale run
#   BATON_E2E_FILES=2000            files in the scale run's repository
#
# One scenario plants one case of every pattern next to threads that must stay out of the report: real turns
# from `baton turn` for thrashing and rework, turn commits written with git plumbing for the cases that need
# old timestamps (abandonment, the 48-hour edge). Then a project with thousands of turns, built with
# `git fast-import`, times a cold run against a cached one.
#
# Everything lives under one temp dir: BATON_HOME, the git config and every repo. Needs bash 4, git, tar, awk.

set -u
export LC_ALL=C

root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
exe=""
[[ ${OSTYPE:-} == msys* || ${OSTYPE:-} == cygwin* ]] && exe=".exe"
BIN=${BATON_BIN:-${CARGO_TARGET_DIR:-$root/target}/release/baton$exe}
THREADS=${BATON_E2E_THREADS:-300}
TURNS=${BATON_E2E_TURNS:-10}
FILES=${BATON_E2E_FILES:-2000}

if [[ ! -x $BIN ]]; then
  (cd "$root" && cargo build --release -q) || { echo "cargo build --release failed" >&2; exit 2; }
fi

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
current=""

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
  [[ -n ${BATON_E2E_KEEP:-} ]] || rm -rf "$TMP"
}
trap cleanup EXIT

section() {
  current=$1
  echo
  echo "== $1"
}

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

eq() { if [[ $2 == "$3" ]]; then ok "$1"; else bad "$1" "expected [$2], got [$3]"; fi; }
contains() { if [[ $2 == *"$3"* ]]; then ok "$1"; else bad "$1" "missing [$3]"; fi; }
lacks() { if [[ $2 != *"$3"* ]]; then ok "$1"; else bad "$1" "found [$3]"; fi; }

now_ms() { local t=$EPOCHREALTIME; t=${t/./}; echo $((t / 1000)); }

# `jq` is not assumed: the JSON is one line, so a pattern is a substring of it.
json() { "$BIN" report "$@" --json 2>&1; }
text() { "$BIN" report "$@" 2>&1; }

# edit SCRIPT FILE : sed -i, which BSD sed spells differently.
edit() { sed "$1" "$2" >"$2.t" && mv "$2.t" "$2"; }

git_digest() { (cd "$1/.git" && find . -type f | sort | xargs sha256sum | sha256sum | cut -d' ' -f1); }

turn_in() { # turn_in dir thread message [baton turn args...]
  local dir=$1 thread=$2 msg=$3
  shift 3
  (cd "$dir" && "$BIN" turn --thread "$thread" --message "$msg" "$@" 2>&1)
}

side_of() { echo "$BATON_HOME/data/projects/github.com+e2e+$1.git"; }

if [[ -z ${EPOCHREALTIME:-} ]]; then
  echo "bash 5 is required (EPOCHREALTIME)" >&2
  exit 2
fi

NOW=$(date +%s)
DAY=86400
iso() { date -u -d "@$1" '+%Y-%m-%dT%H:%M:%SZ' 2>/dev/null || date -u -r "$1" '+%Y-%m-%dT%H:%M:%SZ'; }

# ---------------------------------------------------------------------------------------------- scenario
section "the scenario: threads that plant each pattern, and threads that must stay out"
R="$TMP/rep"
git init -q "$R"
git -C "$R" remote add origin "https://github.com/E2E/rep.git"
mkdir -p "$R/src"
seq 1 12 | sed 's/^/base line /' >"$R/src/hot.rs"
seq 1 6 | sed 's/^/lib line /' >"$R/src/lib.rs"
printf 'readme\n' >"$R/README.md"
at() { GIT_AUTHOR_DATE="$(iso "$1")" GIT_COMMITTER_DATE="$(iso "$1")" "${@:2}"; }
git -C "$R" add -A >/dev/null 2>&1
at $((NOW - 10 * DAY)) git -C "$R" commit -q -m "base"
BASE=$(git -C "$R" rev-parse HEAD)

# A change shipped four days ago, for the 48-hour edge.
printf 'late one\nlate two\nlate three\n' >"$R/src/late.rs"
git -C "$R" add -A >/dev/null 2>&1
at $((NOW - 4 * DAY)) git -C "$R" commit -q -m "late"
LATE=$(git -C "$R" rev-parse HEAD)

"$BIN" enroll --repo "$(wp "$R")" >/dev/null 2>&1
SIDE=$(side_of rep)

wt() { git -C "$R" worktree add -q -b "br-$1" "$TMP/wt-$1" >/dev/null 2>&1; }
for n in thrash1 flaky steady greenred author clean1 clean2 recent-wip; do wt "$n"; done

# thrash1: one file changed in four turns; turn 3 removes a line turn 1 added, and turn 2 one more.
T="$TMP/wt-thrash1"
cat >>"$T/src/hot.rs" <<'EOF'
alpha
beta
gamma
EOF
turn_in "$T" thrash1 "add three lines" --agent claude-code --model m-alpha >/dev/null
edit 's/^beta$/beta two/' "$T/src/hot.rs"
turn_in "$T" thrash1 "reword beta" --agent claude-code --model m-alpha >/dev/null
edit '/^gamma$/d' "$T/src/hot.rs"
turn_in "$T" thrash1 "drop gamma" --agent claude-code --model m-alpha >/dev/null
printf 'delta\n' >>"$T/src/hot.rs"
turn_in "$T" thrash1 "add delta" --agent claude-code --model m-alpha >/dev/null

# flaky: one check red, green, red, on distinct files so the file rule stays quiet.
F="$TMP/wt-flaky"
printf 'f1\n' >"$F/src/f1.rs"
turn_in "$F" flaky "first" --agent codex --model m-beta --check tests=1 >/dev/null
printf 'f2\n' >"$F/src/f2.rs"
turn_in "$F" flaky "second" --agent codex --model m-beta --check tests=0 >/dev/null
printf 'f3\n' >"$F/src/f3.rs"
turn_in "$F" flaky "third" --agent codex --model m-beta --check tests=1 >/dev/null

# steady: red, green, green. greenred: green, red. Neither is red-green-red.
S="$TMP/wt-steady"
printf 's1\n' >"$S/src/s1.rs"
turn_in "$S" steady "first" --agent codex --model m-beta --check tests=1 >/dev/null
printf 's2\n' >"$S/src/s2.rs"
turn_in "$S" steady "second" --agent codex --model m-beta --check tests=0 >/dev/null
printf 's3\n' >"$S/src/s3.rs"
turn_in "$S" steady "third" --agent codex --model m-beta --check tests=0 >/dev/null
G="$TMP/wt-greenred"
printf 'g1\n' >"$G/src/g1.rs"
turn_in "$G" greenred "first" --agent codex --model m-beta --check tests=0 >/dev/null
printf 'g2\n' >"$G/src/g2.rs"
turn_in "$G" greenred "second" --agent codex --model m-beta --check tests=1 >/dev/null

# author: a thread whose work the user ships (one new file, one added line in hot.rs).
A="$TMP/wt-author"
printf 'ship one\nship two\nship three\n' >"$A/src/ship.rs"
printf 'shipped hot line\n' >>"$A/src/hot.rs"
turn_in "$A" author "work to ship" --agent claude-code --model m-alpha >/dev/null
git -C "$A" add -A >/dev/null 2>&1
git -C "$A" commit -q -m "ship the work"
git -C "$R" merge -q --ff-only br-author >/dev/null 2>&1
SHIPPED=$(git -C "$R" rev-parse HEAD)

# reworker: starts after the ship, and changes what author shipped.
wt reworker
W="$TMP/wt-reworker"
edit 's/^ship two$/ship two, reworked/' "$W/src/ship.rs"
edit '/^shipped hot line$/d' "$W/src/hot.rs"
turn_in "$W" reworker "fix what shipped" --agent codex --model m-beta >/dev/null

# clean1 and clean2: at most two turns on a file, never shipped, recent.
C1="$TMP/wt-clean1"
printf 'c1\n' >"$C1/src/c1.rs"
turn_in "$C1" clean1 "first" --agent claude-code --model m-alpha --check tests=0 >/dev/null
printf 'c1 more\n' >>"$C1/src/c1.rs"
turn_in "$C1" clean1 "second" --agent claude-code --model m-alpha --check tests=0 >/dev/null
C2="$TMP/wt-clean2"
printf 'c2\n' >"$C2/src/c2.rs"
turn_in "$C2" clean2 "only" --agent claude-code --model m-alpha >/dev/null

# recent-wip: unshipped work, but the thread is under a day old, so it is not abandoned.
P="$TMP/wt-recent-wip"
printf 'wip\n' >"$P/src/wip.rs"
turn_in "$P" recent-wip "wip" --agent claude-code --model m-alpha >/dev/null

# plant NAME SECONDS_AGO PARENT BASE TREE_SOURCE_COMMIT PATCH_COMMAND SUBJECT : one turn commit with a
# chosen age, a full snapshot as its tree, in the thread's ref.
plant() {
  local name=$1 ago=$2 seq=$3 base=$4 src=$5 patch=$6 dir="$TMP/plant-$1-$3"
  local parent="" index="$TMP/plant.index" when tree commit
  local repo=${P_REPO:-$R} side=${P_SIDE:-$SIDE}
  mkdir -p "$dir"
  git -C "$repo" archive "$src" | tar -x -C "$dir"
  (cd "$dir" && eval "$patch")
  rm -f "$index"
  when="$(iso $((NOW - ago)))"
  GIT_INDEX_FILE="$index" git --git-dir="$side" --work-tree="$dir" add -A >/dev/null 2>&1
  tree=$(GIT_INDEX_FILE="$index" git --git-dir="$side" write-tree)
  parent=$(git --git-dir="$side" rev-parse -q --verify "refs/baton/$name" 2>/dev/null || true)
  {
    printf 'planted turn %s\n\n' "$seq"
    printf 'Baton-Thread: %s\nBaton-Turn: %s\nBaton-Agent: claude-code\nBaton-Model: m-gamma\nBaton-Machine: e2e\nBaton-Base: %s\n' "$name" "$seq" "$base"
  } >"$TMP/plant.msg"
  commit=$(GIT_AUTHOR_NAME=baton GIT_AUTHOR_EMAIL=baton@localhost GIT_COMMITTER_NAME=baton \
    GIT_COMMITTER_EMAIL=baton@localhost GIT_AUTHOR_DATE="$when" GIT_COMMITTER_DATE="$when" \
    git --git-dir="$side" commit-tree "$tree" ${parent:+-p "$parent"} -F "$TMP/plant.msg")
  git --git-dir="$side" update-ref "refs/baton/$name" "$commit"
  rm -rf "$dir" "$index"
}

# lost: two turns three days ago whose files never reach HEAD.
plant lost $((3 * DAY)) 1 "$SHIPPED" "$SHIPPED" 'printf "lost a\nlost b\nlost c\n" >src/lost1.rs'
plant lost $((3 * DAY - 3600)) 2 "$SHIPPED" "$SHIPPED" 'printf "lost a\nlost b\nlost c\n" >src/lost1.rs; printf "lost d\n" >src/lost2.rs'
# late-a: work from five days ago that the user shipped four days ago.
plant late-a $((5 * DAY)) 1 "$BASE" "$BASE" 'printf "late one\nlate two\nlate three\n" >src/late.rs'
# late-b: three hours ago removes a line late-a's work shipped four days ago: past the 48-hour window.
plant late-b 10800 1 "$LATE" "$LATE" 'edit "/^late two$/d" src/late.rs'

out=$(cd "$R" && "$BIN" log lost 2>&1)
contains "baton log reads a planted turn" "$out" "lost"

# ---------------------------------------------------------------------------------------------- the report
section "the JSON document"
DIGEST_BEFORE=$(git_digest "$R")
J=$(cd "$R" && json)
contains "schema" "$J" '"schema":"baton.report.v1"'
contains "project" "$J" '"project":"github.com/e2e/rep"'
contains "the repository was available" "$J" '"available":true'
contains "head is the user's HEAD" "$J" "\"head\":\"$SHIPPED\""
contains "thrash: the file rule names thread, turn and file" "$J" '{"kind":"file","thread":"thrash1","turn":3,"file":"src/hot.rs","turns":[1,2,3,4],"lines":2}'
contains "thrash: the check rule names thread, turn and check" "$J" '{"kind":"check","thread":"flaky","turn":3,"check":"tests"'
contains "rework: author's file, reworked by reworker" "$J" '"thread":"author","turn":1,"file":"src/ship.rs"'
contains "rework: author's line in hot.rs" "$J" '"thread":"author","turn":1,"file":"src/hot.rs"'
contains "rework names who changed it" "$J" '"by":{"thread":"reworker","turn":1'
contains "abandoned: lost, last turn 2" "$J" '"thread":"lost","turn":2'
contains "abandoned: lists lost1.rs" "$J" '"path":"src/lost1.rs"'
contains "abandoned: lists lost2.rs" "$J" '"path":"src/lost2.rs"'
contains "hotspot: hot.rs carries a thrash and a rework" "$J" '{"file":"src/hot.rs","thrash":1,"rework":1}'
contains "models: m-alpha is present" "$J" '"name":"m-alpha"'
contains "agents: codex is present" "$J" '"name":"codex"'
contains "counts: one file thrash and one check flip" "$J" '"thrash":2,'
contains "counts: two threads are abandoned or one" "$J" '"abandoned":1,'

section "clean threads stay out"
for t in clean1 clean2 recent-wip steady greenred late-a late-b; do
  lacks "$t is nowhere in the document" "$J" "\"thread\":\"$t\""
done
lacks "no clean file is a hotspot" "$J" 'src/c1.rs'
lacks "late.rs is not reworked after 48 hours" "$J" 'src/late.rs'
lacks "the flaky thread is not a file thrash" "$J" '"kind":"file","thread":"flaky"'

section "per model and per agent"
alpha_commits=$(git --git-dir="$SIDE" log --all --format=%B | grep -c '^Baton-Model: m-alpha$')
alpha_turns=$(sed -n 's/.*{"name":"m-alpha","turns":\([0-9]*\),.*/\1/p' <<<"$J")
eq "m-alpha turns equal the commits that carry the trailer" "$alpha_commits" "$alpha_turns"
gamma_commits=$(git --git-dir="$SIDE" log --all --format=%B | grep -c '^Baton-Model: m-gamma$')
eq "m-gamma has the four planted turns" 4 "$gamma_commits"

section "plain text"
T=$(cd "$R" && text)
contains "thrash section" "$T" "Thrash ("
contains "rework section" "$T" "Rework ("
contains "abandoned section" "$T" "Abandoned ("
contains "hotspots section" "$T" "Hotspots ("
contains "a thrash entry names thread, turn and file" "$T" "thrash1@3  src/hot.rs"
contains "a check entry names the check" "$T" "flaky@3  check tests"
contains "a rework entry" "$T" "author@1  src/ship.rs"
contains "an abandoned entry lists its files" "$T" "lost@2"
lacks "no clean thread in the text" "$T" "clean1"
lacks "no recent thread in the text" "$T" "recent-wip"

section "options"
J2=$(json --project github.com/e2e/rep --since 1d)
contains "--project works outside the worktree" "$J2" '"schema":"baton.report.v1"'
lacks "a one day window drops the planted threads" "$J2" '"thread":"lost"'
contains "a one day window keeps recent work" "$J2" '"thread":"thrash1"'
out=$(cd "$TMP" && "$BIN" report 2>&1)
contains "outside any worktree it says so" "$out" "git repository"
out=$("$BIN" report --since banana 2>&1)
contains "a bad window is refused" "$out" "not a window"

section "read-only"
eq "the user's repository is byte-identical" "$DIGEST_BEFORE" "$(git_digest "$R")"
# A daemon that starts writes "daemon starting" to its log, so a log that has not grown means none started.
kill_pid "$(daemon_pid)"
sleep 1
LOG="$BATON_HOME/data/daemon.log"
log_before=$(wc -c <"$LOG")
(cd "$R" && "$BIN" report >/dev/null 2>&1; "$BIN" report --json >/dev/null 2>&1)
eq "a report starts no daemon" "$log_before" "$(wc -c <"$LOG")"

# ---------------------------------------------------------------------------------------------- the store policy
section "a first turn is compared with what the store could hold"
# gen/** is excluded and cfg.toml is stored redacted, so neither is a change the thread made. Both are still
# in HEAD, and the base commit's raw trees differ from the turn's in all of them.
X="$TMP/gen"
XS=$(side_of gen)
git init -q "$X"
git -C "$X" remote add origin "https://github.com/E2E/gen.git"
mkdir -p "$X/gen" "$X/src" "$X/.baton"
printf '[snapshot]\nexclude = ["gen/**"]\n' >"$X/.baton/config.toml"
for n in a b c; do seq 1 40 | sed "s/^/generated $n line /" >"$X/gen/$n.rs"; done
printf 'name = "svc"\ndb_password = "hunter2-hunter2-hunter2"\nport = 8080\n' >"$X/cfg.toml"
printf 'fn one() {}\nfn two() {}\n' >"$X/src/app.rs"
git -C "$X" add -A >/dev/null 2>&1
at $((NOW - 10 * DAY)) git -C "$X" commit -q -m "base"
"$BIN" enroll --repo "$(wp "$X")" >/dev/null 2>&1
printf 'fn three() {}\n' >>"$X/src/app.rs"
turn_in "$X" gen1 "add three" --agent claude-code --model m-alpha >/dev/null
side_tree=$(git --git-dir="$XS" ls-tree -r --name-only refs/baton/gen1 | tr '\n' ' ')
lacks "the store left the excluded files out" "$side_tree" "gen/a.rs"
lacks "the store redacted the password" "$(git --git-dir="$XS" show refs/baton/gen1:cfg.toml)" "hunter2"

# The turn's commit is rewritten three days old, then the user ships the line by hand.
tree=$(git --git-dir="$XS" rev-parse "refs/baton/gen1^{tree}")
git --git-dir="$XS" log -1 --format=%B refs/baton/gen1 >"$TMP/gen.msg"
when="$(iso $((NOW - 3 * DAY)))"
commit=$(GIT_AUTHOR_NAME=baton GIT_AUTHOR_EMAIL=baton@localhost GIT_COMMITTER_NAME=baton \
  GIT_COMMITTER_EMAIL=baton@localhost GIT_AUTHOR_DATE="$when" GIT_COMMITTER_DATE="$when" \
  git --git-dir="$XS" commit-tree "$tree" -F "$TMP/gen.msg")
git --git-dir="$XS" update-ref refs/baton/gen1 "$commit"
git -C "$X" add src/app.rs >/dev/null 2>&1
at $((NOW - 2 * DAY)) git -C "$X" commit -q -m "ship three"

J=$(cd "$X" && json)
changed=$(sed -n 's/.*"thread":"gen1","turn":1,"last_at":[0-9]*,"changed_lines":\([0-9]*\),"reached_lines":\([0-9]*\).*/\1 changed, \2 reached/p' <<<"$J")
echo "  gen1: ${changed:-not abandoned}; $(grep -o '"abandoned":[0-9]*,' <<<"$J")"
contains "the thread's one line reached HEAD, so nothing is abandoned" "$J" '"abandoned":0,'
lacks "the thread is nowhere in the document" "$J" '"thread":"gen1"'

# ---------------------------------------------------------------------------------------------- shared lines and skew
section "two threads that wrote the same line, and a turn with a skewed clock"
D="$TMP/dup"
DS=$(side_of dup)
git init -q "$D"
git -C "$D" remote add origin "https://github.com/E2E/dup.git"
mkdir -p "$D/src"
printf 'base d\n' >"$D/src/d.rs"
git -C "$D" add -A >/dev/null 2>&1
at $((NOW - 10 * DAY)) git -C "$D" commit -q -m "base"
DBASE=$(git -C "$D" rev-parse HEAD)
git init -q --bare "$DS"
export P_REPO=$D P_SIDE=$DS
# dupA wrote the lines first and dupB wrote the same text a day later; the user shipped it once, and dupC
# removed one line the next day.
plant dupA $((5 * DAY)) 1 "$DBASE" "$DBASE" 'printf "dup one\ndup two\n" >src/dup.rs'
plant dupB $((4 * DAY)) 1 "$DBASE" "$DBASE" 'printf "dup one\ndup two\n" >src/dup.rs'
printf 'dup one\ndup two\n' >"$D/src/dup.rs"
git -C "$D" add -A >/dev/null 2>&1
at $((NOW - 3 * DAY)) git -C "$D" commit -q -m "ship dup"
DSHIP=$(git -C "$D" rev-parse HEAD)
plant dupC $((2 * DAY)) 1 "$DSHIP" "$DSHIP" 'edit "/^dup two$/d" src/dup.rs'
# skew: turn 2 was written on a machine whose clock is nine days behind, between two turns inside the window.
plant skew 10800 1 "$DBASE" "$DBASE" 'printf "skew one\n" >src/skew.rs'
plant skew $((9 * DAY)) 2 "$DBASE" "$DBASE" 'printf "skew one\nskew two\n" >src/skew.rs'
plant skew 7200 3 "$DBASE" "$DBASE" 'printf "skew one\nskew two\nskew three\n" >src/skew.rs'
unset P_REPO P_SIDE

J=$(cd "$D" && json)
contains "the first thread to write the lines is credited with the rework" "$J" '"thread":"dupA","turn":1,"file":"src/dup.rs","lines":1,'
lacks "the thread that wrote the same text later is not" "$J" '"thread":"dupB"'
contains "the turns on both sides of a skewed one are counted" "$J" '"counts":{"threads":4,"turns":5,'

# ---------------------------------------------------------------------------------------------- scale
section "a large project: cold run against a cached run"
BIG="$TMP/big"
git init -q "$BIG"
git -C "$BIG" remote add origin "https://github.com/E2E/big.git"
BIGSIDE=$(side_of big)

# The user's repository: FILES files, then 400 commits over the last week that touch three files each.
base_stream() {
  awk -v n="$FILES" -v now="$NOW" 'BEGIN {
    printf "commit refs/heads/main\ncommitter e2e <e2e@localhost> %d +0000\ndata 5\nbase\n", now - 9 * 86400
    for (i = 0; i < n; i++) {
      body = sprintf("line a %d\nline b %d\nline c %d\nline d %d\n", i, i, i, i)
      printf "M 100644 inline src/d%02d/f%05d.rs\ndata %d\n%s\n", i % 40, i, length(body), body
    }
  }'
}
{
  base_stream
  awk -v n="$FILES" -v now="$NOW" 'BEGIN {
    for (c = 1; c <= 400; c++) {
      t = now - 7 * 86400 + c * 1400
      printf "commit refs/heads/main\ncommitter e2e <e2e@localhost> %d +0000\ndata 7\nchange\n", t
      for (k = 0; k < 3; k++) {
        i = (c * 7 + k * 131) % n
        body = sprintf("line a %d\nchange %d %d\nline c %d\nline d %d\n", i, c, k, i, i)
        printf "M 100644 inline src/d%02d/f%05d.rs\ndata %d\n%s\n", i % 40, i, length(body), body
      }
    }
  }'
} | git -C "$BIG" fast-import --quiet
git -C "$BIG" reset -q --hard
BIGBASE=$(git -C "$BIG" rev-list --max-parents=0 HEAD)
T0=$(git -C "$BIG" rev-parse "$BIGBASE^{tree}")
git init -q --bare "$BIGSIDE"

# The side repository: THREADS threads of TURNS turns. A thread's first turn replaces the root tree with the
# base tree (a turn tree is a full snapshot) and has no parent; each later turn edits three files. The base
# tree must exist in the side repository first, so the base commit goes in under a scratch ref.
base_stream | sed 's#refs/heads/main#refs/scratch/base#' | git --git-dir="$BIGSIDE" fast-import --quiet
awk -v n="$FILES" -v threads="$THREADS" -v turns="$TURNS" -v now="$NOW" -v base="$BIGBASE" -v t0="$T0" 'BEGIN {
  for (th = 0; th < threads; th++) {
    for (u = 1; u <= turns; u++) {
      t = now - 6 * 86400 + th * 1200 + u * 60
      model = (th % 3 == 0) ? "m-alpha" : "m-beta"
      agent = (th % 2 == 0) ? "claude-code" : "codex"
      msg = sprintf("turn %d\n\nBaton-Thread: t%04d\nBaton-Turn: %d\nBaton-Agent: %s\nBaton-Model: %s\nBaton-Machine: e2e\nBaton-Base: %s\n", u, th, u, agent, model, base)
      printf "commit refs/baton/t%04d\ncommitter baton <baton@localhost> %d +0000\ndata %d\n%s", th, t, length(msg), msg
      if (u == 1) printf "M 040000 %s \"\"\n", t0
      for (k = 0; k < 3; k++) {
        i = (th * 13 + u * 17 + k * 101) % n
        body = sprintf("line a %d\nthread %d turn %d %d\nline c %d\nline d %d\n", i, th, u, k, i, i)
        printf "M 100644 inline src/d%02d/f%05d.rs\ndata %d\n%s\n", i % 40, i, length(body), body
      }
    }
  }
}' | git --git-dir="$BIGSIDE" fast-import --quiet
sides=$(git --git-dir="$BIGSIDE" for-each-ref refs/baton | wc -l | tr -d ' ')
eq "the side repository holds every thread" "$THREADS" "$sides"

CACHE="$BATON_HOME/data/report-cache"
rm -rf "$CACHE"
t0=$(now_ms)
(cd "$BIG" && "$BIN" report --since 14d --json >"$TMP/cold.json" 2>"$TMP/cold.err")
cold=$(($(now_ms) - t0))
t0=$(now_ms)
(cd "$BIG" && "$BIN" report --since 14d --json >"$TMP/warm.json" 2>"$TMP/warm.err")
warm=$(($(now_ms) - t0))
turns_counted=$(sed -n 's/.*"counts":{"threads":[0-9]*,"turns":\([0-9]*\),.*/\1/p' "$TMP/warm.json")
eq "every turn is in the report" $((THREADS * TURNS)) "$turns_counted"
eq "the cached run prints the same document" \
  "$(sed 's/"generated_at":[0-9]*,//; s/"since":[0-9]*,//' "$TMP/cold.json")" \
  "$(sed 's/"generated_at":[0-9]*,//; s/"since":[0-9]*,//' "$TMP/warm.json")"
if ((warm < cold)); then ok "the cached run is faster than the cold one"; else bad "the cached run is faster" "cold ${cold} ms, cached ${warm} ms"; fi
cache_bytes=$(find "$CACHE" -type f -exec wc -c {} + 2>/dev/null | tail -1 | awk '{print $1}')
echo "  $THREADS threads x $TURNS turns, $FILES files, 400 user commits: cold ${cold} ms, cached ${warm} ms, cache ${cache_bytes:-0} bytes"
SCALE="$THREADS threads x $TURNS turns over $FILES files: cold ${cold} ms, cached ${warm} ms"
eq "the large project's repository is untouched by the report" "clean" \
  "$([[ -z $(git -C "$BIG" status --porcelain) ]] && echo clean || echo dirty)"

# The cache is rewritten rather than grown: a repeated run adds nothing, and a thread whose ref is gone
# takes its entries with it.
cache_file=$(find "$CACHE" -name '*.bin' | head -1)
size_warm=$(wc -c <"$cache_file")
(cd "$BIG" && "$BIN" report --since 14d --json >/dev/null 2>&1)
eq "a repeated run leaves the cache file as it was" "$size_warm" "$(wc -c <"$cache_file")"
git --git-dir="$BIGSIDE" update-ref -d refs/baton/t0000
(cd "$BIG" && "$BIN" report --since 14d --json >"$TMP/after.json" 2>/dev/null)
size_after=$(wc -c <"$cache_file")
if ((size_after < size_warm)); then ok "a deleted thread's entries leave the cache"; else bad "a deleted thread's entries leave the cache" "$size_warm bytes before, $size_after after"; fi
eq "the report counts one thread fewer" $((THREADS - 1)) "$(sed -n 's/.*"counts":{"threads":\([0-9]*\),.*/\1/p' "$TMP/after.json")"

# ---------------------------------------------------------------------------------------------- summary
echo
echo "== summary"
for line in "${REPORT[@]}"; do [[ $line == FAIL* ]] && echo "$line"; done
echo "checks: $pass passed, $fail failed"
echo "scale: $SCALE"
[[ $fail == 0 ]]
