#!/usr/bin/env bash
# End-to-end run of the parts of baton that only exist on unix, against the real release binary: the socket
# at mode 0600 in a 0700 directory, where `XDG_RUNTIME_DIR` and `TMPDIR` put it, a `BATON_HOME` too deep for
# `sun_path`, the daemon detached by `setsid`, SIGTERM and SIGKILL, inotify, and the inotify watch limit.
#
#   e2e/unix.sh                     uses target/release/baton, building it when missing
#   BATON_BIN=/path/baton e2e/unix.sh
#
# The other suites set BATON_HOME, which short-circuits the XDG lookup; this one leaves it unset and points
# HOME, the XDG variables and TMPDIR into one temp dir instead, so the real data and config dirs stay
# untouched. Linux and macOS only (it exits 0 on Windows). Needs bash, git, ps, stat. The watch-limit section
# needs `unshare --user` and is skipped without it.

set -u
export LC_ALL=C

case ${OSTYPE:-} in
  msys* | cygwin*)
    echo "e2e/unix.sh: unix only, skipped on Windows"
    exit 0
    ;;
esac

root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
BIN=${BATON_BIN:-$root/target/release/baton}
if [[ ! -x $BIN ]]; then
  (cd "$root" && cargo build --release -q) || { echo "cargo build --release failed" >&2; exit 2; }
fi

REAL_HOME=$HOME
TMP=$(mktemp -d)
UID_NOW=$(id -u)
export HOME="$TMP/home"
export GIT_CONFIG_GLOBAL="$TMP/gitconfig"
export GIT_CONFIG_NOSYSTEM=1
export GIT_OPTIONAL_LOCKS=0
mkdir -p "$HOME"
git config --file "$GIT_CONFIG_GLOBAL" user.name e2e
git config --file "$GIT_CONFIG_GLOBAL" user.email e2e@localhost
git config --file "$GIT_CONFIG_GLOBAL" init.defaultBranch main
unset BATON_HOME XDG_DATA_HOME XDG_CONFIG_HOME XDG_STATE_HOME
export XDG_RUNTIME_DIR="$TMP/run"
export TMPDIR="$TMP/tmp"
mkdir -p "$XDG_RUNTIME_DIR" "$TMPDIR"
chmod 700 "$XDG_RUNTIME_DIR"

pass=0
fail=0
current=""
declare -a REPORT=()
declare -A SECTION_MS=()
section_start=0

now_ms() { local t=${EPOCHREALTIME:-}; if [[ -n $t ]]; then echo $((${t/./} / 1000)); else echo $(($(date +%s) * 1000)); fi; }

section() {
  current=$1
  section_start=$(now_ms)
  echo
  echo "== $1"
}
section_end() { SECTION_MS[$current]=$(($(now_ms) - section_start)); }
ok() { pass=$((pass + 1)); echo "  ok   $1"; REPORT+=("PASS  $current: $1"); }
bad() { fail=$((fail + 1)); echo "  FAIL $1${2:+  ($2)}"; REPORT+=("FAIL  $current: $1${2:+  ($2)}"); }
check() { local name=$1; shift; if "$@" >/dev/null 2>&1; then ok "$name"; else bad "$name"; fi; }
eq() { if [[ $2 == "$3" ]]; then ok "$1"; else bad "$1" "expected [$2], got [$3]"; fi; }
contains() { if [[ $2 == *"$3"* ]]; then ok "$1"; else bad "$1" "missing [$3] in [$2]"; fi; }

mode_of() { stat -c %a "$1" 2>/dev/null || stat -f %Lp "$1" 2>/dev/null; }
daemon_pid() { "$BIN" status 2>/dev/null | sed -n 's/^daemon pid \([0-9]*\).*/\1/p'; }
alive() { kill -0 "$1" 2>/dev/null; }

# Polls every 20 ms: passes when the command succeeds within the timeout.
await() { # await "name" timeout_ms command...
  local name=$1 ms=$2 deadline
  deadline=$(($(now_ms) + ms))
  shift 2
  while :; do
    if "$@" >/dev/null 2>&1; then ok "$name"; return 0; fi
    if (($(now_ms) > deadline)); then bad "$name" "not within ${ms} ms"; return 1; fi
    sleep 0.02
  done
}

# Every daemon this run started, under whatever environment it was started in.
cleanup() {
  local pid
  pid=$(daemon_pid)
  [[ -n $pid ]] && kill -9 "$pid" 2>/dev/null
  for home in "${HOMES[@]:-}"; do
    [[ -n $home ]] || continue
    pid=$(BATON_HOME=$home daemon_pid)
    [[ -n $pid ]] && kill -9 "$pid" 2>/dev/null
  done
  sleep 0.2
  rm -rf "$TMP"
}
HOMES=()
trap cleanup EXIT
trap 'exit 130' INT TERM HUP

REPO="$TMP/repo"
git init -q "$REPO"
git -C "$REPO" remote add origin "https://github.com/E2E/unixbox.git"
printf 'one\n' >"$REPO/a.txt"
git -C "$REPO" add -A
git -C "$REPO" commit -q -m initial
DATA="$HOME/.local/share/baton"

