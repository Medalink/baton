#!/usr/bin/env bash
# End-to-end run of sync against the real release binary: one `baton serve` and several daemons on this
# machine, each with its own BATON_HOME and its own throwaway clone, so the wire between them is real.
#
#   e2e/sync.sh                     uses target/release/baton, building it when missing
#   BATON_BIN=/path/baton e2e/sync.sh
#   BATON_E2E_BIG_REPO=/path/to/a/large/repo   the tree copied for the latency run (git archive HEAD); without it
#                                   the latency run is skipped, except on the owner's Windows box
#   BATON_E2E_TURNS=100             turns timed in the latency run
#   BATON_E2E_PORT=47880            the main server; the TLS, command-auth and raw-wire servers take the next three
#   BATON_E2E_BURST=30              threads in the burst
#   BATON_E2E_KILL_ROUNDS=8         times the server is killed mid-push
#
# Everything lives under one temp dir. The big repo is only read through `git archive`. Needs bash 5
# (EPOCHREALTIME), git, tar, sha256sum, awk. The TLS section also needs openssl and is skipped without it.
# The sections that send what no daemon would (hostile trees, floods, stalled readers, a revoked session)
# drive e2e/wire.py, a raw baton.v1 client that needs Python 3.8 or newer and nothing else; without it they
# fail one check instead of passing silently. They take about two minutes, most of it waiting out timeouts.

set -u
export LC_ALL=C

root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
exe=""
[[ ${OSTYPE:-} == msys* || ${OSTYPE:-} == cygwin* ]] && exe=".exe"
BIN=${BATON_BIN:-$root/target/release/baton$exe}
BIG_REPO=${BATON_E2E_BIG_REPO:-}
[[ -z $BIG_REPO && -n $exe ]] && BIG_REPO=E:/Dev/Laravel/iris # the owner's Windows default; elsewhere the latency run is skipped unless set
TURNS=${BATON_E2E_TURNS:-100}
PORT=${BATON_E2E_PORT:-47880}
BURST=${BATON_E2E_BURST:-30}
KILL_ROUNDS=${BATON_E2E_KILL_ROUNDS:-8}
OFFLINE_TURNS=5

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
TMPW=$(wp "$TMP")
# A command that forgets its BATON_HOME lands here, not in the real data dir.
export BATON_HOME="$TMPW/nowhere"
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
declare -a SECTION_ORDER=()
declare -A SECTION_MS=() SECTION_PASS=() SECTION_FAIL=()
declare -A SERVER_PID=() WIRE_SLOW=()
declare -a BG_PIDS=()
current=""
section_start=0
section_pass0=0
section_fail0=0

kill_pid() {
  case ${OSTYPE:-} in
    msys* | cygwin*) taskkill //F //T //PID "$1" >/dev/null 2>&1 ;;
    *) kill -9 "$1" >/dev/null 2>&1 ;;
  esac
}

# The id the OS knows a background job by. Git Bash numbers its own, and taskkill needs the real one.
native_pid() {
  case ${OSTYPE:-} in
    msys* | cygwin*) ps -p "$1" | awk 'NR == 2 {print $4}' ;;
    *) echo "$1" ;;
  esac
}

cleanup() {
  local h pid
  for h in "$TMP"/home-*; do
    [[ -d $h ]] || continue
    pid=$(BATON_HOME="$(wp "$h")" "$BIN" status 2>/dev/null | sed -n 's/^daemon pid \([0-9]*\).*/\1/p')
    [[ -n $pid ]] && kill_pid "$pid"
  done
  for pid in "${SERVER_PID[@]}" "${BG_PIDS[@]}"; do
    [[ -n $pid ]] && kill_pid "$pid"
  done
  sleep 0.5
  if [[ -n ${BATON_E2E_KEEP:-} ]]; then echo "kept $TMP"; return; fi
  for _ in 1 2 3 4 5 6 7 8; do
    rm -rf "$TMP" 2>/dev/null
    [[ -d $TMP ]] || break
    sleep 0.5
  done
}
trap cleanup EXIT
trap 'exit 130' INT TERM HUP

now_us() { local t=$EPOCHREALTIME; echo "${t/./}"; }

summary() {
  echo
  echo "== summary"
  for line in "${REPORT[@]}"; do [[ $line == FAIL* ]] && echo "$line"; done
  for name in "${SECTION_ORDER[@]}"; do
    printf '  %-62s %3s ok %2s failed %7s ms\n' "$name" "${SECTION_PASS[$name]:-0}" "${SECTION_FAIL[$name]:-0}" "${SECTION_MS[$name]:-0}"
  done
  echo "checks: $pass passed, $fail failed"
  echo "burst: ${BURST_SUMMARY:-n/a}"
  echo "latency: ${LAT_SUMMARY:-skipped}"
  [[ -n ${LAT_TABLE:-} ]] && echo "$LAT_TABLE"
  echo "hook floor: ${FLOOR_SUMMARY:-skipped}"
  echo "first upload of the large tree: ${FIRST_UPLOAD_MS:-n/a} ms"
}

# BATON_E2E_UNTIL=n stops after n sections; BATON_E2E_KEEP=1 leaves the temp dir for a post-mortem.
UNTIL=${BATON_E2E_UNTIL:-0}
section_count=0

section() {
  if ((UNTIL > 0 && section_count >= UNTIL)); then
    summary
    exit 0
  fi
  section_count=$((section_count + 1))
  current=$1
  SECTION_ORDER+=("$1")
  section_start=$(now_us)
  section_pass0=$pass
  section_fail0=$fail
  echo
  echo "== $1"
}

