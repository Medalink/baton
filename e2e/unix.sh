#!/usr/bin/env bash
# End-to-end run of the parts of baton that only exist on unix, against the real release binary: the socket
# at mode 0600 in the data dir (0700), the same socket whatever `XDG_RUNTIME_DIR`, `TMPDIR` or spelling of the
# home a process has, a `BATON_HOME` too deep for `sun_path` and its fixed /tmp socket, a poisoned /tmp
# directory, the daemon detached by `setsid`, SIGTERM and SIGKILL, inotify, and the inotify watch limit.
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
  for sock in "${SHORT_SOCKS[@]:-}"; do [[ -n $sock ]] && rm -f "$sock"; done
  if ((short_existed == 0)); then
    [[ -L $SHORT_DIR && $(readlink "$SHORT_DIR") == "$TMP/elsewhere" ]] && rm -f "$SHORT_DIR"
    [[ -d $SHORT_DIR && ! -L $SHORT_DIR ]] && rmdir "$SHORT_DIR" 2>/dev/null
  fi
  rm -rf "$TMP"
}
HOMES=()
SHORT_SOCKS=()
short_existed=1
SHORT_DIR=/tmp/baton-$UID_NOW
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
section "the socket in the data dir"
out=$("$BIN" enroll "$REPO" 2>&1)
contains "enroll starts the daemon and enrolls" "$out" "enrolled"
SOCK="$DATA/daemon.sock"
check "the socket is at <data dir>/daemon.sock" test -S "$SOCK"
eq "the socket is mode 0600" 600 "$(mode_of "$SOCK")"
eq "the data dir is mode 0700" 700 "$(mode_of "$DATA")"
check "the lock sits beside it" test -f "$DATA/daemon.lock"
check "nothing is made under XDG_RUNTIME_DIR" test ! -e "$XDG_RUNTIME_DIR/baton"
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
first=$(<"$SIDE/refs/baton/x1")
printf 'three
' >>"$REPO/a.txt"
"$BIN" hook claude <"$TMP/payload.json"
await "a write made after the first turn moves the ref again" 5000 bash -c '[[ $(<"$1") != "$2" ]]' _ "$SIDE/refs/baton/x1" "$first"
log=$(cd "$REPO" && "$BIN" log x1 2>&1)
contains "the second turn lists the changed path" "$log" "M a.txt"
section_end

section "SIGTERM stops the daemon and removes its socket"
kill -TERM "$PID"
await "the daemon exits within 3 s" 3000 bash -c "! kill -0 $PID 2>/dev/null"
check "the socket file is gone" test ! -e "$SOCK"
rm -f "$SOCK"
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
PID3=$(daemon_pid)
kill -TERM "$PID3"
await "the daemon is gone" 3000 bash -c "! kill -0 $PID3 2>/dev/null"
rm -f "$SOCK"
chmod 755 "$DATA"
(umask 000; "$BIN" status >/dev/null 2>&1)
eq "the socket is mode 0600 under umask 000" 600 "$(mode_of "$SOCK")"
eq "the data dir is mode 0700 again" 700 "$(mode_of "$DATA")"
section_end

section "XDG_RUNTIME_DIR and TMPDIR do not move the socket"
PID4=$(daemon_pid)
check "a daemon is up" alive "${PID4:-0}"
other=$(XDG_RUNTIME_DIR="$TMP/other-run" TMPDIR="$TMP/other-tmp" daemon_pid)
eq "another XDG_RUNTIME_DIR and TMPDIR reach the same daemon" "$PID4" "$other"
other=$(env -u XDG_RUNTIME_DIR -u TMPDIR "$BIN" status 2>&1 | sed -n 's/^daemon pid \([0-9]*\).*/\1/p')
eq "no XDG_RUNTIME_DIR or TMPDIR reaches the same daemon" "$PID4" "$other"
longrun="$TMP/$(printf 'r%.0s' $(seq 1 150))"
other=$(XDG_RUNTIME_DIR="$longrun" daemon_pid)
eq "a 150-byte XDG_RUNTIME_DIR reaches the same daemon" "$PID4" "$other"
check "no socket appeared in those other dirs" test ! -e "$TMP/other-run/baton" -a ! -e "$TMP/other-tmp/baton-$UID_NOW" -a ! -e "$longrun/baton"
section_end