real_before=$(stat -c %Y "$REAL_HOME/.local/share/baton" 2>/dev/null || stat -f %m "$REAL_HOME/.local/share/baton" 2>/dev/null || echo none)

# --------------------------------------------------------------------------------------------------
section "the socket under XDG_RUNTIME_DIR"
out=$("$BIN" enroll "$REPO" 2>&1)
contains "enroll starts the daemon and enrolls" "$out" "enrolled"
SOCK="$XDG_RUNTIME_DIR/baton/daemon.sock"
check "the socket is at \$XDG_RUNTIME_DIR/baton/daemon.sock" test -S "$SOCK"
eq "the socket is mode 0600" 600 "$(mode_of "$SOCK")"
eq "its directory is mode 0700" 700 "$(mode_of "$XDG_RUNTIME_DIR/baton")"
check "the data dir follows HOME" test -f "$DATA/daemon.lock"
PID=$(daemon_pid)
check "status reports a live daemon pid" alive "${PID:-0}"
section_end

section "the daemon is detached by setsid"
SID=$(ps -o sid= -p "$PID" 2>/dev/null | tr -d ' ')
PGID=$(ps -o pgid= -p "$PID" 2>/dev/null | tr -d ' ')
PPID_D=$(ps -o ppid= -p "$PID" 2>/dev/null | tr -d ' ')
eq "it leads its own session" "$PID" "$SID"
eq "it leads its own process group" "$PID" "$PGID"
if [[ $SID != "$(ps -o sid= -p $$ | tr -d ' ')" ]]; then ok "it is not in this shell's session"; else bad "it is in this shell's session"; fi
if [[ $PPID_D != "$$" ]]; then ok "this shell is no longer its parent"; else bad "its parent is this shell"; fi
tty=$(ps -o tty= -p "$PID" | tr -d ' ')
if [[ $tty == "?" || $tty == "??" ]]; then ok "it has no controlling terminal"; else bad "it has a controlling terminal" "$tty"; fi
if [[ -d /proc/$PID/fd ]]; then
  eq "stdin, stdout and stderr are /dev/null" "/dev/null /dev/null /dev/null" \
    "$(readlink /proc/$PID/fd/0) $(readlink /proc/$PID/fd/1) $(readlink /proc/$PID/fd/2)"
fi
section_end

section "inotify watches the tree and a turn follows a write"
if [[ -d /proc/$PID/fd ]]; then
  check "the daemon holds an inotify instance" bash -c "readlink /proc/$PID/fd/* 2>/dev/null | grep -q inotify"
fi
SIDE="$DATA/projects/github.com+e2e+unixbox.git"
printf 'two\n' >>"$REPO/a.txt"
printf '{"session_id":"x1","cwd":"%s","hook_event_name":"Stop"}' "$REPO" >"$TMP/payload.json"
"$BIN" hook claude <"$TMP/payload.json"
eq "the hook exits 0" 0 $?
await "the hook's turn is recorded through the socket" 5000 test -f "$SIDE/refs/baton/x1"
log=$(cd "$REPO" && "$BIN" log x1 2>&1)
contains "the turn lists the changed path" "$log" "a.txt"
section_end

section "SIGTERM stops the daemon and removes its socket"
kill -TERM "$PID"
await "the daemon exits within 3 s" 3000 bash -c "! kill -0 $PID 2>/dev/null"
check "the socket file is gone" test ! -e "$SOCK"
out=$("$BIN" status 2>&1)
contains "the next command starts a new daemon" "$out" "daemon pid"
PID2=$(daemon_pid)
if [[ -n $PID2 && $PID2 != "$PID" ]]; then ok "with a new pid"; else bad "the pid did not change" "$PID -> $PID2"; fi
contains "and it restored the enrolled worktree" "$out" "$REPO"
section_end

section "SIGKILL leaves a stale socket that the next daemon replaces"
kill -KILL "$PID2"
await "the daemon is gone" 3000 bash -c "! kill -0 $PID2 2>/dev/null"
check "the stale socket file is still there" test -S "$SOCK"
out=$("$BIN" status 2>&1)
contains "the next command starts a daemon anyway" "$out" "daemon pid"
eq "and the socket is mode 0600 again" 600 "$(mode_of "$SOCK")"
section_end

section "a permissive umask does not widen the socket"
kill -TERM "$(daemon_pid)"
await "the daemon is gone" 3000 test ! -e "$SOCK"
rm -rf "$XDG_RUNTIME_DIR/baton"
(umask 000; "$BIN" status >/dev/null 2>&1)
eq "the socket is mode 0600 under umask 000" 600 "$(mode_of "$SOCK")"
eq "the directory is mode 0700 under umask 000" 700 "$(mode_of "$XDG_RUNTIME_DIR/baton")"
section_end