section_end() {
  SECTION_MS[$current]=$(((($(now_us)) - section_start) / 1000))
  SECTION_PASS[$current]=$((pass - section_pass0))
  SECTION_FAIL[$current]=$((fail - section_fail0))
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

# Passes when the command succeeds within the timeout. Polls every 20 ms.
await() { # await "name" timeout_ms command...
  local name=$1 ms=$2 deadline
  deadline=$(($(now_us) + ms * 1000))
  shift 2
  while :; do
    if "$@" >/dev/null 2>&1; then ok "$name"; return 0; fi
    if (($(now_us) > deadline)); then bad "$name" "not within ${ms} ms"; return 1; fi
    sleep 0.02
  done
}

# True when the command never succeeds for the whole span: a refusal has to hold, not merely not yet happen.
never() { # never "name" span_ms command...
  local name=$1 deadline
  deadline=$(($(now_us) + $2 * 1000))
  shift 2
  while (($(now_us) < deadline)); do
    if "$@" >/dev/null 2>&1; then bad "$name" "it happened"; return 1; fi
    sleep 0.05
  done
  ok "$name"
}

# MSYS rewrites an argument like `refs/baton/t:.env` as a path list unless told not to.
gitd() { local d=$1; shift; MSYS_NO_PATHCONV=1 git --git-dir="$d" "$@"; }

percentile() { # percentile file p   (values in microseconds, printed in ms)
  sort -n "$1" | awk -v p="$2" '{a[NR]=$1} END {if (NR == 0) {print "n/a"; exit} i=int(p/100*NR+0.999999); if (i<1) i=1; if (i>NR) i=NR; printf "%.2f", a[i]/1000}'
}

ref_sha() { # ref_sha file : the first word of a loose ref file, empty when it is not there
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
  done
}

# --------------------------------------------------------------------------------------------------
# machines, repos, servers

SRV="$TMPW/server"
PROJ=widgets

home_of() { echo "$TMPW/home-$1"; }
repo_of() { echo "$TMP/r$1"; }
side_of() { echo "$TMPW/home-$1/data/projects/github.com+e2e+${2:-$PROJ}.git"; }
srv_repo() { echo "${2:-$SRV}/projects/github.com+e2e+${1:-$PROJ}.git"; }
dlog() { echo "$TMP/home-$1/data/daemon.log"; }

bt() { local n=$1; shift; BATON_HOME=$(home_of "$n") "$BIN" "$@"; } # baton as machine n, from here
btr() { local n=$1; shift; (cd "$(repo_of "$n")" && BATON_HOME=$(home_of "$n") "$BIN" "$@" 2>&1); }
btr_within() { local n=$1 secs=$2; shift 2; (cd "$(repo_of "$n")" && BATON_HOME=$(home_of "$n") timeout "$secs" "$BIN" "$@" 2>&1); } # a follow that does not refuse runs for ever
turn_as() { # turn_as machine thread message [baton turn args...]
  local n=$1 t=$2 m=$3
  shift 3
  btr "$n" turn --thread "$t" --message "$m" "$@"
}

tip_on() { ref_sha "$(side_of "$1" "${3:-}")/refs/baton/$2"; }          # tip_on machine thread [project]
tip_on_server() { ref_sha "$(srv_repo "${2:-}" "${3:-}")/refs/baton/$1"; } # tip_on_server thread [project] [data]

mkbase() { # mkbase dir name : the repo every machine clones
  local dir=$1
  mkdir -p "$dir/src" "$dir/pkg/gen" "$dir/build"
  git init -q "$dir"
  git -C "$dir" remote add origin "https://github.com/E2E/$2.git"
  printf 'one\n' >"$dir/a.txt"
  printf 'two\n' >"$dir/b.txt"
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

clone_for() { # clone_for machine [project] : a clone of the base repo, with the project's origin
  git clone -q "$BASE" "$(repo_of "$1")" >/dev/null 2>&1
  git -C "$(repo_of "$1")" remote set-url origin "https://github.com/E2E/${2:-$PROJ}.git"
}

mkmin() { # mkmin machine project : one committed file
  local dir
  dir=$(repo_of "$1")
  git init -q "$dir"
  git -C "$dir" remote add origin "https://github.com/E2E/$2.git"
  printf 'a\n' >"$dir/a.txt"
  git -C "$dir" add -A >/dev/null 2>&1
  git -C "$dir" commit -q -m initial
}

add_device() { # add_device name [data] : leaves the token in $TMP/token-name
  "$BIN" admin --data "${2:-$SRV}" device add "$1" 2>/dev/null >"$TMP/token-$1"
}

# mk_machine home_name machine_id url token_file [ca_file] : the config the daemon reads at start
mk_machine() {
  local h="$TMP/home-$1"
  mkdir -p "$h/config"
  {
    printf '[machine]\nid = "%s"\n\n[server]\nurl = "%s"\ntoken_file = "%s"\n' "$2" "$3" "$(wp "$4")"
    if [[ -n ${5:-} ]]; then printf 'ca_file = "%s"\n' "$(wp "$5")"; fi
  } >"$h/config/config.toml"
}

server_listening() { local n; n=$(grep -cE 'listening on wss?://' "$1" 2>/dev/null || true); ((${n:-0} > $2)); }

start_server() { # start_server tag data port [serve args...]
  local tag=$1 data=$2 port=$3 log="$TMP/server-$1.log" before
  shift 3
  before=$(grep -cE 'listening on wss?://' "$log" 2>/dev/null || true)
  "$BIN" serve --data "$data" --listen "127.0.0.1:$port" --trace "$@" 2>>"$log" &
  SERVER_PID[$tag]=$(native_pid $!)
  await "server $tag listens on $port" 10000 server_listening "$log" "${before:-0}"
}

stop_server() { # stop_server tag
  local pid=${SERVER_PID[$1]:-}
  [[ -n $pid ]] && kill_pid "$pid"
  SERVER_PID[$1]=""
  sleep 0.3
}

arrived() { # arrived machine thread sha [project]
  [[ -n $3 && $(tip_on "$1" "$2" "${4:-}") == "$3" ]]
}

everywhere() { # everywhere thread sha machine... : the server and every named machine hold sha
  local t=$1 sha=$2 m
  shift 2
  [[ -n $sha && $(tip_on_server "$t") == "$sha" ]] || return 1
  for m in "$@"; do arrived "$m" "$t" "$sha" || return 1; done
}

log_has() { grep -q -- "$2" "$1"; } # log_has file text
server_tip_is() { [[ $(tip_on_server "$1") == "$2" ]]; }
server_tip_not() { [[ $(tip_on_server "$1") != "$2" ]]; }
server_has() { [[ -n $(tip_on_server "$1") ]]; }
ref_nonempty() { [[ -n $(ref_sha "$1") ]]; }

# Every file of the thread tip is in dir with the same bytes, and dir holds no other file.
tree_matches() { # tree_matches dir gitdir ref
  local dir=$1 gd=$2 ref=$3
  gitd "$gd" ls-tree -r "$ref" | awk -F'\t' '{split($1, m, " "); print m[3] "\t" $2}' | sort -t$'\t' -k2 >"$TMP/tree.want"
  cut -f2 "$TMP/tree.want" >"$TMP/tree.names"
  (cd "$dir" && git hash-object --stdin-paths <"$TMP/tree.names") >"$TMP/tree.shas" 2>/dev/null
  paste "$TMP/tree.shas" "$TMP/tree.names" >"$TMP/tree.have"
  (cd "$dir" && find . -type f -not -path './.git/*' -not -name .git | sed 's#^\./##' | sort) >"$TMP/tree.files"
  diff -q "$TMP/tree.want" "$TMP/tree.have" >/dev/null && diff -q "$TMP/tree.names" "$TMP/tree.files" >/dev/null
}

# --------------------------------------------------------------------------------------------------
# The raw wire client: e2e/wire.py prints `ok name` or `bad name :: why` per check, and these turn each line
# into one check here. A python that is only the Windows store stub fails the version probe and is skipped.
WIRE_PY=""
for cand in python python3 py; do
  if command -v "$cand" >/dev/null 2>&1 && "$cand" -c 'import sys; sys.exit(sys.version_info < (3, 8))' >/dev/null 2>&1; then
    WIRE_PY=$cand
    break
  fi
done
WIRE_SCRIPT=$(wp "$root/e2e/wire.py")

wire_cmd() { # wire_cmd port case device [args...] : as the device, whose token file is $TMP/token-device
  local port=$1 c=$2 d=$3
  shift 3
  "$WIRE_PY" -I "$WIRE_SCRIPT" "$c" "$port" "$(wp "$TMP/token-$d")" "$d" "$@" 2>&1
}

wire_lines() { # wire_lines case text
  local c=$1 line rest n=0
  while IFS= read -r line; do
    line=${line%$'\r'}
    case $line in
      "ok "*) ok "${line#ok }"; n=$((n + 1)) ;;
      "bad "*)
        rest=${line#bad }
        n=$((n + 1))
        if [[ $rest == *" :: "* ]]; then bad "${rest%% :: *}" "${rest#* :: }"; else bad "$rest"; fi
        ;;
      "") ;;
      *) bad "wire $c printed [$line]" ;;
    esac
  done <<<"$2"
  ((n > 0)) || bad "wire $c printed no check"
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