section "a BATON_HOME too deep for sun_path gets a short socket in /tmp"
sha256_of() { { sha256sum 2>/dev/null || shasum -a 256; } | cut -c1-32; }
deep_sock() { printf '/tmp/baton-%s/%s.sock' "$UID_NOW" "$(printf %s "$(cd -P "$1/data" && pwd -P)" | sha256_of)"; }
short_existed=0
[[ -e $SHORT_DIR || -L $SHORT_DIR ]] && short_existed=1
DEEP="$TMP"
while ((${#DEEP} < 120)); do DEEP="$DEEP/deeper-than-a-socket-path-allows"; done
DEEP="$DEEP/home"
mkdir -p "$DEEP/data"
HOMES+=("$DEEP")
if ((${#DEEP} + 17 > 108)); then ok "the plain path ($((${#DEEP} + 17)) bytes) would not fit sun_path"; else bad "the test home is not deep enough"; fi
DEEP_SOCK=$(deep_sock "$DEEP")
SHORT_SOCKS+=("$DEEP_SOCK")
out=$(BATON_HOME="$DEEP" "$BIN" enroll "$REPO" 2>&1)
contains "the daemon starts and enroll succeeds" "$out" "enrolled"
check "no socket file sits in the deep BATON_HOME" test ! -e "$DEEP/data/daemon.sock"
check "the socket is /tmp/baton-<uid>/<32 hex of SHA-256 of the data dir>.sock" test -S "$DEEP_SOCK"
eq "it is mode 0600" 600 "$(mode_of "$DEEP_SOCK")"
eq "its directory is mode 0700" 700 "$(mode_of "$SHORT_DIR")"
DEEP_PID=$(BATON_HOME="$DEEP" daemon_pid)
check "the deep home has its own live daemon" alive "${DEEP_PID:-0}"
ln -sfn "$DEEP" "$TMP/deep-link"
for spelling in "$DEEP/" "$TMP/deep-link" "$DEEP/../home/"; do
  other=$(BATON_HOME="$spelling" XDG_RUNTIME_DIR="$TMP/other-run" TMPDIR="$TMP/other-tmp" daemon_pid)
  eq "BATON_HOME spelled [${spelling#"$TMP"/}] under other XDG_RUNTIME_DIR and TMPDIR reaches that daemon" "$DEEP_PID" "$other"
done
other=$(BATON_HOME="$DEEP" XDG_RUNTIME_DIR="$longrun" daemon_pid)
eq "a 150-byte XDG_RUNTIME_DIR reaches it too" "$DEEP_PID" "$other"
DEEP2="$TMP/second"
for _ in 1 2 3 4 5 6; do DEEP2="$DEEP2/deeper-than-a-socket-path-allows"; done
mkdir -p "$DEEP2/data"
HOMES+=("$DEEP2")
SHORT_SOCKS+=("$(deep_sock "$DEEP2")")
BATON_HOME="$DEEP2" "$BIN" status >/dev/null 2>&1
DEEP2_PID=$(BATON_HOME="$DEEP2" daemon_pid)
if [[ -n $DEEP2_PID && $DEEP2_PID != "${DEEP_PID:-}" ]]; then ok "a second deep home gets a second daemon"; else bad "two deep homes share a daemon" "$DEEP_PID $DEEP2_PID"; fi
# Two homes whose FNV-1a, cut to 32 bits, is the same: the old name made them share one socket.
if command -v python3 >/dev/null 2>&1; then
  pair=$(python3 -I "$root/e2e/fnv32_pair.py" "$TMP/col/$(printf 'p%.0s' $(seq 1 70))/")
  C1=$(sed -n 1p <<<"$pair")
  C2=$(sed -n 2p <<<"$pair")
  if [[ -n $C1 && -n $C2 ]]; then
    mkdir -p "$C1/data" "$C2/data"
    HOMES+=("$C1" "$C2")
    SHORT_SOCKS+=("$(deep_sock "$C1")" "$(deep_sock "$C2")")
    BATON_HOME="$C1" "$BIN" status >/dev/null 2>&1
    BATON_HOME="$C2" "$BIN" status >/dev/null 2>&1
    p1=$(BATON_HOME="$C1" daemon_pid)
    p2=$(BATON_HOME="$C2" daemon_pid)
    if [[ -n $p1 && -n $p2 && $p1 != "$p2" ]]; then ok "two homes with the same 32-bit FNV-1a get their own daemons"; else bad "colliding homes share a daemon" "$p1 $p2"; fi
    check "and each has its own daemon.log" test -s "$C1/data/daemon.log" -a -s "$C2/data/daemon.log"
  else
    echo "  skipped: no colliding pair found"
  fi
else
  echo "  skipped: the FNV collision needs python3"
fi
section_end

section "a bad /tmp/baton-<uid> is refused with a message"
# Only when this run made the directory: another run's sockets there must not be disturbed.
if ((short_existed == 0)); then
  for h in "${HOMES[@]:-}"; do
    pid=$(BATON_HOME=$h daemon_pid)
    [[ -n $pid ]] && kill -TERM "$pid" 2>/dev/null
  done
  sleep 0.5
  for sock in "${SHORT_SOCKS[@]:-}"; do [[ -n $sock ]] && rm -f "$sock"; done
  rmdir "$SHORT_DIR" 2>/dev/null
  BAD="$TMP/bad-home"
  for _ in 1 2 3 4 5 6 7; do BAD="$BAD/deeper-than-a-socket-path-allows"; done
  mkdir -p "$BAD/data"
  HOMES+=("$BAD")
  mkdir "$TMP/elsewhere"
  ln -s "$TMP/elsewhere" "$SHORT_DIR"
  start=$(now_ms)
  out=$(BATON_HOME="$BAD" "$BIN" status 2>&1)
  took=$(($(now_ms) - start))
  contains "a symlink there is refused, naming the path" "$out" "$SHORT_DIR is unusable"
  contains "and saying what is wrong" "$out" "not a directory"
  if ((took < 10000)); then ok "in ${took} ms, not after the 20 s wait"; else bad "it took ${took} ms"; fi
  check "nothing was written through the symlink" test -z "$(ls -A "$TMP/elsewhere")"
  rm -f "$SHORT_DIR"
  mkdir -m 755 "$SHORT_DIR"
  out=$(BATON_HOME="$BAD" "$BIN" status 2>&1)
  contains "a group- and world-readable directory is refused" "$out" "group or other can access it"
  rmdir "$SHORT_DIR"
else
  echo "  skipped: $SHORT_DIR existed before this run"
fi
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
