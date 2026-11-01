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

# MSYS rewrites an argument like `refs/baton/t:.env` as a path list unless told not to.
sgit() { MSYS_NO_PATHCONV=1 git --git-dir="$SIDE" "$@"; }

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

mkmin() { # mkmin dir name : one committed file and no ignore rules
  git init -q "$1"
  git -C "$1" remote add origin "https://github.com/E2E/$2.git"
  printf 'a\n' >"$1/a.txt"
  git -C "$1" add -A >/dev/null 2>&1
  git -C "$1" commit -q -m initial
}

mkfiles() { # mkfiles dir count prefix : count small files, 501 to a directory named prefixNNN
  awk -v dir="$1" -v n="$2" -v pre="$3" 'BEGIN {
    for (i = 0; i < n; i++) {
      d = sprintf("%s/%s%03d", dir, pre, int(i / 501))
      if (i % 501 == 0) system("mkdir -p \"" d "\"")
      f = sprintf("%s/f%04d.txt", d, i % 501)
      print i > f
      close(f)
    }
  }'
}

turn_in() { # turn_in dir thread message [baton turn args...]
  local dir=$1 thread=$2 msg=$3
  shift 3
  (cd "$dir" && "$BIN" turn --thread "$thread" --message "$msg" "$@" 2>&1)
}

tree_of() { sgit ls-tree -r --name-only "refs/baton/$1"; } # needs SIDE
blob_of() { sgit cat-file -p "refs/baton/$1:$2" 2>&1; }

user_config() { # user_config [toml lines...] : the value_files stanza, then whatever follows
  {
    printf '[redact]\nvalue_files = ["%s"]\n' "$(wp "$TMP")/values.json"
    printf '%s\n' "$@"
  } >"$TMP/home/config/config.toml"
}

