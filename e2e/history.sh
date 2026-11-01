#!/usr/bin/env bash
# End-to-end run of `baton blame` and `baton bisect` against the real release binary and throwaway repos.
#
#   e2e/history.sh                  uses target/release/baton, building it when missing
#   BATON_BIN=/path/baton e2e/history.sh
#
# Everything lives under one temp dir: BATON_HOME, the git config, every repo and the bisect scratch
# worktrees (TMP, TEMP and TMPDIR point into it for `baton bisect`). Needs bash, git, sleep.

set -u
export LC_ALL=C

root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
exe=""
[[ ${OSTYPE:-} == msys* || ${OSTYPE:-} == cygwin* ]] && exe=".exe"
BIN=${BATON_BIN:-$root/target/release/baton$exe}
if [[ ! -x $BIN ]]; then
  (cd "$root" && cargo build --release -q) || { echo "cargo build --release failed" >&2; exit 2; }
fi

# A path the native binary and git understand. Git Bash needs `cygpath`; elsewhere paths already are.
wp() { if command -v cygpath >/dev/null 2>&1; then cygpath -m "$1"; else printf '%s' "$1"; fi; }

W=$(mktemp -d)
export BATON_HOME="$(wp "$W")/home"
export GIT_CONFIG_GLOBAL="$W/gitconfig"
export GIT_CONFIG_NOSYSTEM=1
export GIT_OPTIONAL_LOCKS=0
git config --file "$GIT_CONFIG_GLOBAL" user.name e2e
git config --file "$GIT_CONFIG_GLOBAL" user.email e2e@localhost
git config --file "$GIT_CONFIG_GLOBAL" init.defaultBranch main
git config --file "$GIT_CONFIG_GLOBAL" core.autocrlf false
mkdir -p "$BATON_HOME/config" "$W/scratch"
printf '[machine]\nid = "e2e-laptop"\n' >"$BATON_HOME/config/config.toml"

pass=0
fail=0
section_name=""

kill_pid() {
  case ${OSTYPE:-} in
    msys* | cygwin*) taskkill //F //PID "$1" >/dev/null 2>&1 ;;
    *) kill -9 "$1" >/dev/null 2>&1 ;;
  esac
}

cleanup() {
  local pid
  if [[ -d $W/home/data ]]; then
    pid=$("$BIN" status 2>/dev/null | sed -n 's/^daemon pid \([0-9]*\).*/\1/p')
    [[ -n $pid ]] && kill_pid "$pid"
    sleep 0.5
  fi
  rm -rf "$W"
}
trap cleanup EXIT

section() {
  section_name=$1
  echo
  echo "== $1"
}
ok() {
  pass=$((pass + 1))
  echo "  ok   $1"
}
bad() {
  fail=$((fail + 1))
  echo "  FAIL $section_name: $1${2:+  ($2)}"
}
eq() { if [[ $2 == "$3" ]]; then ok "$1"; else bad "$1" "expected [$2], got [$3]"; fi; }
contains() { if [[ $2 == *"$3"* ]]; then ok "$1"; else bad "$1" "missing [$3] in: $2"; fi; }
lacks() { if [[ $2 != *"$3"* ]]; then ok "$1"; else bad "$1" "found [$3] in: $2"; fi; }
check() {
  local name=$1
  shift
  if "$@" >/dev/null 2>&1; then ok "$name"; else bad "$name"; fi
}

mkrepo() { # mkrepo dir name : one commit holding the files given as name=content pairs after the name
  local dir=$1 name=$2
  shift 2
  git init -q "$dir"
  git -C "$dir" remote add origin "https://github.com/E2E/$name.git"
  local pair
  for pair in "$@"; do
    mkdir -p "$dir/$(dirname "${pair%%=*}")"
    printf '%b' "${pair#*=}" >"$dir/${pair%%=*}"
  done
  git -C "$dir" add -A >/dev/null 2>&1
  git -C "$dir" commit -q -m initial
}

turn() { # turn worktree thread message [agent model] : records the worktree as it is now
  (cd "$1" && "$BIN" turn --thread "$2" --message "$3" --agent "${4:-e2e-agent}" --model "${5:-e2e-model}" 2>&1)
}

