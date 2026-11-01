#!/usr/bin/env bash
# End-to-end run of enrollment by project key (`[enroll] projects` in the user config) against the real
# release binary, a real daemon and throwaway git repos. Nothing leaves the temp dir; no network.
#
#   e2e/enroll.sh                   uses target/release/baton, building it when missing
#   BATON_BIN=/path/baton e2e/enroll.sh
#
# Needs bash 5 (EPOCHREALTIME), git, sha256sum.

set -u
export LC_ALL=C

root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
exe=""
[[ ${OSTYPE:-} == msys* || ${OSTYPE:-} == cygwin* ]] && exe=".exe"
BIN=${BATON_BIN:-$root/target/release/baton$exe}

if [[ ! -x $BIN ]]; then
  (cd "$root" && cargo build --release -q) || { echo "cargo build --release failed" >&2; exit 2; }
fi
if [[ -z ${EPOCHREALTIME:-} ]]; then
  echo "bash 5 is required (EPOCHREALTIME)" >&2
  exit 2
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
declare -A SECTION_MS=()
current=""
section_start=0
STARTED=$EPOCHREALTIME
LOG="$BATON_HOME/data/daemon.log"
CONFIG="$BATON_HOME/config/config.toml"
mkdir -p "$BATON_HOME/config"

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
check() {
  local name=$1
  shift
  if "$@" >/dev/null 2>&1; then ok "$name"; else bad "$name"; fi
}
eq() { if [[ $2 == "$3" ]]; then ok "$1"; else bad "$1" "expected [$2], got [$3]"; fi; }
contains() { if [[ $2 == *"$3"* ]]; then ok "$1"; else bad "$1" "missing [$3]"; fi; }
lacks() { if [[ $2 != *"$3"* ]]; then ok "$1"; else bad "$1" "found [$3]"; fi; }

sgit() { MSYS_NO_PATHCONV=1 git --git-dir="$SIDE" "$@"; }
side_of() { echo "$BATON_HOME/data/projects/github.com+e2e+$1.git"; }
tree_of() { sgit ls-tree -r --name-only "refs/baton/$1"; }
base_of() { sgit show -s --format=%B "refs/baton/$1" | sed -n 's/^Baton-Base: //p'; }

ref_sha() {
  local sha=""
  { read -r sha <"$1"; } 2>/dev/null
  echo "$sha"
}

wait_ref_change() { # wait_ref_change file old_sha timeout_ms
  local file=$1 old=$2 deadline=$(($(now_us) + $3 * 1000)) sha
  while :; do
    sha=""
    { read -r sha <"$file"; } 2>/dev/null
    [[ -n $sha && $sha != "$old" ]] && return 0
    (($(now_us) > deadline)) && return 1
    sleep 0.05
  done
}

payload() { # payload file cwd session
  printf '{"session_id":"%s","cwd":"%s","hook_event_name":"Stop"}' "$3" "$(wp "$2")" >"$1"
}

# A hook whose turn lands: refs/baton/<session> moves in the side repo SIDE.
turn_lands() { # turn_lands cwd session
  local old
  old=$(ref_sha "$SIDE/refs/baton/$2")
  payload "$TMP/payload.json" "$1" "$2"
  "$BIN" hook claude <"$TMP/payload.json"
  wait_ref_change "$SIDE/refs/baton/$2" "$old" 15000
}

log_count() { grep -cF -- "$1" "$LOG" 2>/dev/null || true; }
wait_log() { # wait_log text timeout_ms
  local deadline=$(($(now_us) + $2 * 1000))
  while (($(log_count "$1") == 0)); do
    (($(now_us) > deadline)) && return 1
    sleep 0.1
  done
}

# A hook that is dropped: the daemon says it ignored the turn from that directory.
turn_ignored() { # turn_ignored cwd session
  local needle before deadline
  needle="ignored a turn from $(wp "$1")"
  before=$(log_count "$needle")
  payload "$TMP/payload.json" "$1" "$2"
  "$BIN" hook claude <"$TMP/payload.json"
  deadline=$(($(now_us) + 10000000))
  while (($(log_count "$needle") <= before)); do
    (($(now_us) > deadline)) && return 1
    sleep 0.05
  done
}

mkrepo() { # mkrepo dir name : one commit, origin https://github.com/E2E/<name>.git
  git init -q "$1"
  git -C "$1" remote add origin "https://github.com/E2E/$2.git"
  printf 'a\n' >"$1/a.txt"
  git -C "$1" add -A >/dev/null 2>&1
  git -C "$1" commit -q -m initial
}