BASE="$TMP/base"
mkbase "$BASE" "$PROJ"
BASE_SHA=$(git -C "$BASE" rev-parse HEAD)
SECRET_KEY="AKIAIOSFODNN7EXAMPLE"
URL="ws://127.0.0.1:$PORT"

# ==================================================================================================
section "setup: a server and two machines"
start_server main "$SRV" "$PORT"
for n in a b c d; do add_device "$n"; done
for n in a b; do mk_machine "$n" "$n" "$URL" "$TMP/token-$n"; clone_for "$n"; done
out=$("$BIN" admin --data "$SRV" device list 2>&1)
contains "the device list shows a" "$out" " a "
contains "the device list shows b" "$out" " b "
lacks "no device is revoked yet" "$out" "revoked"
out=$(bt a enroll "$(wp "$(repo_of a)")" 2>&1)
contains "A enrolls its clone" "$out" "enrolled"
out=$(bt b enroll "$(wp "$(repo_of b)")" 2>&1)
contains "B enrolls its clone" "$out" "enrolled"
await "A's daemon connects" 10000 log_has "$(dlog a)" "connected as a"
await "B's daemon connects" 10000 log_has "$(dlog b)" "connected as b"
section_end

# ==================================================================================================
section "a turn on A reaches B"
printf 'edited by a\n' >>"$(repo_of a)/a.txt"
out=$(turn_as a t1 "first turn from a")
contains "A records the turn" "$out" "t1@1"
T1_A=$(tip_on a t1)
await "the ref reaches the server and B" 10000 everywhere t1 "$T1_A" b
eq "B's ref equals A's tip" "$T1_A" "$(tip_on b t1)"
out=$(btr b log t1)
contains "B's baton log lists the turn" "$out" "first turn from a"
contains "B's baton log lists the thread's first turn" "$out" "t1@1"
contains "B's daemon says which machine it came from" "$(cat "$(dlog b)")" "follow t1 ${T1_A:0:8} from a"
check "B's side repo passes fsck" gitd "$(side_of b)" fsck --strict --no-dangling
eq "the server's repo runs with gc.auto=0" "0" "$(gitd "$(srv_repo)" config gc.auto 2>/dev/null || echo unset)"
eq "the server's thread index agrees" "1" "$("$BIN" admin --data "$SRV" threads | grep -c ' t1 ')"
section_end

# ==================================================================================================
section "the server's repo is plain git"
SRV_GIT=$(srv_repo)
printf 'edited again\n' >>"$(repo_of a)/b.txt"
turn_as a t1 "second turn from a" >/dev/null
T2_A=$(tip_on a t1)
await "the second turn reaches the server" 10000 everywhere t1 "$T2_A" b
MIRROR="$TMPW/mirror.git"
check "git clone --mirror of the server repo works" git clone -q --mirror "$(wp "$SRV_GIT")" "$MIRROR"
check "fsck --strict of the mirror is clean" gitd "$MIRROR" fsck --strict --no-dangling
eq "the mirror holds A's tip" "$T2_A" "$(gitd "$MIRROR" rev-parse refs/baton/t1)"
check "fsck --strict of the server repo itself is clean" gitd "$SRV_GIT" fsck --strict --no-dangling
msg=$(gitd "$SRV_GIT" cat-file commit refs/baton/t1)
contains "the tip names its machine" "$msg" "Baton-Machine: a"
contains "the tip names its thread" "$msg" "Baton-Thread: t1"
printf 'a third edit\n' >>"$(repo_of a)/a.txt"
turn_as a t1 "third turn from a" >/dev/null
T3_A=$(tip_on a t1)
await "the third turn reaches the server" 10000 everywhere t1 "$T3_A" b
check "git fetch of the mirror brings the new tip" gitd "$MIRROR" fetch -q "$(wp "$SRV_GIT")" '+refs/baton/*:refs/baton/*'
eq "the mirror moved to the third turn" "$T3_A" "$(gitd "$MIRROR" rev-parse refs/baton/t1)"
check "fsck --strict of the mirror is clean after the fetch" gitd "$MIRROR" fsck --strict --no-dangling
eq "the history is linear: three turns" "3" "$(gitd "$SRV_GIT" rev-list --count refs/baton/t1)"
section_end

# ==================================================================================================
section "redacted content stays redacted"
printf 'AWS_KEY=%s\nnote=plain\n' "$SECRET_KEY" >"$(repo_of a)/app.cfg"
turn_as a sec "key $SECRET_KEY added" >/dev/null
SEC_A=$(tip_on a sec)
await "the turn reaches the server and B" 10000 everywhere sec "$SEC_A" b
check "the secret is in A's working file, so the check below is not vacuous" grep -q "$SECRET_KEY" "$(repo_of a)/app.cfg"
blob=$(gitd "$SRV_GIT" cat-file -p refs/baton/sec:app.cfg)
lacks "the server's blob has no AWS key" "$blob" "$SECRET_KEY"
contains "the server's blob carries the redaction marker" "$blob" "[baton:redacted:aws-access-key]"
for where in "$SRV_GIT" "$(side_of a)" "$(side_of b)"; do
  gitd "$where" cat-file --batch-all-objects --batch >"$TMP/objects.dump"
  check "no object in $(basename "$(dirname "$(dirname "$where")")")/$(basename "$where") holds the key" bash -c '! grep -aq -e "$0" "$1"' "$SECRET_KEY" "$TMP/objects.dump"
done
rm -f "$TMP/objects.dump"
lacks "the server's tip subject has no key" "$(gitd "$SRV_GIT" cat-file commit refs/baton/sec)" "$SECRET_KEY"
leaks=$(grep -rl -e "$SECRET_KEY" "$SRV" "$(home_of b)" 2>/dev/null | grep -v config.toml || true)
eq "no file under the server's data dir or B's home holds the key" "" "$leaks"
section_end

# ==================================================================================================
section "one owner per thread"
printf 'own\n' >>"$(repo_of a)/a.txt"
turn_as a own "a starts the thread" >/dev/null
OWN_A=$(tip_on a own)
await "A's thread reaches B" 10000 everywhere own "$OWN_A" b
turn_as b own "b writes to a's thread" >/dev/null
OWN_B=$(tip_on b own)
check "B recorded a local turn on A's thread" test "$OWN_B" != "$OWN_A"
await "B's daemon says the thread is A's and does not push" 10000 log_has "$(dlog b)" "sync: own belongs to a"
never "the server's tip never leaves A's" 1000 server_tip_not own "$OWN_A"
# The server takes the owner from the tip's Baton-Machine trailer, so an operator who points a ref at
# another machine's turn by hand hands the thread over. The previous owner's next push is refused.
printf 'mine\n' >>"$(repo_of b)/b.txt"
turn_as b mine "b starts a thread" >/dev/null
MINE_B=$(tip_on b mine)
await "B's own thread reaches the server and A" 10000 everywhere mine "$MINE_B" a
gitd "$SRV_GIT" update-ref refs/baton/mine "$OWN_A"
printf 'mine again\n' >>"$(repo_of b)/b.txt"
turn_as b mine "b continues a thread the server now gives to a" >/dev/null
MINE_B2=$(tip_on b mine)
check "B recorded the next turn" test "$MINE_B2" != "$MINE_B"
await "the server refuses it as not_owner" 10000 log_has "$(dlog b)" "refused ${MINE_B2:0:8} of mine (NotOwner)"
eq "the ref stays where the operator put it" "$OWN_A" "$(tip_on_server mine)"
eq "A's own thread is untouched" "$OWN_A" "$(tip_on a own)"
check "the server repo is still clean" gitd "$SRV_GIT" fsck --strict --no-dangling
section_end