values_json() { # values_json [extra members, with a leading comma]
  printf '{"db":{"password":"%s","short":"abc"}%s}' "$SECRET_LITERAL" "${1:-}" >"$TMP/values.json"
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
LATE_LITERAL="LATE-literal-secret-9"
MACHINE_SECRET="ghp_AbCdEfGhIjKlMnOpQrStUvWxYz0123456789"
values_json ""
mkdir -p "$TMP/home/config"
user_config
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

section "A. the walk and today's policy beat the stat cache"
A="$TMP/policya"
mkmin "$A" policya
mkdir -p "$A/vendor"
printf 'scratch 1\n' >"$A/scratch.txt"
printf 'v1\n' >"$A/vendor/lib.js"
printf 'Case\n' >"$A/Case.txt"
printf 'prefix RULESECRET-abc suffix\n' >"$A/rule.txt"
printf 'uses %s here\n' "$LATE_LITERAL" >"$A/lit.txt"
"$BIN" enroll "$(wp "$A")" >/dev/null 2>&1
SIDE=$(side_of policya)
turn_in "$A" a1 "baseline" >/dev/null
contains "an untracked, unignored file is stored" "$(tree_of a1)" "scratch.txt"
sleep 2.3 # past the 2 s racy window: from here only the stat cache vouches for a file
printf 'scratch.txt\n' >"$A/.gitignore"
turn_in "$A" a2 "ignore it" >/dev/null
lacks "a path that becomes ignored leaves the tree" "$(tree_of a2)" "scratch.txt"
printf 'scratch 2\n' >"$A/scratch.txt"
turn_in "$A" a3 "edit it" >/dev/null
lacks "a later edit to it is never stored" "$(tree_of a3)" "scratch.txt"
user_config '[snapshot]' 'exclude = ["vendor/"]'
turn_in "$A" a4 "exclude vendor" >/dev/null
lacks "a path the user config starts excluding leaves the tree" "$(tree_of a4)" "vendor/lib.js"
mkdir -p "$A/nest"
printf 'ignored later\n' >"$A/nest/ign.txt"
printf 'kept\n' >"$A/nest/kept.txt"
sleep 0.4
printf 'ign.txt\n' >"$A/nest/.gitignore"
sleep 0.4
turn_in "$A" a5 "nested ignore arrives late" >/dev/null
tree=$(tree_of a5)
lacks "a file is not stored when its nested .gitignore arrives in a later batch" "$tree" "nest/ign.txt"
contains "its sibling is" "$tree" "nest/kept.txt"
mv "$A/Case.txt" "$A/case.txt"
sleep 0.3
turn_in "$A" a6 "case-only rename" >/dev/null
tree=$(tree_of a6)
contains "a case-only rename stores the new spelling" "$tree" "case.txt"
lacks "and drops the old one" "$tree" "Case.txt"
mkdir -p "$A/.baton"
printf '[[redact.rule]]\nname = "rule-secret"\nregex = "RULESECRET-[a-z]+"\n' >"$A/.baton/config.toml"
values_json ",\"late\":\"$LATE_LITERAL\""
turn_in "$A" a7 "new rule and new literal" >/dev/null
rule_blob=$(blob_of a7 rule.txt)
lit_blob=$(blob_of a7 lit.txt)
lacks "a project rule added later reaches a file that has not changed since" "$rule_blob" "RULESECRET-abc"
contains "and leaves its marker" "$rule_blob" "[baton:redacted:rule-secret]"
lacks "a literal added later reaches a file that has not changed since" "$lit_blob" "$LATE_LITERAL"
user_config
values_json ""
section_end

section "B. a file git tracks is stored even when an ignore rule matches it"
B="$TMP/trackedb"
mkmin "$B" trackedb
mkdir -p "$B/build"
printf 'build/\n' >"$B/.gitignore"
printf 'forced 1\n' >"$B/build/forced.o"
printf 'obj\n' >"$B/build/x.o"
git -C "$B" add -f .gitignore build/forced.o >/dev/null 2>&1
git -C "$B" commit -q -m "track a file the ignore rule matches"
"$BIN" enroll "$(wp "$B")" >/dev/null 2>&1
SIDE=$(side_of trackedb)
turn_in "$B" b1 "baseline" >/dev/null
tree=$(tree_of b1)
contains "the tracked file under an ignored directory is stored" "$tree" "build/forced.o"
lacks "an untracked file beside it is not" "$tree" "build/x.o"
printf 'forced 2\n' >"$B/build/forced.o"
turn_in "$B" b2 "edit it" >/dev/null
eq "an edit to it is stored" "forced 2" "$(blob_of b2 build/forced.o)"
section_end

section "C. a worktree beyond the dirty-path cap falls back to a full tree build"
C="$TMP/capc"
mkmin "$C" capc
mkfiles "$C" 50100 w1
out=$("$BIN" enroll "$(wp "$C")" 2>&1)
contains "enroll counts every file" "$out" "(50101 files)"
SIDE=$(side_of capc)
turn_in "$C" c1 "baseline" >/dev/null
eq "the baseline tree holds every file" 50101 "$(tree_of c1 | wc -l | tr -d ' ')"
mkfiles "$C" 50100 w2
turn_in "$C" c2 "a second wave while watched" >/dev/null
eq "a second wave beyond the cap is all in the next turn" 100201 "$(tree_of c2 | wc -l | tr -d ' ')"
check "the side repo passes fsck" sgit fsck --strict --no-dangling
"$BIN" unenroll "$(wp "$C")" >/dev/null 2>&1
section_end

section "D. redaction holes"
D="$TMP/redd"
mkmin "$D" redd
mkdir -p "$D/.baton"
printf '[snapshot]\nstore_whole = ["*"]\n[[redact.rule]]\nname = "oops"\nregex = "z*"\n' >"$D/.baton/config.toml"
printf 'DB=%s\nAWS=%s\n' "$SECRET_LITERAL" "$SECRET_KEY" >"$D/notes.txt"
cat >"$D/.env" <<EOT
DB_PASSWORD=p4ssw0rd-very-long-secret-value-1
API_KEY=key-very-long-secret-value-0123456789
password=unquoted-very-long-secret-value-1
DB_PASSWORD="prefixed-quoted-very-long-secret"
DATABASE_URL=postgres://user:pa/ss+word=longer@db.example.com/app
aws_secret_access_key = "AbcdEfghIjklMnopQrstUvwxYz0123456789/+=="
EOT
printf 'x\n' >"$D/$SECRET_KEY.txt"
printf 'x\n' >"$D/fine.txt"
git -C "$D" add -A >/dev/null 2>&1
git -C "$D" commit -q -m "secrets"
"$BIN" enroll "$(wp "$D")" >/dev/null 2>&1
SIDE=$(side_of redd)
user_config '[machine]' "id = \"$MACHINE_SECRET\""
out=$(turn_in "$D" d1 "work on $SECRET_KEY" --check "$SECRET_KEY=0" --check lint=0)
contains "the turn is recorded despite a repo rule that matches the empty string" "$out" "d1@1"
user_config
daemon_log=$(cat "$BATON_HOME/data/daemon.log")
contains "the empty-matching rule is dropped with a log line" "$daemon_log" "rule oops matches the empty string"
contains "a repo store_whole is ignored with a log line" "$daemon_log" "sets store_whole"
notes=$(blob_of d1 notes.txt)
lacks "a repo store_whole does not keep a value_files literal in a file" "$notes" "$SECRET_LITERAL"
lacks "or the AWS key" "$notes" "$SECRET_KEY"
env_blob=$(blob_of d1 .env)
for secret in "p4ssw0rd-very-long" "key-very-long-secret" "unquoted-very-long" "prefixed-quoted-very" "pa/ss+word" "Yz0123456789/+"; do
  lacks "the built-in patterns cut out $secret" "$env_blob" "$secret"
done
contains "the URL keeps its host" "$env_blob" "@db.example.com/app"
contains "the AWS secret ending in = is one match" "$env_blob" "\"[baton:redacted:aws-secret-key]\""
tree=$(tree_of d1)
lacks "a file named after a secret is not in the tree" "$tree" "$SECRET_KEY"
contains "other files still are" "$tree" "fine.txt"
msg=$(sgit cat-file commit refs/baton/d1)
contains "Baton-Skipped names it with the secret cut out" "$msg" "Baton-Skipped: [baton:redacted:aws-access-key].txt"
contains "a machine id holding a secret gives way to the host name" "$msg" "Baton-Machine: "
lacks "so the machine id is not in the commit" "$msg" "$MACHINE_SECRET"
lacks "a check name holding a secret is dropped" "$msg" "Baton-Check: $SECRET_KEY"
contains "other checks stay" "$msg" "Baton-Check: lint=0"
lacks "the subject is redacted" "$msg" "$SECRET_KEY"
out=$(cd "$D" && "$BIN" log d1 2>&1)
contains "baton log reads a turn that skipped a secret-named file" "$out" "d1@1"
out=$(cd "$D" && "$BIN" rewind d1@1 2>&1)
contains "rewind plans against such a turn" "$out" "rewind to d1@1"
sgit cat-file --batch-all-objects --batch >"$TMP/objects.dump"
for secret in "$SECRET_KEY" "$SECRET_LITERAL" "pa/ss+word" "$MACHINE_SECRET"; do
  check "no object in the side repo holds $secret" bash -c '! grep -aq -e "$0" "$1"' "$secret" "$TMP/objects.dump"
done
rm -f "$TMP/objects.dump"
check "the side repo passes fsck" sgit fsck --strict --no-dangling
section_end

section "E. a hook from a cwd spelled through a link"
E="$TMP/linke"
mkmin "$E" linke
"$BIN" enroll "$(wp "$E")" >/dev/null 2>&1
SIDE=$(side_of linke)
case ${OSTYPE:-} in
  msys* | cygwin*) cmd //c mklink //J "$(cygpath -w "$TMP/link")" "$(cygpath -w "$E")" >/dev/null ;;
  *) ln -s "$E" "$TMP/link" ;;