# --------------------------------------------------------------------------------------------------
section "blame: three threads, a commit that ships some lines, one hand-written line"
R="$W/blame"
mkrepo "$R" blame "src/app.txt=alpha\nbeta\ngamma\n" "side.txt=side\n"
"$BIN" enroll --repo "$(wp "$R")" >/dev/null 2>&1
for w in x a b c; do git -C "$R" worktree add -q -b "$w" "$W/blame-$w"; done
# thread x runs on a branch that is never shipped: its base is not an ancestor of the shipped commit
printf 'other\n' >"$W/blame-x/side.txt"
git -C "$W/blame-x" commit -q -am "side commit"
printf 'alpha agent line one\n' >>"$W/blame-x/src/app.txt"
turn "$W/blame-x" x-thread "Write the line on the side branch" >/dev/null
sleep 1.2
printf 'alpha agent line one\n' >>"$W/blame-a/src/app.txt"
turn "$W/blame-a" alpha-thread "Add the first alpha line" claude-code m-alpha >/dev/null
printf 'shared agent line\n' >>"$W/blame-a/src/app.txt"
turn "$W/blame-a" alpha-thread "Add the shared line" claude-code m-alpha >/dev/null
sleep 1.2
printf 'beta agent line\n' >"$W/blame-b/src/beta.txt"
turn "$W/blame-b" beta-thread "Add the beta file" codex m-beta >/dev/null
sleep 1.2
printf 'shared agent line\n' >>"$W/blame-c/src/app.txt"
turn "$W/blame-c" gamma-thread "Write the shared line again" opencode >/dev/null
printf 'gamma agent line\n' >>"$W/blame-c/src/app.txt"
turn "$W/blame-c" gamma-thread "Add the gamma line" opencode >/dev/null
log_out=$(cd "$R" && "$BIN" log alpha-thread 2>&1)
contains "the setup turns were recorded" "$log_out" "alpha-thread@2"
sleep 1.2
git -C "$W/blame-a" commit -q -am "ship alpha lines"
git -C "$R" merge -q --ff-only a
printf 'hand written line\n' >>"$R/src/app.txt"
git -C "$R" commit -q -am "hand edit"
status_before=$(git -C "$R" status --porcelain)
lineno() { grep -n -F -x -- "$1" "$R/src/app.txt" | head -1 | cut -d: -f1; }
L_ONE=$(lineno "alpha agent line one")
L_SHARED=$(lineno "shared agent line")
L_HAND=$(lineno "hand written line")
L_BASE=$(lineno "alpha")

out=$(cd "$R" && "$BIN" blame "src/app.txt:$L_ONE" 2>&1)
contains "an agent-written line names its thread and turn" "$out" "thread alpha-thread  turn 1"
contains "and the agent, model and machine" "$out" "agent claude-code  model m-alpha  machine e2e-laptop"
contains "and the git commit's subject" "$out" "ship alpha lines"
contains "and the turn's message" "$out" "message Add the first alpha line"
contains "and baton log's line" "$out" "alpha-thread@1  "
contains "the side-branch copy counts as another match" "$out" "1 other turn also added this line"
lacks "the earlier side-branch turn did not win (its base is not an ancestor)" "$out" "x-thread@1"

out=$(cd "$R" && "$BIN" blame "src/app.txt:$L_SHARED" 2>&1)
contains "of two threads that wrote a line, the earlier turn wins" "$out" "thread alpha-thread  turn 2"
contains "and the other is counted" "$out" "1 other turn also added this line"

out=$(cd "$R" && "$BIN" blame "src/app.txt:$L_HAND" 2>&1)
contains "a hand-written line was not written in a recorded turn" "$out" "not written in a recorded turn"
contains "and its commit is still shown" "$out" "hand edit"

out=$(cd "$R" && "$BIN" blame "src/app.txt:$L_BASE" 2>&1)
contains "a line that was in the base commit is not attributed to the thread that kept it" "$out" "not written in a recorded turn"

out=$(cd "$R" && "$BIN" blame src/app.txt -L "$L_ONE,$L_HAND" 2>&1)
eq "-L prints one turn line per file line" 3 "$(grep -c '^  turn ' <<<"$out")"