# ==================================================================================================
section "a forged Baton-Machine trailer"
# C is configured as machine a but holds the token of device c, so its turns claim a machine it is not.
mk_machine c a "$URL" "$TMP/token-c"
clone_for c
out=$(bt c enroll "$(wp "$(repo_of c)")" 2>&1)
contains "C enrolls" "$out" "enrolled"
await "C connects with its own token" 10000 log_has "$(dlog c)" "connected as c"
printf 'forged\n' >>"$(repo_of c)/b.txt"
turn_as c forged "c claims to be a" >/dev/null
FORGED=$(tip_on c forged)
check "C recorded the turn locally" test -n "$FORGED"
contains "C's turn names machine a" "$(gitd "$(side_of c)" cat-file commit refs/baton/forged)" "Baton-Machine: a"
await "C's daemon logs the refusal" 10000 log_has "$(dlog c)" "refused ${FORGED:0:8} of forged (MachineMismatch)"
eq "the server has no such thread" "" "$(tip_on_server forged)"
eq "B never received it" "" "$(tip_on b forged)"
check "the server repo is still clean" gitd "$SRV_GIT" fsck --strict --no-dangling
section_end

# ==================================================================================================
section "A offline for $OFFLINE_TURNS turns"
stop_server main
printf 'offline\n' >>"$(repo_of a)/a.txt"
for i in $(seq 1 "$OFFLINE_TURNS"); do
  printf 'offline %s\n' "$i" >>"$(repo_of a)/b.txt"
  out=$(turn_as a off "offline turn $i")
  contains "turn $i is recorded with the server down" "$out" "off@$i"
done
OFF_A=$(tip_on a off)
sleep 0.5
eq "B has nothing of it yet" "" "$(tip_on b off)"
began=$(now_us)
start_server main "$SRV" "$PORT"
await "all $OFFLINE_TURNS turns reach the server and B" 30000 everywhere off "$OFF_A" b
echo "  caught up $((($(now_us) - began) / 1000)) ms after the server listened again"
eq "the turns arrived in order" "$(gitd "$(side_of a)" rev-list refs/baton/off | tr '\n' ' ')" "$(gitd "$(side_of b)" rev-list refs/baton/off | tr '\n' ' ')"
eq "B counts $OFFLINE_TURNS turns" "$OFFLINE_TURNS" "$(gitd "$(side_of b)" rev-list --count refs/baton/off)"
out=$(btr b log off)
contains "B's log lists the last offline turn" "$out" "offline turn $OFFLINE_TURNS"
contains "A's daemon logged the loss and the reconnect" "$(cat "$(dlog a)")" "sync: disconnected"
section_end

# ==================================================================================================
section "a burst of $BURST turns from $BURST threads in 3 s"
for i in $(seq -w 1 "$BURST"); do printf 'burst %s\n' "$i" >"$(repo_of a)/burst-$i.txt"; done
began=$(now_us)
burst_jobs=()
n=0
for i in $(seq -w 1 "$BURST"); do
  turn_as a "burst-$i" "burst $i" >/dev/null &
  burst_jobs+=($!)
  n=$((n + 1))
  while (($(now_us) < began + n * 100000)); do sleep 0.005; done
done
wait "${burst_jobs[@]}"
span=$((($(now_us) - began) / 1000))
echo "  $BURST turns issued over $span ms"
all_arrived() { # all_arrived : every burst thread is the same sha on A, the server and B
  local i
  for i in $(seq -w 1 "$BURST"); do
    everywhere "burst-$i" "$(tip_on a "burst-$i")" b || return 1
  done
}
await "all $BURST threads reach the server and B" 30000 all_arrived
sed -n 's/^\([0-9]*\)\.\([0-9]*\) .* turn burst-\([0-9]*\)@1 .*/\3 \1\2/p' "$(dlog a)" >"$TMP/burst.a"
sed -n 's/^\([0-9]*\)\.\([0-9]*\) follow burst-\([0-9]*\) [0-9a-f]* from .* stored.*/\3 \1\2/p' "$(dlog b)" >"$TMP/burst.b"
awk 'FNR == NR {a[$1] = $2; next} ($1 in a) {print $2 - a[$1]}' "$TMP/burst.a" "$TMP/burst.b" >"$TMP/burst.us"
eq "every burst turn is in both daemons' logs" "$BURST" "$(wc -l <"$TMP/burst.us" | tr -d ' ')"
BURST_SUMMARY="turn recorded on A to stored on B: p50 $(percentile "$TMP/burst.us" 50) ms, p95 $(percentile "$TMP/burst.us" 95) ms, max $(percentile "$TMP/burst.us" 100) ms over $BURST threads"
echo "  $BURST_SUMMARY"
eq "B holds $BURST burst refs" "$BURST" "$(ls "$(side_of b)/refs/baton" | grep -c '^burst-')"
check "the server repo is clean after the burst" gitd "$SRV_GIT" fsck --strict --no-dangling
section_end

# ==================================================================================================
section "a revoked token"
mk_machine d d "$URL" "$TMP/token-d"
clone_for d
bt d enroll "$(wp "$(repo_of d)")" >/dev/null 2>&1
await "D connects while its token is good" 10000 log_has "$(dlog d)" "connected as d"
printf 'd1\n' >>"$(repo_of d)/a.txt"
turn_as d dthread "d before revoke" >/dev/null
D1=$(tip_on d dthread)
await "D's turn reaches the server" 10000 everywhere dthread "$D1" b
out=$("$BIN" admin --data "$SRV" device revoke d 2>&1)
contains "revoke says what it did" "$out" "revoked 1"
contains "the device list marks it revoked" "$("$BIN" admin --data "$SRV" device list)" "revoked"
await "the open session of D ends within 6 s of the revoke" 6000 log_has "$(dlog d)" "sync: disconnected"
printf 'd2\n' >>"$(repo_of d)/a.txt"
turn_as d dthread "d after revoke" >/dev/null
D2=$(tip_on d dthread)
check "D recorded the second turn locally" test "$D2" != "$D1"
never "the server never receives it" 3000 server_tip_is dthread "$D2"
eq "the server's tip is the one from before the revoke" "$D1" "$(tip_on_server dthread)"
await "D's daemon logs the rejected connect" 10000 log_has "$(dlog d)" "401"
printf 'nope\n' >"$TMP/token-nope"
mk_machine x x "$URL" "$TMP/token-nope"
clone_for x
bt x enroll "$(wp "$(repo_of x)")" >/dev/null 2>&1
await "an unknown token is rejected the same way" 10000 log_has "$(dlog x)" "401"
turn_as x xthread "x has no device" >/dev/null
never "the server never receives X's turn" 2000 server_has xthread
section_end