esac
printf 'via link\n' >"$E/new.txt"
payload "$TMP/payload-link.json" "$TMP/link" viaj
"$BIN" hook claude <"$TMP/payload-link.json"
if wait_ref_change "$SIDE/refs/baton/viaj" "" 5000; then ok "the turn from a junction or symlink cwd is recorded"; else bad "the turn from a link cwd was dropped"; fi
contains "and holds the file written through the real path" "$(tree_of viaj)" "new.txt"
case ${OSTYPE:-} in
  msys* | cygwin*) cmd //c rmdir "$(cygpath -w "$TMP/link")" >/dev/null 2>&1 ;;
  *) rm -f "$TMP/link" ;;
esac
section_end

section "F. rewind --apply refuses what it would trample"
F="$TMP/rewf"
mkmin "$F" rewf
printf 'keep me\n' >"$F/del.txt"
printf 'i am a file\n' >"$F/swap"
printf 'b1\n' >"$F/b.txt"
"$BIN" enroll "$(wp "$F")" >/dev/null 2>&1
SIDE=$(side_of rewf)
turn_in "$F" f1 "v1" >/dev/null
printf 'b2\n' >"$F/b.txt"
turn_in "$F" f1 "v2" >/dev/null
printf 'b1\n' >"$F/b.txt"
rm "$F/del.txt" "$F/swap"
mkdir "$F/swap"
printf 'inner\n' >"$F/swap/inner.txt"
sleep 0.3
out=$(cd "$F" && "$BIN" rewind f1@1 2>&1)
contains "the plan refuses a path deleted since the last snapshot" "$out" "refuse del.txt: deleted since the last snapshot; take a turn first"
contains "the plan refuses a file that is now a directory" "$out" "refuse swap: a directory or an unreadable file is in the way"
out=$(cd "$F" && "$BIN" rewind f1@1 --apply 2>&1)
rc=$?
if [[ $rc != 0 ]]; then ok "--apply fails"; else bad "--apply fails" "rc=0"; fi
check "the deleted file was not brought back" test ! -e "$F/del.txt"
check "the directory is still a directory" test -f "$F/swap/inner.txt"
eq "no rewind temp file is left behind" "" "$(find "$F" -name '.baton-rewind-*' -not -path '*/.git/*')"
section_end