section "without XDG_RUNTIME_DIR the socket is in the temp dir"
kill -TERM "$(daemon_pid)"
await "the daemon is gone" 3000 test ! -e "$SOCK"
FALLBACK="$TMPDIR/baton-$UID_NOW"
(
  unset XDG_RUNTIME_DIR
  "$BIN" status >"$TMP/status.out" 2>&1
  pid=$(daemon_pid)
  echo "$pid" >"$TMP/fallback.pid"
)
contains "a daemon starts" "$(cat "$TMP/status.out")" "daemon pid"
check "the socket is at \$TMPDIR/baton-<uid>/daemon.sock" test -S "$FALLBACK/daemon.sock"
eq "it is mode 0600" 600 "$(mode_of "$FALLBACK/daemon.sock")"
eq "its directory is mode 0700" 700 "$(mode_of "$FALLBACK")"
kill -TERM "$(cat "$TMP/fallback.pid")" 2>/dev/null
await "that daemon is gone" 3000 test ! -e "$FALLBACK/daemon.sock"
section_end

section "a BATON_HOME too deep for sun_path gets a short socket"
DEEP="$TMP"
while ((${#DEEP} < 120)); do DEEP="$DEEP/deeper-than-a-socket-path-allows"; done
DEEP="$DEEP/home"
mkdir -p "$DEEP"
HOMES+=("$DEEP")
if ((${#DEEP} + 17 > 108)); then ok "the plain path ($((${#DEEP} + 17)) bytes) would not fit sun_path"; else bad "the test home is not deep enough"; fi
out=$(BATON_HOME="$DEEP" "$BIN" enroll "$REPO" 2>&1)
contains "the daemon starts and enroll succeeds" "$out" "enrolled"
check "no socket file sits in the deep BATON_HOME" test ! -e "$DEEP/data/daemon.sock"
shortsock=$(find "$XDG_RUNTIME_DIR/baton" -name '*.sock' | head -1)
check "a short socket is in the runtime dir" test -S "${shortsock:-/nonexistent}"
eq "it is mode 0600" 600 "$(mode_of "${shortsock:-/nonexistent}")"
DEEP_PID=$(BATON_HOME="$DEEP" daemon_pid)
check "the deep home has its own live daemon" alive "${DEEP_PID:-0}"
DEEP2="$TMP/second"
for _ in 1 2 3 4 5 6; do DEEP2="$DEEP2/deeper-than-a-socket-path-allows"; done
mkdir -p "$DEEP2"
HOMES+=("$DEEP2")
BATON_HOME="$DEEP2" "$BIN" status >/dev/null 2>&1
DEEP2_PID=$(BATON_HOME="$DEEP2" daemon_pid)
if [[ -n $DEEP2_PID && $DEEP2_PID != "${DEEP_PID:-}" ]]; then ok "a second deep home gets a second daemon"; else bad "two deep homes share a daemon" "$DEEP_PID $DEEP2_PID"; fi
section_end

section "the inotify watch limit is named"
if command -v unshare >/dev/null 2>&1 && unshare --user --map-root-user true 2>/dev/null &&
  unshare --user --map-root-user bash -c 'test -w /proc/sys/user/max_inotify_watches' 2>/dev/null; then
  LIM="$TMP/limited"
  mkdir -p "$LIM/home" "$LIM/run" "$LIM/repo"
  chmod 700 "$LIM/run"
  git init -q "$LIM/repo"
  git -C "$LIM/repo" remote add origin "https://github.com/E2E/limited.git"
  for i in $(seq 1 40); do mkdir -p "$LIM/repo/d$i"; echo "$i" >"$LIM/repo/d$i/f"; done
  out=$(unshare --user --map-root-user env HOME="$LIM/home" XDG_RUNTIME_DIR="$LIM/run" bash -c '
    echo 15 >/proc/sys/user/max_inotify_watches
    "$1" enroll "$2" 2>&1
    pid=$("$1" status 2>/dev/null | sed -n "s/^daemon pid \([0-9]*\).*/\1/p")
    [[ -n $pid ]] && kill -9 "$pid"
  ' _ "$BIN" "$LIM/repo")
  contains "enroll fails and says which sysctl to raise" "$out" "fs.inotify.max_user_watches"
else
  echo "  skipped: needs unshare --user with a writable /proc/sys/user/max_inotify_watches"
fi
section_end

section "the real data dir was not touched"
real_after=$(stat -c %Y "$REAL_HOME/.local/share/baton" 2>/dev/null || stat -f %m "$REAL_HOME/.local/share/baton" 2>/dev/null || echo none)
eq "the real data dir was not created or modified" "$real_before" "$real_after"
section_end

# --------------------------------------------------------------------------------------------------
echo
echo "== summary"
for line in "${REPORT[@]}"; do [[ $line == FAIL* ]] && echo "$line"; done
for name in "${!SECTION_MS[@]}"; do printf '  %-62s %6s ms\n' "$name" "${SECTION_MS[$name]}"; done | sort
echo "checks: $pass passed, $fail failed"
[[ $fail == 0 ]]