out=$(cd "$R" && "$BIN" blame src/app.txt -L "$L_ONE,$L_ONE" --json 2>&1)
contains "--json names its schema" "$out" '"schema": "baton.blame.v1"'
contains "--json names the thread" "$out" '"thread": "alpha-thread"'
contains "--json carries the model" "$out" '"model": "m-alpha"'
contains "--json carries the other matches" "$out" '"others": 1'
out=$(cd "$R" && "$BIN" blame src/app.txt -L "$L_HAND,$L_HAND" --json 2>&1)
contains "--json says a hand-written line has no turn" "$out" '"turn": null'

out=$(cd "$R" && "$BIN" blame src/app.txt 2>&1)
contains "a file with no line is refused with the usage" "$out" "name a line"
eq "blame left the user's worktree as it was" "$status_before" "$(git -C "$R" status --porcelain)"

# --------------------------------------------------------------------------------------------------
section "bisect: a planted failing turn, a skip, a timeout"
B="$W/bisect"
mkrepo "$B" bisect "state.txt=start\n" "keep.txt=keep\n"
"$BIN" enroll --repo "$(wp "$B")" >/dev/null 2>&1
BASH_ABS=$(wp "$(command -v bash)")
export RUNS="$(wp "$W")/runs.log"
export MARK="$(wp "$W")/marker"
cat >"$W/check.sh" <<'EOF'
#!/usr/bin/env bash
cat state.txt >>"$RUNS"
if grep -q HANG state.txt; then
  ( sleep 5; touch "$MARK" ) &
  sleep 30
  exit 0
fi
grep -q SKIP state.txt && exit 125
grep -q BAD state.txt && exit 1
exit 0
EOF
CHECK=("$BASH_ABS" "$(wp "$W")/check.sh")

thread_of() { # thread_of name state... : a worktree with one turn per state
  local name=$1 n=0 s
  shift
  git -C "$B" worktree add -q -b "$name" "$W/bisect-$name"
  for s in "$@"; do
    n=$((n + 1))
    printf '%s\n' "$s" >"$W/bisect-$name/state.txt"
    mkdir -p "$W/bisect-$name/extra"
    printf '%s\n' "$n" >"$W/bisect-$name/extra/$n.txt"
    turn "$W/bisect-$name" "$name" "turn $n" >/dev/null
  done
}
bisect() { # bisect args... : from the main worktree, scratch worktrees under $W/scratch
  (cd "$B" && TMP="$(wp "$W/scratch")" TEMP="$(wp "$W/scratch")" TMPDIR="$W/scratch" "$BIN" bisect "$@" 2>&1)
}
runs() { [[ -f $W/runs.log ]] && wc -l <"$W/runs.log" | tr -d ' ' || echo 0; }
worktrees() { git -C "$B" worktree list | wc -l | tr -d ' '; }

thread_of plant ok1 ok2 ok3 BAD4 BAD5 BAD6
before=$(worktrees)
kept=$(cat "$W/bisect-plant/state.txt")
out=$(bisect plant -- "${CHECK[@]}")
rc=$?
eq "bisect exits 0" 0 "$rc"
contains "it finds the planted turn" "$out" "plant@4  "
contains "and lists the paths that turn changed" "$out" "A extra/4.txt"
contains "and M for the state file" "$out" "M state.txt"
lacks "it did not name the turn after it" "$out" "plant@5  "
eq "it ran the check on 4 turns (the first, the latest, then two more)" 4 "$(runs)"
eq "no worktree is left registered" "$before" "$(worktrees)"
eq "git has nothing to prune" "" "$(git -C "$B" worktree prune --dry-run 2>&1)"
eq "the scratch directory is empty" "" "$(ls -A "$W/scratch")"
eq "the thread's own worktree was not touched" "$kept" "$(cat "$W/bisect-plant/state.txt")"

: >"$W/runs.log"
out=$(bisect plant --good 3 --bad 6 -- "${CHECK[@]}")
contains "--good and --bad narrow the search" "$out" "plant@4  "
eq "and the latest turn is not run first" 2 "$(runs)"
out=$(bisect plant --good 9 -- "${CHECK[@]}")
contains "a turn the thread lacks is refused" "$out" "no turn 9"

thread_of clean ok1 ok2 ok3
before=$(worktrees)
out=$(bisect clean -- "${CHECK[@]}")
rc=$?
eq "a thread whose latest turn passes is an error" 1 "$rc"
contains "and says so" "$out" "passes at the latest turn"
eq "even then no worktree is left" "$before" "$(worktrees)"