section "G. a hook from a nested repo that is not enrolled"
G="$TMP/outerg"
mkmin "$G" outerg
mkdir -p "$G/inner"
git init -q "$G/inner"
printf 'i\n' >"$G/inner/i.txt"
git -C "$G/inner" add -A >/dev/null 2>&1
git -C "$G/inner" commit -q -m inner
"$BIN" enroll "$(wp "$G")" >/dev/null 2>&1
SIDE=$(side_of outerg)
payload "$TMP/payload-outer.json" "$G" outer
"$BIN" hook claude <"$TMP/payload-outer.json"
if wait_ref_change "$SIDE/refs/baton/outer" "" 5000; then ok "a hook from the enrolled root is recorded"; else bad "a hook from the enrolled root was dropped"; fi
payload "$TMP/payload-inner.json" "$G/inner" nested
"$BIN" hook claude <"$TMP/payload-inner.json"
sleep 0.8
check "a hook from the nested repo is not recorded on the outer project" test ! -e "$SIDE/refs/baton/nested"
contains "the daemon says why" "$(cat "$BATON_HOME/data/daemon.log")" "inside the nested repo"
out=$(cd "$G/inner" && "$BIN" turn --thread nested2 --message x 2>&1)
contains "baton turn from there says to enroll it" "$out" "run \`baton enroll\` there"
section_end

