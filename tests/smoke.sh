#!/usr/bin/env bash
set -euo pipefail

bin="${INDEXSEARCH_BIN:-./indexsearch}"
case "$bin" in
  /*) ;;
  *) bin="$PWD/$bin" ;;
esac
daemon_bin="$(dirname "$bin")/is-daemon"

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

no_cfg_tmp="$(mktemp -d)"
search_no_cfg_tmp="$(mktemp -d)"
trap 'rm -rf "$tmp" "$no_cfg_tmp" "$search_no_cfg_tmp"' EXIT
printf 'no_search_config_symbol\n' > "$search_no_cfg_tmp/a.txt"
if "$bin" -q -F no_search_config_symbol "$search_no_cfg_tmp" 2>/dev/null; then
  echo "search should not create an index project without confirmation" >&2
  exit 1
fi
[[ ! -f "$search_no_cfg_tmp/index-search-project.txt" ]]
if [[ -x "$daemon_bin" ]]; then
  if "$daemon_bin" -q -F no_search_config_symbol "$search_no_cfg_tmp" 2>/dev/null; then
    echo "backend search should not create an index project without confirmation" >&2
    exit 1
  fi
  [[ ! -f "$search_no_cfg_tmp/index-search-project.txt" ]]
fi
printf 'auto_config_symbol\n' > "$no_cfg_tmp/a.txt"
"$bin" index "$no_cfg_tmp" >/dev/null
[[ -f "$no_cfg_tmp/index-search-project.txt" ]]
"$bin" -q -F auto_config_symbol "$no_cfg_tmp"
help_out="$("$bin" --help)"
grep -q 'Commands' <<<"$help_out"
grep -q 'Common Search Options' <<<"$help_out"
! grep -q '<init|' <<<"$help_out"
install_help="$("$bin" install --help)"
grep -q 'install the daemon backend and user-facing commands' <<<"$install_help"
grep -q -- '--dir PATH' <<<"$install_help"
watch_help="$("$bin" watch --help)"
grep -q -- '--compact-delta-count NUM' <<<"$watch_help"
unwatch_help="$("$bin" unwatch --help)"
grep -q -- '--all' <<<"$unwatch_help"
color_help="$(env -u NO_COLOR CLICOLOR_FORCE=1 "$bin" --help)"
grep -q $'\033\\[' <<<"$color_help"
plain_help="$(NO_COLOR=1 CLICOLOR_FORCE=1 "$bin" --help)"
! grep -q $'\033\\[' <<<"$plain_help"

cat > "$tmp/index-search-project.txt" <<'CFG'
[IndexSearch.paths.ignore]
out/

[IndexSearch.files.ignore]
*.bin

[IndexSearch.files.include]
*.txt
*.cc
CFG

mkdir -p "$tmp/src" "$tmp/out"
printf 'hello_world = 1\nneedle here\nFExample::Call()\nQExample::Call()\nRenderThing\nSkeletalMeshComponent\n' > "$tmp/src/a.cc"
printf 'Needle there\n' > "$tmp/src/b.txt"
printf 'needle ignored\n' > "$tmp/src/c.bin"
printf 'needle ignored\n' > "$tmp/out/d.cc"

"$bin" index "$tmp" >/dev/null
"$bin" -q -F needle "$tmp"
result="$("$bin" -n -i needle "$tmp")"
grep -q "$tmp/src/a.cc:2:needle here" <<<"$result"
grep -q "$tmp/src/b.txt:1:Needle there" <<<"$result"
! grep -q 'c.bin' <<<"$result"
! grep -q 'out/d.cc' <<<"$result"
sorted_result="$("$bin" --sort path -n -i needle "$tmp")"
[[ "$sorted_result" == "$tmp/src/a.cc:2:needle here"* ]]
mkdir -p "$tmp/src/sub"
printf 'needle nested\n' > "$tmp/src/sub/nested.txt"
"$bin" update --force-scan "$tmp" >/dev/null
subdir_result="$(cd "$tmp/src/sub" && "$bin" --no-daemon -n -F needle)"
[[ "$subdir_result" == 'nested.txt:1:needle nested' ]]
subdir_dot_result="$(cd "$tmp/src/sub" && "$bin" --no-daemon -n -F needle .)"
[[ "$subdir_dot_result" == './nested.txt:1:needle nested' ]]
subdir_files="$(cd "$tmp/src/sub" && "$bin" --no-daemon --files)"
[[ "$subdir_files" == 'nested.txt' ]]
default_plain="$("$bin" -F needle "$tmp")"
grep -q "$tmp/src/a.cc:needle here" <<<"$default_plain"
! grep -q "$tmp/src/a.cc:2:needle here" <<<"$default_plain"
double_dash_default_path="$(cd "$tmp" && "$bin" -- needle)"
grep -q "src/a.cc:needle here" <<<"$double_dash_default_path"
heading_search="$("$bin" --heading -n -F needle "$tmp")"
[[ "$heading_search" == *"$tmp/src/a.cc"$'\n2:needle here'* ]]
no_heading_search="$("$bin" --no-heading -n -F needle "$tmp")"
grep -q "$tmp/src/a.cc:2:needle here" <<<"$no_heading_search"
context_search="$("$bin" -n -C1 -F needle "$tmp")"
grep -q "$tmp/src/a.cc-1-hello_world = 1" <<<"$context_search"
grep -q "$tmp/src/a.cc:2:needle here" <<<"$context_search"
grep -q "$tmp/src/a.cc-3-FExample::Call()" <<<"$context_search"
default_context_search="$("$bin" -C1 -F needle "$tmp")"
grep -q "$tmp/src/a.cc-hello_world = 1" <<<"$default_context_search"
grep -q "$tmp/src/a.cc:needle here" <<<"$default_context_search"
plain_search="$("$bin" --color=auto -F needle "$tmp")"
! grep -q $'\033\\[' <<<"$plain_search"
colored_search="$(env -u NO_COLOR CLICOLOR_FORCE=1 "$bin" --color=auto -n -F needle "$tmp")"
grep -q $'\033\\[35m' <<<"$colored_search"
grep -q $'\033\\[32m2\033\\[0m' <<<"$colored_search"
grep -q $'\033\\[1;31mneedle\033\\[0m' <<<"$colored_search"
always_colored_search="$(NO_COLOR=1 "$bin" --color=always -F needle "$tmp")"
grep -q $'\033\\[1;31mneedle\033\\[0m' <<<"$always_colored_search"
quiet_hit="$("$bin" -q -F needle "$tmp")"
[[ -z "$quiet_hit" ]]
"$bin" -q -F needle "$tmp"
! "$bin" -q -F missing_symbol "$tmp"

single="$("$bin" -I -n -F 'hello_world' "$tmp/src/a.cc")"
[[ "$single" == '1:hello_world = 1' ]]

regex="$("$bin" -n 'hello.*1' "$tmp")"
grep -q "$tmp/src/a.cc:1:hello_world = 1" <<<"$regex"
qualified_regex="$("$bin" -n 'F[A-Za-z0-9_]+::[A-Za-z0-9_]+\(' "$tmp")"
grep -q "$tmp/src/a.cc:3:FExample::Call()" <<<"$qualified_regex"
generic_qualified_regex="$("$bin" -n '[A-Za-z_][A-Za-z0-9_]*::[A-Za-z0-9_]+\(' "$tmp")"
grep -q "$tmp/src/a.cc:4:QExample::Call()" <<<"$generic_qualified_regex"
alternation_regex="$("$bin" -n '\b(Render|Shader|Nanite|Lumen)[A-Za-z0-9_]*\b' "$tmp")"
grep -q "$tmp/src/a.cc:5:RenderThing" <<<"$alternation_regex"
wordspan_regex="$("$bin" -n 'Skeletal[A-Za-z0-9_]*Component' "$tmp")"
grep -q "$tmp/src/a.cc:6:SkeletalMeshComponent" <<<"$wordspan_regex"

files="$("$bin" --files "$tmp")"
grep -q 'src/a.cc' <<<"$files"
grep -q 'src/b.txt' <<<"$files"

multi_root_tmp="$(mktemp -d)"
trap 'rm -rf "$tmp" "$multi_root_tmp" "$no_cfg_tmp" "$search_no_cfg_tmp"' EXIT
cat > "$multi_root_tmp/index-search-project.txt" <<'CFG'
[IndexSearch.files.include]
*.txt
CFG
mkdir -p "$multi_root_tmp/src"
printf 'needle from second root\n' > "$multi_root_tmp/src/second.txt"
"$bin" index "$multi_root_tmp" >/dev/null
multi_root_result="$("$bin" -n -F needle "$tmp/src" "$multi_root_tmp/src")"
grep -q "$tmp/src/a.cc:2:needle here" <<<"$multi_root_result"
grep -q "$multi_root_tmp/src/second.txt:1:needle from second root" <<<"$multi_root_result"
multi_root_mixed_result="$(cd "$(dirname "$tmp")" && "$bin" -n -F needle "$(basename "$tmp")/src" "$multi_root_tmp/src")"
grep -q "$(basename "$tmp")/src/a.cc:2:needle here" <<<"$multi_root_mixed_result"
grep -q "$multi_root_tmp/src/second.txt:1:needle from second root" <<<"$multi_root_mixed_result"
if [[ -x "$daemon_bin" ]]; then
  backend_multi_root_result="$("$daemon_bin" -n -F needle "$tmp/src" "$multi_root_tmp/src")"
  grep -q "$tmp/src/a.cc:2:needle here" <<<"$backend_multi_root_result"
  grep -q "$multi_root_tmp/src/second.txt:1:needle from second root" <<<"$backend_multi_root_result"
fi
"$bin" unwatch "$multi_root_tmp" >/dev/null 2>&1 || true

printf 'needle changed\nfresh_symbol\n' > "$tmp/src/a.cc"
printf 'fresh_symbol added\n' > "$tmp/src/new.cc"
rm "$tmp/src/b.txt"
update_out="$("$bin" update --force-scan "$tmp")"
grep -Eq 'reused|updated index from watcher' <<<"$update_out"

fresh="$("$bin" -n fresh_symbol "$tmp")"
grep -q "$tmp/src/a.cc:2:fresh_symbol" <<<"$fresh"
grep -q "$tmp/src/new.cc:1:fresh_symbol added" <<<"$fresh"
! "$bin" -q -F 'Needle there' "$tmp"

status="$("$bin" status "$tmp")"
grep -q 'config_stale: false' <<<"$status"

git_tmp="$(mktemp -d)"
trap 'rm -rf "$tmp" "$no_cfg_tmp" "$search_no_cfg_tmp" "$git_tmp"' EXIT

cat > "$git_tmp/index-search-project.txt" <<'CFG'
[IndexSearch.paths.ignore]
.git/

[IndexSearch.files.ignore]
*.bin

[IndexSearch.files.include]
*.txt
*.cc
CFG

git -C "$git_tmp" init -q
mkdir -p "$git_tmp/src"
printf 'git_symbol old\n' > "$git_tmp/src/a.cc"
printf 'remove_me\n' > "$git_tmp/src/b.txt"
git -C "$git_tmp" add .
git -C "$git_tmp" -c user.name=IndexSearch -c user.email=indexsearch@example.invalid commit -qm init

"$bin" index "$git_tmp" >/dev/null
printf 'git_symbol new\n' > "$git_tmp/src/a.cc"
rm "$git_tmp/src/b.txt"
git -C "$git_tmp" add -A
git -C "$git_tmp" -c user.name=IndexSearch -c user.email=indexsearch@example.invalid commit -qm update
[[ -z "$(git -C "$git_tmp" status --porcelain)" ]]
git_update="$("$bin" update --git "$git_tmp")"
grep -q 'modified' <<<"$git_update"
find "$git_tmp/.indexsearch/deltas" -name '*.bin' | grep -q .

git_result="$("$bin" -n git_symbol "$git_tmp")"
grep -q "$git_tmp/src/a.cc:1:git_symbol new" <<<"$git_result"
! "$bin" -q -F 'remove_me' "$git_tmp"

printf 'untracked_symbol\n' > "$git_tmp/src/untracked.cc"
"$bin" update --git "$git_tmp" >/dev/null
"$bin" -q -F 'untracked_symbol' "$git_tmp"

daemon_record="$git_tmp/.indexsearch/search-daemon.txt"
"$bin" -q -F 'git_symbol new' "$git_tmp"
daemon_pid_before="$(awk -F= '$1 == "pid" { print $2 }' "$daemon_record")"
printf 'daemon_delta_symbol\n' > "$git_tmp/src/daemon_delta.txt"
git -C "$git_tmp" add -A
git -C "$git_tmp" -c user.name=IndexSearch -c user.email=indexsearch@example.invalid commit -qm daemon-delta
auto_update_result="$("$bin" --auto-update -F daemon_delta_symbol "$git_tmp")"
grep -q "$git_tmp/src/daemon_delta.txt:daemon_delta_symbol" <<<"$auto_update_result"
plain_daemon_result="$("$bin" -F daemon_delta_symbol "$git_tmp")"
grep -q "$git_tmp/src/daemon_delta.txt:daemon_delta_symbol" <<<"$plain_daemon_result"
daemon_pid_after="$(awk -F= '$1 == "pid" { print $2 }' "$daemon_record")"
[[ "$daemon_pid_before" != "$daemon_pid_after" ]]

compact_out="$("$bin" compact "$git_tmp")"
grep -q 'compacted' <<<"$compact_out"
[[ ! -d "$git_tmp/.indexsearch/deltas" ]]
"$bin" -q -F 'git_symbol new' "$git_tmp"
"$bin" -q -F 'untracked_symbol' "$git_tmp"
"$bin" -q -F 'daemon_delta_symbol' "$git_tmp"

watch_tmp="$(mktemp -d)"
trap 'rm -rf "$tmp" "$no_cfg_tmp" "$search_no_cfg_tmp" "$git_tmp" "$watch_tmp"' EXIT
cat > "$watch_tmp/index-search-project.txt" <<'CFG'
[IndexSearch.paths.ignore]
.git/

[IndexSearch.files.ignore]
*.bin

[IndexSearch.files.include]
*.txt
CFG
printf 'watch_first\n' > "$watch_tmp/a.txt"
"$bin" watch --idle-seconds 1 --compact-delta-count 100 "$watch_tmp" >/dev/null
watch_pid="$(awk -F= '$1 == "pid" { print $2 }' "$watch_tmp/.indexsearch/search-daemon.txt")"
listed_watch="$("$bin" list-watches)"
grep -q "pid=$watch_pid" <<<"$listed_watch"
mkdir -p "$watch_tmp/sub"
covered="$("$bin" watch "$watch_tmp/sub")"
grep -q 'watch already covered' <<<"$covered"
printf 'watch_second\n' > "$watch_tmp/a.txt"
for _ in 1 2 3 4 5; do
  sleep 1
  if "$bin" -q -F watch_second "$watch_tmp"; then
    break
  fi
done
"$bin" -q -F watch_second "$watch_tmp"
watch_log="$("$bin" watch-log "$watch_tmp")"
grep -q 'startup-index' <<<"$watch_log"
grep -q 'auto-update' <<<"$watch_log"
daemon_update="$("$bin" update "$watch_tmp")"
grep -q 'watcher current' <<<"$daemon_update"
if grep -q 'scanned' <<<"$daemon_update"; then
  echo "watch-backed update should not scan" >&2
  exit 1
fi
printf 'ignored\n' > "$watch_tmp/ignored.bin"
sleep 2
watch_log="$("$bin" watch-log "$watch_tmp")"
if grep -q 'auto-update-noop' <<<"$watch_log"; then
  echo "watch-log should omit no-op updates" >&2
  exit 1
fi
"$bin" unwatch "$watch_tmp" >/dev/null

offline_watch_tmp="$(mktemp -d)"
trap 'rm -rf "$tmp" "$no_cfg_tmp" "$search_no_cfg_tmp" "$git_tmp" "$watch_tmp" "$offline_watch_tmp"' EXIT
cat > "$offline_watch_tmp/index-search-project.txt" <<'CFG'
[IndexSearch.files.include]
*.txt
CFG
printf 'offline_old\n' > "$offline_watch_tmp/a.txt"
"$bin" index "$offline_watch_tmp" >/dev/null
printf 'offline_new\n' > "$offline_watch_tmp/a.txt"
printf 'offline_added\n' > "$offline_watch_tmp/added.txt"
"$bin" watch --idle-seconds 1 --compact-delta-count 100 "$offline_watch_tmp" >/dev/null
"$bin" -q -F offline_new "$offline_watch_tmp"
"$bin" -q -F offline_added "$offline_watch_tmp"
! "$bin" -q -F offline_old "$offline_watch_tmp"
offline_watch_log="$("$bin" watch-log "$offline_watch_tmp")"
grep -q 'startup-update' <<<"$offline_watch_log"
"$bin" unwatch "$offline_watch_tmp" >/dev/null

implicit_watch_tmp="$(mktemp -d)"
trap 'rm -rf "$tmp" "$no_cfg_tmp" "$search_no_cfg_tmp" "$git_tmp" "$watch_tmp" "$offline_watch_tmp" "$implicit_watch_tmp"' EXIT
cat > "$implicit_watch_tmp/index-search-project.txt" <<'CFG'
[IndexSearch.files.include]
*.txt
CFG
printf 'implicit_first\n' > "$implicit_watch_tmp/a.txt"
"$bin" -q -F implicit_first "$implicit_watch_tmp"
implicit_watch_log="$("$bin" watch-log "$implicit_watch_tmp")"
grep -q 'startup-index' <<<"$implicit_watch_log"
printf 'implicit_second\n' > "$implicit_watch_tmp/a.txt"
for _ in 1 2 3 4 5; do
  sleep 1
  if "$bin" -q -F implicit_second "$implicit_watch_tmp"; then
    break
  fi
done
"$bin" -q -F implicit_second "$implicit_watch_tmp"
"$bin" unwatch "$implicit_watch_tmp" >/dev/null

clean_tmp="$(mktemp -d)"
all_home="$(mktemp -d)"
all_watch_a="$(mktemp -d)"
all_watch_b="$(mktemp -d)"
registry_home="$(mktemp -d)"
trap 'rm -rf "$tmp" "$no_cfg_tmp" "$search_no_cfg_tmp" "$git_tmp" "$watch_tmp" "$offline_watch_tmp" "$implicit_watch_tmp" "$clean_tmp" "$all_home" "$all_watch_a" "$all_watch_b" "$registry_home"' EXIT
cat > "$clean_tmp/index-search-project.txt" <<'CFG'
[IndexSearch.files.include]
*.txt
CFG
printf 'clean_symbol\n' > "$clean_tmp/a.txt"
"$bin" watch --idle-seconds 1 "$clean_tmp" >/dev/null
clean_dry="$("$bin" clean --dry-run "$clean_tmp")"
grep -q 'would remove' <<<"$clean_dry"
"$bin" clean --yes "$clean_tmp" >/dev/null
[[ ! -d "$clean_tmp/.indexsearch" ]]
[[ -f "$clean_tmp/index-search-project.txt" ]]
! "$bin" list-watches | grep -q "$clean_tmp"

mkdir -p "$registry_home/.indexsearch/watch" "$registry_home/work/subdir"
cat > "$registry_home/.indexsearch/watch/fake.watch" <<'EOF'
id=fake
pid=999999
root=/tmp/fake-indexsearch-root
EOF
registry_clean="$(HOME="$registry_home" "$bin" clean --yes "$registry_home/work/subdir")"
grep -q 'cleaned 0 index directories' <<<"$registry_clean"
[[ -f "$registry_home/.indexsearch/watch/fake.watch" ]]

for all_watch_tmp in "$all_watch_a" "$all_watch_b"; do
  cat > "$all_watch_tmp/index-search-project.txt" <<'CFG'
[IndexSearch.files.include]
*.txt
CFG
  printf 'all_watch_symbol\n' > "$all_watch_tmp/a.txt"
  HOME="$all_home" "$bin" watch "$all_watch_tmp" >/dev/null
done
all_list_before="$(HOME="$all_home" "$bin" list-watches)"
grep -q "$all_watch_a" <<<"$all_list_before"
grep -q "$all_watch_b" <<<"$all_list_before"
all_unwatch="$(HOME="$all_home" "$bin" unwatch --all)"
grep -q "$all_watch_a" <<<"$all_unwatch"
grep -q "$all_watch_b" <<<"$all_unwatch"
[[ -z "$(HOME="$all_home" "$bin" list-watches)" ]]

install_tmp="$(mktemp -d)"
skills_home="$(mktemp -d)"
skills_project="$(mktemp -d)"
trap 'rm -rf "$tmp" "$no_cfg_tmp" "$search_no_cfg_tmp" "$git_tmp" "$watch_tmp" "$offline_watch_tmp" "$implicit_watch_tmp" "$clean_tmp" "$all_home" "$all_watch_a" "$all_watch_b" "$registry_home" "$install_tmp" "$skills_home" "$skills_project"' EXIT
"$bin" install --dir "$install_tmp" >/dev/null
install_exe="$install_tmp/indexsearch"
install_alias="$install_tmp/is"
install_daemon="$install_tmp/is-daemon"
if [[ "$(uname -s)" == MINGW* || "$(uname -s)" == MSYS* ]]; then
  install_exe="$install_exe.exe"
  install_alias="$install_alias.exe"
  install_daemon="$install_daemon.exe"
fi
"$install_exe" --version >/dev/null
"$install_alias" --version >/dev/null
"$install_daemon" --version >/dev/null
[[ -x "$install_alias" || -L "$install_alias" ]]
[[ -x "$install_daemon" || -L "$install_daemon" ]]
HOME="$skills_home" "$bin" install-skills --target all --scope user >/dev/null
[[ -f "$skills_home/.codex/skills/indexsearch/SKILL.md" ]]
[[ -f "$skills_home/.claude/skills/indexsearch/SKILL.md" ]]
grep -q 'IndexSearch Agent Instructions' "$skills_home/.config/opencode/AGENTS.md"
"$bin" install-skills --target all --scope project --project "$skills_project" --ue-template >/dev/null
[[ -f "$skills_project/AGENTS.md" ]]
[[ -f "$skills_project/CLAUDE.md" ]]
[[ -f "$skills_project/.claude/skills/indexsearch/SKILL.md" ]]
[[ -f "$skills_project/.cursor/rules/indexsearch.mdc" ]]
[[ -f "$skills_project/index-search-project.txt" ]]

"$bin" unwatch "$no_cfg_tmp" >/dev/null 2>&1 || true
"$bin" unwatch "$tmp" >/dev/null 2>&1 || true
"$bin" unwatch "$git_tmp" >/dev/null 2>&1 || true