# ==================================================================================================
section "baton pull"
PULLED="$TMPW/pulled"
B_STATUS=$(git -C "$(repo_of b)" status --porcelain)
out=$(btr b pull t1 --into "$PULLED")
contains "pull says what it pulled" "$out" "pulled t1@3"
contains "pull names the base commit" "$out" "$BASE_SHA"
check "the thread tip equals the pulled files" tree_matches "$TMP/pulled" "$(side_of b)" refs/baton/t1
eq "the worktree is detached at the thread's base" "$BASE_SHA" "$(git -C "$TMP/pulled" rev-parse HEAD)"
check "git registered it as a worktree of B's clone" bash -c "git -C '$(repo_of b)' worktree list | grep -q pulled"
eq "B's own checkout was not touched" "$B_STATUS" "$(git -C "$(repo_of b)" status --porcelain)"
mkdir -p "$TMP/stranger"
git init -q "$TMP/stranger/repo"
git -C "$TMP/stranger/repo" remote add origin "https://github.com/E2E/$PROJ.git"
printf 'unrelated\n' >"$TMP/stranger/repo/x.txt"
git -C "$TMP/stranger/repo" add -A >/dev/null 2>&1
git -C "$TMP/stranger/repo" commit -q -m unrelated
bt b enroll "$(wp "$TMP/stranger/repo")" >/dev/null 2>&1
out=$(cd "$TMP/stranger/repo" && BATON_HOME=$(home_of b) "$BIN" pull t1 --into "$TMPW/never" 2>&1)
contains "a missing base commit says to git fetch" "$out" "git fetch"
check "nothing was created for it" test ! -e "$TMP/never"
bt b unenroll "$(wp "$TMP/stranger/repo")" >/dev/null 2>&1
section_end

# ==================================================================================================
section "baton follow --into"
mkdir -p "$TMP/plain"
printf 'mine\n' >"$TMP/plain/keep.txt"
out=$(btr_within b 20 follow t1 --into "$TMPW/plain")
contains "follow refuses a directory that baton pull did not make" "$out" "is not a worktree \`baton pull\` made"
eq "and writes nothing into it" "keep.txt" "$(ls "$TMP/plain")"
HOOKS_BEFORE=$(ls -A "$(repo_of b)/.git/hooks" | sort)
out=$(btr_within b 20 follow t1 --into "$(wp "$(repo_of b)")/.git/hooks")
contains "follow refuses a path under .git" "$out" "is not a worktree \`baton pull\` made"
eq "and the hooks directory is unchanged" "$HOOKS_BEFORE" "$(ls -A "$(repo_of b)/.git/hooks" | sort)"
cd "$(repo_of b)" || exit 2
export BATON_HOME=$(home_of b)
"$BIN" follow t1 --into "$PULLED" >"$TMP/follow.out" 2>&1 &
FOLLOW=$(native_pid $!)
export BATON_HOME="$TMPW/nowhere"
cd "$root" || exit 2
BG_PIDS+=("$FOLLOW")
await "follow starts from the tip" 10000 log_has "$TMP/follow.out" "following t1 from ${T3_A:0:8}"
printf 'edited here\n' >"$TMP/pulled/b.txt"
printf 'untouched here\n' >"$TMP/pulled/keep-me.txt"
printf 'next from a\n' >>"$(repo_of a)/a.txt"
printf 'next from a\n' >"$(repo_of a)/src/new.rs"
printf 'next from a, b side\n' >>"$(repo_of a)/b.txt"
turn_as a t1 "fourth turn from a" >/dev/null
T4_A=$(tip_on a t1)
await "the next turn reaches B" 10000 everywhere t1 "$T4_A" b
file_has() { grep -q -- "$2" "$1"; }
await "follow applies a path nobody touched here" 10000 file_has "$TMP/pulled/a.txt" "next from a"
await "follow prints the turn" 10000 log_has "$TMP/follow.out" "fourth turn from a"
# The turn is printed before its files are written, so wait for the report that ends the apply.
await "follow reports what it applied" 10000 log_has "$TMP/follow.out" "kept b.txt"
check "follow added the new file" test -f "$TMP/pulled/src/new.rs"
eq "follow kept the locally edited path" "edited here" "$(cat "$TMP/pulled/b.txt")"
contains "follow says it kept b.txt and why" "$(cat "$TMP/follow.out")" "kept b.txt"
eq "an unrelated local file is left alone" "untouched here" "$(cat "$TMP/pulled/keep-me.txt")"
kill_pid "$FOLLOW"
section_end

# ==================================================================================================
section "killing the server mid-push"
behind=0
caught=0
delays=(0.05 0.1 0.15 0.2 0.3 0.4 0.5 0.7)
KILL_A=""
for r in $(seq 1 "$KILL_ROUNDS"); do
  (
    for k in 1 2 3; do
      head -c 1500000 /dev/urandom >"$(repo_of a)/kill-$k.bin"
      turn_as a kill "round $r turn $k" >/dev/null 2>&1
    done
  ) &
  worker=$!
  sleep "${delays[$(((r - 1) % ${#delays[@]}))]}"
  a_tip=$(tip_on a kill)
  s_tip=$(tip_on_server kill)
  stop_server main
  if [[ $a_tip != "$s_tip" ]]; then behind=$((behind + 1)); else caught=$((caught + 1)); fi
  wait "$worker"
  check "round $r: the server repo passes fsck after the kill" gitd "$SRV_GIT" fsck --strict --no-dangling
  eq "round $r: no ref lock is left behind" "" "$(find "$SRV_GIT/refs" -name '*.lock' 2>/dev/null)"
  start_server main "$SRV" "$PORT"
  KILL_A=$(tip_on a kill)
  await "round $r: the server and B catch up" 30000 everywhere kill "$KILL_A" b
done
echo "  the server was behind A when killed in $behind rounds and level with it in $caught"
check "the server repo passes fsck at the end" gitd "$SRV_GIT" fsck --strict --no-dangling
eq "the history is linear and complete" "$((KILL_ROUNDS * 3))" "$(gitd "$SRV_GIT" rev-list --count refs/baton/kill)"
section_end