section "H. a write below a directory the policy now admits is not dropped"
H="$TMP/cachh"
mkmin "$H" cachh
mkdir -p "$H/vendor"
printf 'v1\n' >"$H/vendor/lib.js"
user_config '[snapshot]' 'exclude = ["vendor"]'
"$BIN" enroll "$(wp "$H")" >/dev/null 2>&1
SIDE=$(side_of cachh)
turn_in "$H" h1 "vendor excluded" >/dev/null
lacks "a directory the user config excludes is not stored" "$(tree_of h1)" "vendor/"
printf 'junk\n' >"$H/vendor/junk.js"
sleep 0.4
turn_in "$H" h1 "a write below it" >/dev/null
user_config
turn_in "$H" h1 "exclude removed" >/dev/null
contains "dropping the exclude stores the directory" "$(tree_of h1)" "vendor/lib.js"
printf 'v2\n' >"$H/vendor/lib.js"
sleep 0.4
turn_in "$H" h1 "edit it" >/dev/null
eq "a later edit below it is stored" "v2" "$(blob_of h1 vendor/lib.js)"
LM="$TMP/linkm"
mkmin "$LM" linkm
printf 'dist/\n' >"$LM/.gitignore"
git -C "$LM" add .gitignore >/dev/null 2>&1
git -C "$LM" commit -q -m "ignore dist"
git -C "$LM" worktree add -q "$TMP/linkwt" -b feature
"$BIN" enroll "$(wp "$TMP/linkwt")" >/dev/null 2>&1
SIDE=$(side_of linkm)
mkdir -p "$TMP/linkwt/dist"
printf 'app-v1\n' >"$TMP/linkwt/dist/app.js"
sleep 0.4
turn_in "$TMP/linkwt" h2 "ignored and untracked" >/dev/null
lacks "an ignored, untracked file in a linked worktree is not stored" "$(tree_of h2)" "dist/app.js"
git -C "$TMP/linkwt" add -f dist/app.js >/dev/null 2>&1
git -C "$TMP/linkwt" commit -q -m "track dist/app.js"
turn_in "$TMP/linkwt" h2 "now tracked" >/dev/null
contains "a commit made in a linked worktree starts the file's storage" "$(tree_of h2)" "dist/app.js"
printf 'app-v2\n' >"$TMP/linkwt/dist/app.js"
sleep 0.4
turn_in "$TMP/linkwt" h2 "edit it" >/dev/null
eq "the first edit after that commit is stored" "app-v2" "$(blob_of h2 dist/app.js)"
printf 'app-v3\n' >"$TMP/linkwt/dist/app.js"
sleep 0.4
turn_in "$TMP/linkwt" h2 "edit it again" >/dev/null
eq "and so is the next" "app-v3" "$(blob_of h2 dist/app.js)"
"$BIN" unenroll "$(wp "$TMP/linkwt")" >/dev/null 2>&1
"$BIN" unenroll "$(wp "$H")" >/dev/null 2>&1
section_end