linked() { # linked repo dir branch file : a linked worktree with its own commit and an untracked file
  git -C "$1" worktree add -q -b "$3" "$2"
  printf '%s\n' "$3" >"$2/branch-$3.txt"
  git -C "$2" add -A >/dev/null 2>&1
  git -C "$2" commit -q -m "branch $3"
  printf '%s\n' "$3" >"$2/$4"
}

set_config() { printf '%s\n' "$@" >"$CONFIG"; }
status() { "$BIN" status 2>&1 | tr '\' '/'; } # paths with forward slashes on every platform

# --------------------------------------------------------------------------------------------------
section "no list: a repository is not recorded, so the daemon has nothing to enroll by"
L="$TMP/listed"
mkrepo "$L" listed
linked "$L" "$TMP/l-wt1" b1 one.txt
SIDE=$(side_of listed)
out=$("$BIN" status 2>&1) # starts the daemon; a hook never does
PID0=$(daemon_pid)
lacks "status names no enroll projects" "$out" "enroll projects"
if turn_ignored "$TMP/l-wt1" s1; then ok "a hook from a linked worktree is ignored"; else bad "the hook was not reported ignored"; fi
check "no side repo was made" test ! -d "$SIDE"
section_end

# --------------------------------------------------------------------------------------------------
section "a config edit adds a key, and the next hook enrolls the repository with no restart"
set_config '[enroll]' 'projects = ["GitHub.com/E2E/listed ", "not a key!", "github.com/e2e/renamed", "github.com/e2e/never-cloned"]'
HEAD_1=$(git -C "$TMP/l-wt1" rev-parse HEAD)
if turn_lands "$TMP/l-wt1" s1; then ok "the first hook from a linked worktree is enrolled and its turn is stored"; else bad "the listed repository's first hook was dropped"; fi
t=$(tree_of s1)
contains "the turn holds the untracked file" "$t" "one.txt"
contains "and the branch's committed file" "$t" "branch-b1.txt"
eq "its Baton-Base is the linked worktree's HEAD" "$HEAD_1" "$(base_of s1)"
eq "the daemon was not restarted" "$PID0" "$(daemon_pid)"
log=$(cat "$LOG")
contains "the log says it enrolled through the list" "$log" "as github.com/e2e/listed: it is in [enroll] projects"
contains "an entry that is no project key is skipped with a log line" "$log" 'enroll list: skipped "not a key!"'
contains "the log counts the three valid entries" "$log" "enroll list: 3 projects"
out=$(status)
contains "status names the configured projects, lowercased and trimmed" "$out" "enroll projects: github.com/e2e/listed, github.com/e2e/renamed, github.com/e2e/never-cloned"
contains "status marks the repository as enrolled through the list" "$out" "through [enroll] projects (github.com/e2e/listed)"

linked "$L" "$TMP/l-wt2" b2 two.txt
if turn_lands "$TMP/l-wt2" s2; then ok "a second linked worktree is recorded from its first turn"; else bad "second worktree dropped"; fi
contains "its turn holds its own file" "$(tree_of s2)" "two.txt"
lacks "and not the first worktree's" "$(tree_of s2)" "one.txt"
printf 'main\n' >"$L/m.txt"
if turn_lands "$L" s3; then ok "the main worktree is recorded too"; else bad "main worktree dropped"; fi
linked "$L" "$TMP/l-wt3" b3 three.txt
out=$(cd "$TMP/l-wt3" && "$BIN" turn --thread s4 --message "from baton turn" 2>&1)
contains "baton turn from a new worktree enrolls it the same way" "$out" "s4@1"
contains "and stores its file" "$(tree_of s4)" "three.txt"
section_end

# --------------------------------------------------------------------------------------------------
section "an unlisted repository is not enrolled, and the answer is not worked out again for each turn"
O="$TMP/other"
mkrepo "$O" other
linked "$O" "$TMP/o-wt1" ob1 o1.txt
SIDE_O=$(side_of other)
for i in 1 2 3 4 5; do turn_ignored "$TMP/o-wt1" "o$i" || bad "hook $i from the unlisted repository was not reported ignored"; done
turn_ignored "$O" om
check "no side repo was made for it" test ! -d "$SIDE_O"
eq "its project key was worked out once for six hooks" 1 "$(log_count '(key github.com/e2e/other)')"
section_end

# --------------------------------------------------------------------------------------------------
section "a repo config cannot enroll or list anything"
E="$TMP/evil"
mkrepo "$E" evil
mkdir -p "$E/.baton"
printf '[enroll]\nprojects = ["github.com/e2e/evil"]\n' >"$E/.baton/config.toml"
git -C "$E" add -A >/dev/null 2>&1
git -C "$E" commit -q -m "baton config"
linked "$E" "$TMP/e-wt1" eb1 e1.txt
turn_ignored "$TMP/e-wt1" e1
check "a repo that lists itself is not recorded" test ! -d "$(side_of evil)"
lacks "and status does not list its key" "$(status)" "github.com/e2e/evil"

V="$TMP/viaconfig"
mkrepo "$V" unrelated
mkdir -p "$V/.baton"
printf 'project = "github.com/e2e/listed"\n\n[enroll]\nprojects = ["github.com/e2e/evil"]\n' >"$V/.baton/config.toml"
git -C "$V" add -A >/dev/null 2>&1
git -C "$V" commit -q -m "baton config"
SIDE=$(side_of listed)
linked "$V" "$TMP/v-wt1" vb1 v1.txt
if turn_lands "$TMP/v-wt1" sv1; then ok "a project set in .baton/config.toml counts as the repo's key"; else bad "a listed project from .baton/config.toml was not enrolled"; fi
contains "its [enroll] table is ignored with a notice" "$(cat "$LOG")" ".baton/config.toml sets [enroll], which only the user config may; ignored"
lacks "and does not add to the list" "$(status)" "github.com/e2e/evil"
section_end

# --------------------------------------------------------------------------------------------------
section "a remote that changes is handled where the repository next meets a worktree"
SIDE=$(side_of renamed)
git -C "$O" remote set-url origin https://github.com/E2E/renamed.git
if turn_lands "$TMP/o-wt1" r1; then ok "an unlisted repository whose remote now names a listed project is enrolled at once"; else bad "the changed remote was not noticed"; fi
contains "its status line names the project it matched" "$(status)" "(github.com/e2e/renamed)"

git -C "$O" remote rename origin upstream >/dev/null 2>&1
linked "$O" "$TMP/o-wt2" ob2 o2.txt
if turn_lands "$TMP/o-wt2" r2; then ok "renaming the only remote keeps the key, so a new worktree is still recorded"; else bad "worktree dropped after the remote was renamed"; fi

git -C "$O" remote set-url upstream https://github.com/E2E/elsewhere.git
linked "$O" "$TMP/o-wt3" ob3 o3.txt
turn_ignored "$TMP/o-wt3" r3
check "a new worktree of a repository whose project is no longer listed is not recorded" test ! -e "$SIDE/refs/baton/r3"
contains "the daemon logs that the repository left the list" "$(cat "$LOG")" "left [enroll] projects (its project is now github.com/e2e/elsewhere)"
lacks "status no longer lists the repository" "$(status)" "other/.git"
turn_ignored "$TMP/o-wt1" r4
check "its earlier worktree stopped too" test ! -e "$SIDE/refs/baton/r4"

git -C "$O" remote set-url upstream https://github.com/E2E/renamed.git
if turn_lands "$TMP/o-wt3" r5; then ok "putting the remote back enrolls it again at once"; else bad "the restored remote was not noticed"; fi
section_end

# --------------------------------------------------------------------------------------------------
section "removing a project from the list stops its repositories without a restart"
set_config '[enroll]' 'projects = ["github.com/e2e/listed"]'
deadline=$(($(now_us) + 15000000))
while [[ $(status) == *"(github.com/e2e/renamed)"* ]] && (($(now_us) < deadline)); do sleep 0.3; done
lacks "status no longer lists the repository of the removed project" "$(status)" "(github.com/e2e/renamed)"
contains "the log says why" "$(cat "$LOG")" "left [enroll] projects (the list no longer names github.com/e2e/renamed)"
turn_ignored "$TMP/o-wt3" r6
check "a hook from it is ignored" test ! -e "$SIDE/refs/baton/r6"
SIDE=$(side_of listed)
printf 'again\n' >"$TMP/l-wt1/again.txt"
if turn_lands "$TMP/l-wt1" s1; then ok "a repository still on the list keeps recording"; else bad "listed repository stopped"; fi
section_end

# --------------------------------------------------------------------------------------------------
section "the list is read when the daemon starts, and the watch cap still applies"
set_config '[watch]' 'max_repo_worktrees = 2' '' '[enroll]' 'projects = ["github.com/e2e/listed", "github.com/e2e/capped"]'
kill_pid "$(daemon_pid)"
sleep 0.5
"$BIN" status >/dev/null
C="$TMP/capped"
mkrepo "$C" capped
SIDE=$(side_of capped)
for n in 1 2 3; do
  linked "$C" "$TMP/c-wt$n" cb$n c$n.txt
  if turn_lands "$TMP/c-wt$n" c$n; then ok "worktree $n of a listed repository is recorded"; else bad "capped worktree $n dropped"; fi
done
out=$(status)
via=$(grep -c ' via ' <<<"$out")
check "never more than the cap of two are watched" test "$via" -le 2
contains "status reports the cap" "$out" "at most 2"
printf 'late\n' >"$TMP/c-wt1/late.txt"
if turn_lands "$TMP/c-wt1" c1; then ok "an unwatched worktree is recorded again at its next turn"; else bad "evicted worktree dropped"; fi
section_end

# --------------------------------------------------------------------------------------------------
section "unenroll --repo sticks for a repository the list brought in, across a daemon restart"
SIDE=$(side_of listed)
printf 'live\n' >"$TMP/l-wt1/live.txt"
turn_lands "$TMP/l-wt1" u0 >/dev/null || bad "the listed repository was not recording before the unenroll"
out=$(cd "$L" && "$BIN" unenroll --repo 2>&1)
contains "unenroll --repo says it stopped the repository" "$out" "unenrolled repository"
contains "and that it is opted out" "$out" "opted out"
contains "and how to undo it" "$out" "baton enroll --repo"
if turn_ignored "$TMP/l-wt1" u1; then ok "a hook from the worktree it was recording is now ignored"; else bad "hook not ignored after unenroll --repo"; fi
if turn_ignored "$TMP/l-wt2" u2; then ok "and one from another worktree"; else bad "hook from another worktree not ignored"; fi
out=$(status)
contains "status names the opt-out" "$out" "opted out of [enroll] projects: "
lacks "and no longer lists the repository as enrolled through the list" "$out" "through [enroll] projects (github.com/e2e/listed)"
contains "the project is still on the list" "$out" "github.com/e2e/listed"

kill_pid "$(daemon_pid)"
sleep 0.5
"$BIN" status >/dev/null
if turn_ignored "$TMP/l-wt1" u3; then ok "after a daemon restart the repository is still not enrolled"; else bad "the opt-out did not survive a restart"; fi
contains "and status still names the opt-out" "$(status)" "opted out of [enroll] projects: "
check "nothing was recorded for it" test ! -e "$SIDE/refs/baton/u1"
check "nothing from the second worktree either" test ! -e "$SIDE/refs/baton/u2"

set_config '[watch]' 'max_repo_worktrees = 2' '' '[enroll]' 'projects = ["github.com/e2e/listed", "github.com/e2e/capped", "github.com/e2e/pre"]'
turn_ignored "$TMP/l-wt2" u4
check "editing the list does not undo it" test ! -e "$SIDE/refs/baton/u4"

out=$(cd "$L" && "$BIN" enroll --repo 2>&1)
contains "enroll --repo enrolls it again" "$out" "enrolled repository"
if turn_lands "$TMP/l-wt1" u5; then ok "its hooks are recorded again"; else bad "hook dropped after enroll --repo"; fi
lacks "status no longer names the opt-out" "$(status)" "opted out of [enroll] projects: "
kill_pid "$(daemon_pid)"
sleep 0.5
contains "after a restart the repository is enrolled with no turn, since enroll --repo saves it" "$(status)" "listed/.git"

out=$(cd "$L" && "$BIN" unenroll --repo 2>&1)
contains "unenrolling a repository the person enrolled, whose project is listed, is also an opt-out" "$out" "opted out"
turn_ignored "$TMP/l-wt1" u6
check "and its next hook is ignored" test ! -e "$SIDE/refs/baton/u6"
(cd "$L" && "$BIN" enroll --repo >/dev/null 2>&1)
section_end

# --------------------------------------------------------------------------------------------------
section "opt-outs only where they mean something"
out=$(cd "$O" && "$BIN" unenroll --repo 2>&1)
contains "an unlisted repository is just not enrolled" "$out" "was not enrolled"
lacks "and nothing is opted out" "$out" "opted out"

P="$TMP/pre"
mkrepo "$P" pre
linked "$P" "$TMP/p-wt1" pb1 p1.txt
out=$(cd "$P" && "$BIN" unenroll --repo 2>&1)
contains "a listed repository that was never enrolled can be opted out beforehand" "$out" "opted out"
turn_ignored "$TMP/p-wt1" p1
check "and its first hook is ignored" test ! -d "$(side_of pre)"
(cd "$P" && "$BIN" enroll --repo >/dev/null 2>&1)
SIDE=$(side_of pre)
if turn_lands "$TMP/p-wt1" p2; then ok "enroll --repo undoes it"; else bad "pre-opted-out repository not recorded after enroll --repo"; fi

SIDE=$(side_of capped)
turn_lands "$TMP/c-wt2" c4 >/dev/null || bad "the capped repository was not recording"
out=$("$BIN" unenroll "$(wp "$TMP/c-wt2")" 2>&1)
contains "plain unenroll of a worktree a repository brought in says it returns" "$out" "next turn enrolls it again"
if turn_lands "$TMP/c-wt2" c5; then ok "and its next turn is recorded"; else bad "worktree did not come back after plain unenroll"; fi
section_end

# --------------------------------------------------------------------------------------------------
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