# ==================================================================================================
section "latency: hook call on A to the ref moved in B's side repo"
LAT_SUMMARY="skipped"
if [[ -d $BIG_REPO/.git ]]; then
  BIG="$TMP/big"
  mkdir -p "$BIG"
  git -C "$BIG_REPO" archive HEAD | tar -x -C "$BIG"
  git init -q "$BIG"
  git -C "$BIG" remote add origin "https://github.com/E2E/bigtree.git"
  files=$(find "$BIG" -type f -not -path '*/.git/*' | wc -l | tr -d ' ')
  echo "  copied $files files"
  mkmin bigb bigtree
  bt b enroll "$(wp "$(repo_of bigb)")" >/dev/null 2>&1
  out=$(bt a enroll "$(wp "$BIG")" 2>&1)
  contains "the large tree enrolls on A" "$out" "enrolled"
  echo "  $out"
  await "A's daemon is connected" 10000 log_has "$(dlog a)" "connected as a"
  await "B subscribes to the project" 10000 log_has "$(dlog b)" "enrolled project=github.com/e2e/bigtree"
  export BATON_HOME=$(home_of a)
  mapfile -t edit < <(cd "$BIG" && find . -name '*.php' -path './app/*' | head -3)
  payload() { printf '{"session_id":"%s","cwd":"%s","hook_event_name":"Stop"}' "$2" "$(wp "$1")" >"$3"; }
  payload "$BIG" lat "$TMP/payload-big.json"
  LAT_A="$(side_of a bigtree)/refs/baton/lat"
  LAT_B="$(side_of b bigtree)/refs/baton/lat"
  # The first turn of a thread uploads the whole tree. It is timed and set aside.
  for f in "${edit[@]}"; do printf 'line 0\n' >>"$BIG/$f"; done
  t0=$(now_us)
  "$BIN" hook claude <"$TMP/payload-big.json"
  if wait_ref_change "$LAT_B" "" 120000; then
    FIRST_UPLOAD_MS=$((($(now_us) - t0) / 1000))
    ok "the first turn uploads the whole tree to B"
    echo "  first upload of $files files, hook call to B: ${FIRST_UPLOAD_MS} ms (set aside)"
  else
    bad "the first turn never reached B"
    FIRST_UPLOAD_MS="n/a"
    TURNS=0
  fi
  sleep 1
  : >"$TMP/lat.tsv"
  lost=0
  for i in $(seq 1 "$TURNS"); do
    for f in "${edit[@]}"; do printf 'line %s\n' "$i" >>"$BIG/$f"; done
    old=$(ref_sha "$LAT_B")
    t0=$(now_us)
    "$BIN" hook claude <"$TMP/payload-big.json"
    t1=$(now_us)
    if wait_ref_change "$LAT_B" "$old" 10000; then
      t2=$(now_us)
      sha=$(ref_sha "$LAT_B")
      printf '%s\t%s\t%s\t%s\t%s\n' "$i" "$t0" "$t1" "$t2" "${sha:0:8}" >>"$TMP/lat.tsv"
    else
      lost=$((lost + 1))
      bad "turn $i never reached B"
      if ((lost >= 3)); then
        echo "  three samples lost in a row; the run stops here"
        break
      fi
    fi
    sleep 0.15
  done
  n=$(wc -l <"$TMP/lat.tsv" | tr -d ' ')
  eq "all $TURNS timed turns reached B" "$TURNS" "$n"
  eq "B's tip equals A's tip" "$(ref_sha "$LAT_A")" "$(ref_sha "$LAT_B")"
  eq "the server's tip equals A's tip" "$(ref_sha "$LAT_A")" "$(tip_on_server lat bigtree)"

  stamps() { # stamps log sed-expression : "short-sha microseconds" per matching line
    sed -n "$2" "$1" >"$3"
  }
  stamps "$(dlog a)" 's/^\([0-9]*\)\.\([0-9]*\) .* turn lat@[0-9]* \([0-9a-f]*\) .*/\3 \1\2/p' "$TMP/st.commit"
  stamps "$(dlog a)" 's/^\([0-9]*\)\.\([0-9]*\) push lat \([0-9a-f]*\) sent.*/\3 \1\2/p' "$TMP/st.sent"
  stamps "$TMP/server-main.log" 's/^\([0-9]*\)\.\([0-9]*\) advance lat \([0-9a-f]*\) took.*/\3 \1\2/p' "$TMP/st.adv"
  stamps "$(dlog b)" 's/^\([0-9]*\)\.\([0-9]*\) follow lat \([0-9a-f]*\) from .* stored.*/\3 \1\2/p' "$TMP/st.stored"
  rm -f "$TMP"/seg.*
  awk -v dir="$TMP" '
    kind == "turn"   { T0[$5] = $2; T1[$5] = $3; T2[$5] = $4; ord[++n] = $5; next }
    kind == "commit" { C[$1] = $2; next }
    kind == "sent"   { P[$1] = $2; next }
    kind == "adv"    { S[$1] = $2; next }
    kind == "stored" { B[$1] = $2; next }
    END {
      for (i = 1; i <= n; i++) {
        s = ord[i]
        if (!(s in P) || !(s in S) || !(s in B)) { missing++; continue }
        print T1[s] - T0[s] > dir "/seg.hook"
        print P[s] - T1[s] > dir "/seg.adaemon"
        print S[s] - P[s] > dir "/seg.server"
        print B[s] - S[s] > dir "/seg.bdaemon"
        print T2[s] - B[s] > dir "/seg.seen"
        print T2[s] - T0[s] > dir "/seg.total"
        print B[s] - T0[s] > dir "/seg.tostored"
        print B[s] - T1[s] > dir "/seg.syncpath"
        print s in C ? C[s] - T1[s] : 0 > dir "/seg.capture"
        joined++
      }
      print joined + 0 > dir "/seg.joined"
      print missing + 0 > dir "/seg.missing"
    }' kind=turn "$TMP/lat.tsv" kind=commit "$TMP/st.commit" kind=sent "$TMP/st.sent" kind=adv "$TMP/st.adv" kind=stored "$TMP/st.stored"
  joined=$(cat "$TMP/seg.joined")
  eq "every timed turn is found in all four logs" "$n" "$joined"
  LAT_TABLE=$(
    printf '| %-46s | %7s | %7s | %7s |\n' "segment" "p50 ms" "p95 ms" "max ms"
    for row in "hook call to B's ref stored (B's log)|tostored" "hook call to B's ref seen by the poller|total" "hook process (spawn to exit)|hook" "A's daemon (hook exit to push sent)|adaemon" "server (push sent to advance done)|server" "B's daemon (advance done to ref stored)|bdaemon" "hook exit to B's ref stored (the sync path)|syncpath" "poller lag (ref stored to seen)|seen"; do
      label=${row%|*}
      key=${row#*|}
      printf '| %-46s | %7s | %7s | %7s |\n' "$label" "$(percentile "$TMP/seg.$key" 50)" "$(percentile "$TMP/seg.$key" 95)" "$(percentile "$TMP/seg.$key" 100)"
    done
  )
  echo "$LAT_TABLE" | sed 's/^/  /'
  LAT_SUMMARY="hook call to B's ref stored: p50 $(percentile "$TMP/seg.tostored" 50) ms, p95 $(percentile "$TMP/seg.tostored" 95) ms, max $(percentile "$TMP/seg.tostored" 100) ms over $n turns on $files files ($lost lost)"
  echo "  $LAT_SUMMARY"
  : >"$TMP/floor.us"
  payload "$TMP" nobody "$TMP/payload-none.json"
  for _ in $(seq 1 30); do
    t0=$(now_us)
    "$BIN" hook claude <"$TMP/payload-none.json"
    echo $(($(now_us) - t0)) >>"$TMP/floor.us"
  done
  export BATON_HOME="$TMPW/nowhere"
  FLOOR_SUMMARY="p50 $(percentile "$TMP/floor.us" 50) ms, p95 $(percentile "$TMP/floor.us" 95) ms over 30 hook processes"
  echo "  hook process with nothing to record, frame to a running daemon: $FLOOR_SUMMARY"
  check "the server's bigtree repo passes fsck" gitd "$(srv_repo bigtree)" fsck --strict --no-dangling
  check "B's bigtree side repo passes fsck" gitd "$(side_of b bigtree)" fsck --strict --no-dangling
else
  echo "  skipped: $BIG_REPO is not a git repo (set BATON_E2E_BIG_REPO)"
fi
section_end

# ==================================================================================================
section "the index is rebuildable"
stop_server main
before=$("$BIN" admin --data "$SRV" threads | sort)
mkdir -p "$TMP/server-rebuilt"
cp -r "$SRV/projects" "$TMP/server-rebuilt/projects"
out=$("$BIN" admin --data "$TMPW/server-rebuilt" reindex 2>&1)
contains "reindex reports what it found" "$out" "indexed"
eq "an index rebuilt from the refs alone lists the same threads" "$before" "$("$BIN" admin --data "$TMPW/server-rebuilt" threads | sort)"
refs_on_disk=$(for p in "$SRV"/projects/*.git; do gitd "$p" for-each-ref --format='x' refs/baton; done | wc -l | tr -d ' ')
eq "it indexes one thread per ref on disk" "$refs_on_disk" "$(printf '%s\n' "$before" | grep -c .)"
out=$("$BIN" admin --data "$SRV" reindex 2>&1)
contains "reindex also works in place" "$out" "indexed"
eq "and changes nothing" "$before" "$("$BIN" admin --data "$SRV" threads | sort)"
contains "the revoked device is still revoked: devices are not rebuilt" "$("$BIN" admin --data "$SRV" device list)" "revoked"
start_server main "$SRV" "$PORT"
contains "the restarted server loads the index" "$(grep 'index holds' "$TMP/server-main.log" | tail -1)" "index holds"
section_end

# ==================================================================================================
section "wss with a private CA"
if command -v openssl >/dev/null 2>&1; then
  TLS="$TMP/tls"
  mkdir -p "$TLS"
  (
    cd "$TLS" || exit 1
    export MSYS_NO_PATHCONV=1
    openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:prime256v1 -nodes -keyout ca.key -out ca.crt -days 2 \
      -subj "/CN=baton e2e CA" -addext "basicConstraints=critical,CA:TRUE" -addext "keyUsage=critical,keyCertSign,cRLSign"
    openssl req -newkey ec -pkeyopt ec_paramgen_curve:prime256v1 -nodes -keyout srv.key -out srv.csr -subj "/CN=localhost"
    printf 'subjectAltName=IP:127.0.0.1,DNS:localhost\nbasicConstraints=CA:FALSE\nextendedKeyUsage=serverAuth\n' >ext.cnf
    openssl x509 -req -in srv.csr -CA ca.crt -CAkey ca.key -CAcreateserial -out srv.crt -days 2 -extfile ext.cnf
  ) >"$TMP/openssl.out" 2>&1
  check "openssl made a CA and a server certificate" test -s "$TLS/srv.crt"
  TLS_PORT=$((PORT + 1))
  TLS_DATA="$TMPW/server-tls"
  start_server tls "$TLS_DATA" "$TLS_PORT" --tls-cert "$(wp "$TLS/srv.crt")" --tls-key "$(wp "$TLS/srv.key")"
  for n in ta tb tx; do add_device "$n" "$TLS_DATA"; done
  mk_machine ta ta "wss://127.0.0.1:$TLS_PORT" "$TMP/token-ta" "$TLS/ca.crt"
  mk_machine tb tb "wss://127.0.0.1:$TLS_PORT" "$TMP/token-tb" "$TLS/ca.crt"
  mk_machine tx tx "wss://127.0.0.1:$TLS_PORT" "$TMP/token-tx"
  mkmin ta tlsproj
  mkmin tb tlsproj
  mkmin tx tlsproj
  for n in ta tb tx; do bt "$n" enroll "$(wp "$(repo_of "$n")")" >/dev/null 2>&1; done
  await "a daemon that trusts the CA connects over wss" 10000 log_has "$(dlog ta)" "connected as ta"
  await "the second one connects too" 10000 log_has "$(dlog tb)" "connected as tb"
  printf 'over tls\n' >>"$(repo_of ta)/a.txt"
  turn_as ta tls "turn over wss" >/dev/null
  TLS_A=$(tip_on ta tls tlsproj)
  tls_everywhere() { [[ -n $TLS_A && $(ref_sha "$(srv_repo tlsproj "$TLS_DATA")/refs/baton/tls") == "$TLS_A" ]] && arrived tb tls "$TLS_A" tlsproj; }
  await "the turn crosses the TLS server" 10000 tls_everywhere
  never "a daemon without the CA never connects" 2000 log_has "$(dlog tx)" "connected as"
  contains "and says the certificate is why" "$(cat "$(dlog tx)")" "sync: disconnected"
  check "the TLS server repo passes fsck" gitd "$(srv_repo tlsproj "$TLS_DATA")" fsck --strict --no-dangling
  stop_server tls
else
  echo "  skipped: openssl is not installed"
fi
section_end

# ==================================================================================================
section "a command auth provider"
CMD_PORT=$((PORT + 2))
CMD_DATA="$TMPW/server-cmd"
mkdir -p "$CMD_DATA"
cat >"$TMP/auth.sh" <<'EOF'
#!/usr/bin/env bash
in=$(cat)
token=$(printf '%s' "$in" | sed -n 's/.*"authorization":"Bearer \([^"]*\)".*/\1/p')
printf '%s\n' "$token" >>"$AUTH_LOG"
case $token in
  secret-ca) echo '{"schema":"baton.auth.v1","ok":true,"machine":"ca"}' ;;
  secret-cb) echo '{"schema":"baton.auth.v1","ok":true,"machine":"cb"}' ;;
  crash) echo 'this is not json'; exit 3 ;;
  late) (sleep 7; echo '{"schema":"baton.auth.v1","ok":true,"machine":"cl"}') & exit 0 ;;
  hold) echo '{"schema":"baton.auth.v1","ok":true,"machine":"ch"}'; (sleep 13) & exit 0 ;;
  *) echo '{"ok":false}' ;;