section "I. a global core.excludesFile set or edited after enrollment is honoured"
GX="$TMP/globx"
mkmin "$GX" globx
mkdir -p "$GX/vendor"
printf 'lib1\n' >"$GX/vendor/lib.js"
"$BIN" enroll "$(wp "$GX")" >/dev/null 2>&1
SIDE=$(side_of globx)
DPID=$(daemon_pid)
turn_in "$GX" i1 "nothing ignores it yet" >/dev/null
contains "an untracked directory is stored while nothing ignores it" "$(tree_of i1)" "vendor/lib.js"
printf 'vendor/\n' >"$TMP/globalignore"
printf 'vendor/\n' >"$TMP/globalignore2"
git config --file "$GIT_CONFIG_GLOBAL" core.excludesFile "$(wp "$TMP")/globalignore"
printf 'lib2\n' >"$GX/vendor/lib.js"
sleep 0.4
turn_in "$GX" i1 "key added to the global gitconfig" >/dev/null
lacks "the key added to the global gitconfig takes effect" "$(tree_of i1)" "vendor/"
: >"$TMP/globalignore"
printf 'lib3\n' >"$GX/vendor/lib.js"
sleep 0.4
turn_in "$GX" i1 "excludes file emptied" >/dev/null
eq "an edit to the excludes file it names takes effect" "lib3" "$(blob_of i1 vendor/lib.js)"
git config --file "$GIT_CONFIG_GLOBAL" core.excludesFile "$(wp "$TMP")/globalignore2"
printf 'lib4\n' >"$GX/vendor/lib.js"
sleep 0.4
turn_in "$GX" i1 "key points at another file" >/dev/null
lacks "pointing the key at another file takes effect" "$(tree_of i1)" "vendor/"
git config --file "$GIT_CONFIG_GLOBAL" --unset core.excludesFile
printf 'lib5\n' >"$GX/vendor/lib.js"
sleep 0.4
turn_in "$GX" i1 "key removed" >/dev/null
eq "removing the key takes effect" "lib5" "$(blob_of i1 vendor/lib.js)"
eq "all of it without a daemon restart" "$DPID" "$(daemon_pid)"
"$BIN" unenroll "$(wp "$GX")" >/dev/null 2>&1
section_end

section "J. quoted secrets with spaces and URL passwords with odd characters"
J="$TMP/redj"
mkmin "$J" redj
"$BIN" enroll "$(wp "$J")" >/dev/null 2>&1
SIDE=$(side_of redj)
cat >"$J/.env" <<'EOT'
DB_PASSWORD="my secret passphrase here"
API_KEY='abc def ghi jkl mno'
DATABASE_URL2=postgres://user:sec'ret+pass/word@db.example.com/app
DATABASE_URL3=postgres://user:sec<ret+pass/word@db.example.com/app
DATABASE_URL4=postgres://:empty-user-pass/word+x@db.example.com/app
EOT
sleep 0.4
turn_in "$J" j1 "quoted and odd URL secrets" >/dev/null
env_blob=$(blob_of j1 .env)
contains "a double-quoted value with spaces is cut out whole" "$env_blob" 'DB_PASSWORD="[baton:redacted:assigned-secret]"'
contains "a single-quoted value with spaces is cut out whole" "$env_blob" "API_KEY='[baton:redacted:assigned-secret]'"
contains "a URL password holding a quote" "$env_blob" 'DATABASE_URL2=postgres://user:[baton:redacted:url-password]@db.example.com/app'
contains "a URL password holding a <" "$env_blob" 'DATABASE_URL3=postgres://user:[baton:redacted:url-password]@db.example.com/app'
contains "a URL with an empty user name" "$env_blob" 'DATABASE_URL4=postgres://:[baton:redacted:url-password]@db.example.com/app'
sgit cat-file --batch-all-objects --batch >"$TMP/objects.dump"
for secret in "my secret passphrase" "abc def ghi" "sec'ret" "sec<ret" "empty-user-pass"; do
  check "no object in the side repo holds $secret" bash -c '! grep -aq -e "$0" "$1"' "$secret" "$TMP/objects.dump"
done
rm -f "$TMP/objects.dump"
"$BIN" unenroll "$(wp "$J")" >/dev/null 2>&1
section_end

section "K. on a case-insensitive file system a path is the same file in any case"
KR="$TMP/casek"
mkmin "$KR" casek
if [[ $(git -C "$KR" config core.ignorecase) != true ]]; then
  echo "  skipped: this file system is case sensitive (git did not set core.ignorecase)"