: >"$W/runs.log"
thread_of skippy ok1 ok2 ok3 ok4 SKIP5 ok6 BAD7 BAD8
before=$(worktrees)
out=$(bisect skippy -- "${CHECK[@]}")
rc=$?
eq "a run that exits 125 does not stop the search" 0 "$rc"
contains "the skip is reported" "$out" "skipped (exit 125)"
contains "the first bad turn is still found" "$out" "skippy@7  "
eq "the skipped turn was run once and not again" 1 "$(grep -c SKIP5 "$W/runs.log")"
eq "no worktree is left after a skip" "$before" "$(worktrees)"

rm -f "$MARK"
thread_of hangs ok1 ok2 HANG3
before=$(worktrees)
started=$SECONDS
out=$(bisect hangs --timeout 2s -- "${CHECK[@]}")
rc=$?
eq "a timed-out run is a bad turn" 0 "$rc"
contains "the timeout is reported" "$out" "timed out after 2s"
contains "and the hanging turn is the first bad one" "$out" "hangs@3  "
((SECONDS - started < 20)) && ok "the search did not wait for the 30 s sleep" || bad "the search waited for the sleep"
sleep 5
check "the check's background child was killed with it (it never wrote its marker)" test ! -e "$MARK"
eq "no worktree is left after a timeout" "$before" "$(worktrees)"
eq "and the scratch directory is empty" "" "$(ls -A "$W/scratch")"
out=$(bisect hangs --timeout soon -- "${CHECK[@]}")
contains "a bad timeout is refused" "$out" "is not a timeout"

if [[ ${OSTYPE:-} != msys* && ${OSTYPE:-} != cygwin* ]]; then
  cat >"$W/slow.sh" <<'EOF'
#!/usr/bin/env bash
touch "$MARK.started"
sleep 30
EOF
  rm -f "$MARK.started"
  # exec, so that $! is baton itself and the signal reaches it
  (cd "$B" && TMPDIR="$W/scratch" exec "$BIN" bisect plant --good 1 --bad 6 -- bash "$W/slow.sh" >"$W/int.out" 2>&1) &
  pid=$!
  for _ in $(seq 1 100); do [[ -e $MARK.started ]] && break; sleep 0.1; done
  kill -INT "$pid"
  wait "$pid"
  eq "Ctrl+C ends the search with 130" 130 $?
  eq "and removes the scratch worktree" "$before" "$(worktrees)"
  eq "and its directory" "" "$(ls -A "$W/scratch")"
else
  echo "  skipped: Ctrl+C on Windows is not driven from bash"
fi

# --------------------------------------------------------------------------------------------------
section "bisect: a failing first turn, a one-turn thread, a planted directory, a killed run"
thread_of allbad BAD1 BAD2 BAD3
: >"$W/runs.log"
before=$(worktrees)
out=$(bisect allbad -- "${CHECK[@]}")
rc=$?
eq "a thread whose turns all fail exits 0" 0 "$rc"
contains "and reports turn 1" "$out" "allbad@1  "
eq "after running only the first turn" 1 "$(runs)"
out=$(bisect allbad --bad 1 -- "${CHECK[@]}")
rc=$?
eq "--bad 1 exits 0" 0 "$rc"
contains "and reports turn 1 after running it" "$out" "allbad@1  "
eq "no worktree is left after a first-turn find" "$before" "$(worktrees)"

thread_of single BAD1
out=$(bisect single -- "${CHECK[@]}")
rc=$?
eq "a one-turn thread that fails exits 0" 0 "$rc"
contains "and reports its turn" "$out" "single@1  "
thread_of singleok ok1
out=$(bisect singleok -- "${CHECK[@]}")
rc=$?
eq "a one-turn thread that passes is the passes-at-the-latest-turn error" 1 "$rc"
contains "and says so" "$out" "passes at the latest turn"
out=$(bisect singleok --bad 1 -- "${CHECK[@]}")
contains "--bad 1 naming a passing turn is refused" "$out" "passes at turn 1"
thread_of firstskip SKIP1 ok2 BAD3
out=$(bisect firstskip -- "${CHECK[@]}")
contains "a skipped first turn is assumed good and the search goes on" "$out" "firstskip@3  "
thread_of allskip SKIP1 SKIP2 BAD3
out=$(bisect allskip -- "${CHECK[@]}")
contains "turns skipped before the failing one are listed with it" "$out" "one of these 3"
out=$(bisect plant --good 2 --bad 1 -- "${CHECK[@]}")
contains "--good at or after --bad is still refused" "$out" "needs a good turn before a bad one"