esac
EOF
printf '[auth]\nprovider = "command"\ncommand = ["%s", "%s"]\ntimeout_ms = 5000\n' "$(wp "$(command -v bash)")" "$(wp "$TMP/auth.sh")" >"$CMD_DATA/server.toml"
export AUTH_LOG="$TMPW/auth.calls"
: >"$TMP/auth.calls"
start_server cmd "$CMD_DATA" "$CMD_PORT"
CMD_URL="ws://127.0.0.1:$CMD_PORT"
printf 'secret-ca' >"$TMP/token-ca"
printf 'secret-cb' >"$TMP/token-cb"
printf 'wrong' >"$TMP/token-cw"
printf 'crash' >"$TMP/token-cx"
mk_machine ca ca "$CMD_URL" "$TMP/token-ca"
mk_machine cb cb "$CMD_URL" "$TMP/token-cb"
mk_machine cw cw "$CMD_URL" "$TMP/token-cw"
mk_machine cx cx "$CMD_URL" "$TMP/token-cx"
for n in ca cb cw cx; do mkmin "$n" cmdproj; bt "$n" enroll "$(wp "$(repo_of "$n")")" >/dev/null 2>&1; done
await "a token the provider accepts connects as the machine it names" 10000 log_has "$(dlog ca)" "connected as ca"
await "the other accepted token connects" 10000 log_has "$(dlog cb)" "connected as cb"
printf 'cmd\n' >>"$(repo_of ca)/a.txt"
turn_as ca cmd "turn through the provider" >/dev/null
CMD_A=$(tip_on ca cmd cmdproj)
cmd_everywhere() { [[ -n $CMD_A && $(ref_sha "$(srv_repo cmdproj "$CMD_DATA")/refs/baton/cmd") == "$CMD_A" ]] && arrived cb cmd "$CMD_A" cmdproj; }
await "the turn crosses a command-auth server" 10000 cmd_everywhere
await "a token the provider rejects gets 401" 10000 log_has "$(dlog cw)" "401"
await "a provider that crashes gets 401: auth fails closed" 10000 log_has "$(dlog cx)" "401"
turn_as cx cmdx "from a rejected machine" >/dev/null
never "nothing from a rejected machine reaches the server" 2000 ref_nonempty "$(srv_repo cmdproj "$CMD_DATA")/refs/baton/cmdx"
contains "the provider saw the bearer token" "$(cat "$TMP/auth.calls")" "secret-ca"
contains "and the rejected one" "$(cat "$TMP/auth.calls")" "wrong"
check "the command-auth server repo passes fsck" gitd "$(srv_repo cmdproj "$CMD_DATA")" fsck --strict --no-dangling
# The deadline covers the whole call, not just the provider process: a grandchild that answers after the
# provider exited, or that keeps the pipe open after a good answer, must not move the verdict or the time.
if [[ -z $WIRE_PY ]]; then
  bad "Python 3.8 or newer is needed to time the provider cases"