else
  mkdir -p "$KR/sub2"
  printf 'x\n' >"$KR/sub2/f.txt"
  "$BIN" enroll "$(wp "$KR")" >/dev/null 2>&1
  SIDE=$(side_of casek)
  turn_in "$KR" k1 "baseline" >/dev/null
  contains "a file in a plain directory is stored" "$(tree_of k1)" "sub2/f.txt"
  printf 'f.txt\n' >"$KR/sub2/.GitIgnore"
  sleep 0.4
  turn_in "$KR" k1 "an ignore file spelled .GitIgnore" >/dev/null
  lacks "an ignore file in another case drops the file it names" "$(tree_of k1)" "sub2/f.txt"
  KA="$TMP/casea"
  mkmin "$KA" casea
  mkdir -p "$KA/build"
  printf 'build/\n' >"$KA/.gitignore"
  printf 'forced-v1\n' >"$KA/build/Forced.o"
  git -C "$KA" add -f .gitignore build/Forced.o >/dev/null 2>&1
  git -C "$KA" commit -q -m "track a file below an ignored directory"
  "$BIN" enroll "$(wp "$KA")" >/dev/null 2>&1
  SIDE=$(side_of casea)
  turn_in "$KA" k2 "baseline" >/dev/null
  contains "a tracked file below an ignored directory is stored" "$(tree_of k2)" "build/Forced.o"
  mv "$KA/build/Forced.o" "$KA/build/forced.o"
  printf 'forced-v2\n' >"$KA/build/forced.o"
  sleep 0.4
  turn_in "$KA" k2 "renamed to another case, new content" >/dev/null
  tree=$(tree_of k2)
  contains "a case-only rename of it is stored under the new spelling" "$tree" "build/forced.o"
  lacks "and the old spelling leaves the tree" "$tree" "build/Forced.o"
  eq "with the new content" "forced-v2" "$(blob_of k2 build/forced.o)"
  kill_pid "$(daemon_pid)"
  sleep 0.5
  mv "$KA/build/forced.o" "$KA/build/FORCED.o"
  turn_in "$KA" k2 "renamed while the daemon was down" >/dev/null
  tree=$(tree_of k2)
  contains "a rename made while the daemon was down is found by the rescan" "$tree" "build/FORCED.o"
  lacks "and the spelling it replaced leaves the tree" "$tree" "build/forced.o"
  eq "with the content unchanged" "forced-v2" "$(blob_of k2 build/FORCED.o)"
  check "the side repo passes fsck" sgit fsck --strict --no-dangling
  "$BIN" unenroll "$(wp "$KA")" >/dev/null 2>&1
  KB="$TMP/caseb"
  mkmin "$KB" caseb
  printf 'del1\n' >"$KB/Del.txt"
  git -C "$KB" add -A >/dev/null 2>&1
  git -C "$KB" commit -q -m "add Del.txt"
  "$BIN" enroll "$(wp "$KB")" >/dev/null 2>&1
  SIDE=$(side_of caseb)
  turn_in "$KB" k3 "Del.txt" >/dev/null
  mv "$KB/Del.txt" "$KB/del.txt"
  sleep 0.4
  turn_in "$KB" k3 "renamed to del.txt" >/dev/null
  rm "$KB/del.txt"
  sleep 0.4
  out=$(cd "$KB" && "$BIN" rewind k3@1 --apply 2>&1)
  contains "rewind refuses a file deleted under another spelling" "$out" "refuse Del.txt: deleted since the last snapshot; take a turn first"
  lacks "it does not call the renamed file a stray" "$out" "are not in this turn"
  check "and writes nothing" test ! -e "$KB/Del.txt"
  "$BIN" unenroll "$(wp "$KB")" >/dev/null 2>&1
  "$BIN" unenroll "$(wp "$KR")" >/dev/null 2>&1
fi
section_end

SIDE=$(side_of widgets)

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
if [[ $slow != 0 ]]; then
  grep -E ' turn [^ ]+@[0-9]+ ' "$log" | awk 'match($0, /ref=[0-9.]+ms/) && substr($0, RSTART + 4, RLENGTH - 6) + 0 > 1000 { print "    " $0 }'
fi
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