# A directory that was there before the run is never removed, even when `git worktree add` fails (here the
# admin directory cannot be made, because a file stands where git wants `worktrees/`).
mkdir -p "$W/scratch/baton-bisect-planted"
printf 'mine\n' >"$W/scratch/baton-bisect-planted/KEEP.txt"
mkdir -p "$W/scratch/baton-bisect-$$-plant"
printf 'mine\n' >"$W/scratch/baton-bisect-$$-plant/KEEP.txt"
mv "$B/.git/worktrees" "$W/worktrees.aside"
printf 'in the way\n' >"$B/.git/worktrees"
out=$(bisect plant -- "${CHECK[@]}")
rc=$?
rm -f "$B/.git/worktrees"
mv "$W/worktrees.aside" "$B/.git/worktrees"
eq "a failing worktree add fails the command" 1 "$rc"
contains "and the error names it" "$out" "making the scratch worktree"
check "a planted directory survived" test -f "$W/scratch/baton-bisect-planted/KEEP.txt"
check "and one named the way the old scheme would" test -f "$W/scratch/baton-bisect-$$-plant/KEEP.txt"
eq "baton removed its own scratch directory and nothing else" "baton-bisect-$$-plant
baton-bisect-planted" "$(ls -A "$W/scratch")"
rm -rf "$W/scratch/baton-bisect-planted" "$W/scratch/baton-bisect-$$-plant"

# A baton killed hard cannot clean up. The next bisect in the repository does.
kill_tree() {
  case ${OSTYPE:-} in
    msys* | cygwin*) taskkill //F //T //PID "$1" >/dev/null 2>&1 ;;
    *)
      pkill -9 -P "$1" >/dev/null 2>&1
      kill -9 "$1" >/dev/null 2>&1
      ;;
  esac
}
cat >"$W/slow-forever.sh" <<'EOF'
#!/usr/bin/env bash
touch "$MARK.started"
sleep 30
EOF
thread_of stale ok1 ok2 BAD3 BAD4
before=$(worktrees)
rm -f "$MARK.started"
(cd "$B" && TMP="$(wp "$W/scratch")" TEMP="$(wp "$W/scratch")" TMPDIR="$W/scratch" exec "$BIN" bisect stale --good 1 --bad 4 -- "$BASH_ABS" "$(wp "$W")/slow-forever.sh" >"$W/stale.out" 2>&1) &
bg=$!
for _ in $(seq 1 100); do [[ -e $MARK.started ]] && break; sleep 0.1; done
home=$(ls -A "$W/scratch")
pid=$(printf '%s' "$home" | sed -n 's/^baton-bisect-\([0-9]*\)-.*/\1/p')
check "the run's scratch directory is named for its pid and a random suffix" test -n "$pid"
out=$(bisect plant -- "${CHECK[@]}")
lacks "a second bisect leaves a running one's worktree alone" "$out" "removed the scratch worktree"
eq "and it is still registered" "$((before + 1))" "$(worktrees)"
kill_tree "$pid"
wait "$bg" 2>/dev/null
eq "a hard-killed baton leaves its worktree registered" "$((before + 1))" "$(worktrees)"
eq "and git has nothing to prune" "" "$(git -C "$B" worktree prune --dry-run 2>&1)"
check "and its directory" test -d "$W/scratch/$home/tree"
out=$(bisect plant -- "${CHECK[@]}")
contains "the next bisect says it removed it" "$out" "removed the scratch worktree"
contains "and still finds its turn" "$out" "plant@4  "
eq "the stale worktree is no longer registered" "$before" "$(worktrees)"
eq "and its directory is gone" "" "$(ls -A "$W/scratch")"

# --------------------------------------------------------------------------------------------------
echo
echo "checks: $pass passed, $fail failed"
[[ $fail == 0 ]]