else
  printf 'late' >"$TMP/token-cl"
  printf 'hold' >"$TMP/token-ch"
  out=$(wire_cmd "$CMD_PORT" connect cl cl)
  read -r status ms <<<"$out"
  eq "a provider that exits and leaves a grandchild to answer later gets 401" "401" "$status"
  check "and it gets it in under 3 s, not when the grandchild is done at 7 s" test "${ms:-99999}" -lt 3000
  out=$(wire_cmd "$CMD_PORT" connect ch ch)
  read -r status ms <<<"$out"
  eq "a good answer is accepted although a grandchild keeps the pipe open" "101" "$status"
  check "and it is accepted in under 3 s, not when the grandchild lets go at 13 s" test "${ms:-99999}" -lt 3000
fi
stop_server cmd
section_end

# ==================================================================================================
section "a hostile .gitmodules is not captured"
printf '[submodule "x"]\n\tpath = x\n\turl = -uevil\n' >"$(repo_of a)/.gitmodules"
printf 'fine\n' >"$(repo_of a)/capture-ok.txt"
turn_as a hostile "a repo file git would refuse" >/dev/null
HOSTILE=$(tip_on a hostile)
check "the turn was recorded" test -n "$HOSTILE"
files=$(gitd "$(side_of a)" ls-tree -r --name-only refs/baton/hostile 2>/dev/null)
contains "the ordinary file is in the turn" "$files" "capture-ok.txt"
lacks "the .gitmodules that git would refuse is not" "$files" ".gitmodules"
rm -f "$(repo_of a)/.gitmodules" "$(repo_of a)/capture-ok.txt"
section_end

# ==================================================================================================
section "hostile clients on the raw wire"
if [[ -z $WIRE_PY ]]; then
  bad "Python 3.8 or newer is needed to drive e2e/wire.py"
else
  WIRE_PORT=$((PORT + 3))
  WIRE_DATA="$TMPW/server-wire"
  WIRE_LOG="$TMP/server-wire.log"
  wire_repo() { echo "$WIRE_DATA/projects/wire+$1.git"; }
  wire() { # wire case [args...] : as w1, one check per line the case prints
    local c=$1
    shift
    wire_lines "$c" "$(wire_cmd "$WIRE_PORT" "$c" w1 "$@")"
  }
  start_server wire "$WIRE_DATA" "$WIRE_PORT"
  for n in w1 w2 w3; do add_device "$n" "$WIRE_DATA"; done

  # The two slow cases wait out the server's timers while the quick ones run.
  for slow in stall quiet; do
    "$WIRE_PY" -I "$WIRE_SCRIPT" "$slow" "$WIRE_PORT" "$(wp "$TMP/token-w1")" w1 "$(wp "$WIRE_LOG")" >"$TMP/wire-$slow.out" 2>&1 &
    WIRE_SLOW[$slow]=$!
    BG_PIDS+=("$(native_pid $!)")
  done

  wire names
  check "the server repo passes fsck --strict after the hostile names" gitd "$(wire_repo names)" fsck --strict --no-dangling
  eq "and holds only the three clean turns" "3" "$(gitd "$(wire_repo names)" for-each-ref --format=x refs/baton | wc -l | tr -d ' ')"
  wire kinds
  check "the server repo passes fsck --strict after the kind mismatches" gitd "$(wire_repo kinds)" fsck --strict --no-dangling
  wire depth
  check "the server repo passes fsck --strict after the deep trees" gitd "$(wire_repo depth)" fsck --strict --no-dangling
  wire partial
  wire frames
  wire want
  wire flood
  eq "no project directory exists for a key that was only named" "" "$(ls "$WIRE_DATA/projects" | grep -E '^wire\+(sub|adv|flood)-' || true)"
  wire_lines revoke "$(wire_cmd "$WIRE_PORT" revoke w3 "$(wp "$TMP/token-w2")" w2 "$(wp "$BIN")" admin --data "$WIRE_DATA" device revoke w3)"
  wire handshake

  wait "${WIRE_SLOW[quiet]}"
  wire_lines quiet "$(cat "$TMP/wire-quiet.out")"
  contains "the server logged that the idle writer stopped" "$(cat "$WIRE_LOG")" "writer stopped after 30s idle"
  wait "${WIRE_SLOW[stall]}"
  wire_lines stall "$(cat "$TMP/wire-stall.out")"
  contains "the server logged why it closed the reader that stopped" "$(cat "$WIRE_LOG")" "the client did not read for 30s"
  contains "the server logged the handshakes it timed out" "$(cat "$WIRE_LOG")" "no handshake within"
  check "the server repo of the idle project passes fsck --strict" gitd "$(wire_repo idle)" fsck --strict --no-dangling
  stop_server wire
fi
section_end

# ==================================================================================================
section "the real data and config dirs are untouched"
real_after=""
for d in "${real_dirs[@]}"; do
  [[ -n $d ]] && real_after+="$d:$(stat -c %Y "$d" 2>/dev/null || stat -f %m "$d" 2>/dev/null || echo none);"
done
eq "no baton directory outside the temp dir was created or modified" "$real_before" "$real_after"
eq "the base repo's HEAD is unchanged" "$BASE_SHA" "$(git -C "$BASE" rev-parse HEAD)"
section_end

# --------------------------------------------------------------------------------------------------
summary
[[ $fail == 0 ]]
