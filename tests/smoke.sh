#!/usr/bin/env bash
set -euo pipefail

bin="${INDEXSEARCH_BIN:-./indexsearch}"
case "$bin" in
  /*) ;;
  *) bin="$PWD/$bin" ;;
esac
daemon_bin="$(dirname "$bin")/is-daemon"
tool_bin="${INDEXSEARCH_TOOL_BIN:-$(dirname "$bin")/istool}"
grep_bin="${INDEXSEARCH_GREP_BIN:-$(dirname "$bin")/isgrep}"
if [[ ! -x "$daemon_bin" && -x "$daemon_bin.exe" ]]; then
  daemon_bin="$daemon_bin.exe"
fi
if [[ ! -x "$tool_bin" && -x "$tool_bin.exe" ]]; then
  tool_bin="$tool_bin.exe"
fi
if [[ ! -x "$grep_bin" && -x "$grep_bin.exe" ]]; then
  grep_bin="$grep_bin.exe"
fi
if [[ -z "${INDEXSEARCH_GREP_BIN:-}" && ! -x "$grep_bin" ]]; then
  if [[ "$(uname -s)" == MINGW* || "$(uname -s)" == MSYS* ]]; then
    cp "$bin" "$grep_bin.exe"
    grep_bin="$grep_bin.exe"
  else
    ln -sf "$(basename "$bin")" "$grep_bin"
  fi
fi
test_home="$(mktemp -d)"
export HOME="$test_home"

cleanup() {
  "$tool_bin" stop --all >/dev/null 2>&1 || "$daemon_bin" stop --all >/dev/null 2>&1 || true
  rm -rf "${tmp:-}" "${no_cfg_tmp:-}" "${search_no_cfg_tmp:-}" "${grep_no_cfg_tmp:-}" "${backend_auto_tmp:-}" \
    "${home_registry_tmp:-}" "${outside_log_tmp:-}" "${multi_root_tmp:-}" "${git_tmp:-}" "${ue_git_tmp:-}" "${sub_git_tmp:-}" "${watch_tmp:-}" \
    "${compact_watch_tmp:-}" "${config_watch_tmp:-}" "${offline_watch_tmp:-}" \
    "${implicit_watch_tmp:-}" "${clean_tmp:-}" "${all_home:-}" "${all_watch_a:-}" \
    "${all_watch_b:-}" "${registry_home:-}" "${install_tmp:-}" "${install_project:-}" \
    "${skills_home:-}" "${skills_project:-}" "$test_home"
}

assert_daemon_capabilities() {
  local record="$1"
  grep -q '^service_name=is-daemon$' "$record"
  grep -q '^protocol=1$' "$record"
  grep -q '^capabilities=.*search' "$record"
  grep -q '^capabilities=.*update' "$record"
  if [[ "$(uname -s)" != MINGW* && "$(uname -s)" != MSYS* && "$(uname -s)" != CYGWIN* ]]; then
    grep -q '^capabilities=.*direct_stdout' "$record"
  fi
}

write_project_config() {
  local root="$1"
  mkdir -p "$root/.indexsearch"
  cat > "$root/.indexsearch/is-project-config.txt"
}

tmp="$(mktemp -d)"
trap cleanup EXIT

no_cfg_tmp="$(mktemp -d)"
search_no_cfg_tmp="$(mktemp -d)"
printf 'no_search_config_symbol\n' > "$search_no_cfg_tmp/a.txt"
"$bin" -q -F no_search_config_symbol "$search_no_cfg_tmp"
[[ ! -e "$search_no_cfg_tmp/.indexsearch/is-project-config.txt" ]]
(
  cd "$search_no_cfg_tmp"
  "$bin" -q -F no_search_config_symbol
)
[[ ! -e "$search_no_cfg_tmp/.indexsearch/is-project-config.txt" ]]
grep_no_cfg_tmp="$(mktemp -d)"
printf 'grep_no_config_symbol\n' > "$grep_no_cfg_tmp/a.txt"
"$grep_bin" -r -q -F grep_no_config_symbol "$grep_no_cfg_tmp"
[[ ! -e "$grep_no_cfg_tmp/.indexsearch/is-project-config.txt" ]]
if [[ -x "$tool_bin" ]]; then
  backend_auto_tmp="$(mktemp -d)"
  printf 'backend_auto_symbol\n' > "$backend_auto_tmp/a.txt"
  "$tool_bin" search -q -F backend_auto_symbol "$backend_auto_tmp"
  [[ -f "$backend_auto_tmp/.indexsearch/is-project-config.txt" ]]
fi
home_registry_tmp="$(mktemp -d)"
mkdir -p "$home_registry_tmp/.indexsearch/projects" "$home_registry_tmp/work/sub"
printf 'home_registry_symbol\n' > "$home_registry_tmp/work/sub/a.txt"
(
  cd "$home_registry_tmp/work/sub"
  HOME="$home_registry_tmp" "$bin" -q -F home_registry_symbol
)
[[ -f "$home_registry_tmp/work/sub/.indexsearch/is-project-config.txt" ]]
[[ -f "$home_registry_tmp/work/sub/.indexsearch/index.bin" ]]
[[ ! -f "$home_registry_tmp/.indexsearch/is-project-config.txt" ]]
[[ ! -f "$home_registry_tmp/.indexsearch/index.bin" ]]
HOME="$home_registry_tmp" "$tool_bin" stop "$home_registry_tmp/work/sub" >/dev/null 2>&1 || true
printf 'auto_config_symbol\n' > "$no_cfg_tmp/a.txt"
"$tool_bin" index "$no_cfg_tmp" >/dev/null
[[ -f "$no_cfg_tmp/.indexsearch/is-project-config.txt" ]]
"$bin" -q -F auto_config_symbol "$no_cfg_tmp"

ue_git_tmp="$(mktemp -d)"
git -C "$ue_git_tmp" init -q
"$tool_bin" index "$ue_git_tmp" >/dev/null
grep -q '# Local ignore IndexSearch and IndexGraph' "$ue_git_tmp/.git/info/exclude"
grep -qx '/.indexsearch/' "$ue_git_tmp/.git/info/exclude"
[[ -f "$ue_git_tmp/.indexsearch/is-project-config.txt" ]]
"$tool_bin" index "$ue_git_tmp" >/dev/null
[[ "$(grep -cx '/.indexsearch/' "$ue_git_tmp/.git/info/exclude")" -eq 1 ]]

sub_git_tmp="$(mktemp -d)"
git -C "$sub_git_tmp" init -q
mkdir -p "$sub_git_tmp/nested/project"
"$tool_bin" index "$sub_git_tmp/nested/project" >/dev/null
grep -qx '/nested/project/.indexsearch/' "$sub_git_tmp/.git/info/exclude"
git -C "$sub_git_tmp" check-ignore -q nested/project/.indexsearch/is-project-config.txt

help_out="$("$bin" --help)"
grep -q 'search the indexed tree' <<<"$help_out"
grep -q 'Options' <<<"$help_out"
grep -q -- '--files-without-match' <<<"$help_out"
grep -q -- '--count-matches' <<<"$help_out"
grep -q -- '--type-list' <<<"$help_out"
grep -q 'istool <COMMAND>' <<<"$help_out"
! grep -q '<init|' <<<"$help_out"
tool_help="$("$tool_bin" --help)"
grep -q 'Search Frontends' <<<"$tool_help"
grep -q 'istool search --help' <<<"$tool_help"
grep -q 'isgrep \[GREP_OPTIONS\]' <<<"$tool_help"
grep -q 'always expects a subcommand' <<<"$tool_help"
grep_help="$("$grep_bin" --help)"
grep -q 'grep-compatible frontend' <<<"$grep_help"
grep -Fq 'A\|B' <<<"$grep_help"
install_help="$("$tool_bin" install --help)"
grep -q 'install the daemon backend and user-facing commands' <<<"$install_help"
grep -q -- '--dir PATH' <<<"$install_help"
projects_help="$("$tool_bin" projects --help)"
grep -q 'list active project services' <<<"$projects_help"
stop_help="$("$tool_bin" stop --help)"
grep -q -- '--all' <<<"$stop_help"
log_help="$("$tool_bin" log --help)"
grep -q 'print project service activity' <<<"$log_help"
completions_help="$("$tool_bin" completions --help)"
grep -q 'print shell completion script' <<<"$completions_help"
completion_ps="$("$tool_bin" completions powershell)"
grep -q 'Register-ArgumentCompleter' <<<"$completion_ps"
grep -q "'log'" <<<"$completion_ps"
grep -q "'search'" <<<"$completion_ps"
outside_log_tmp="$(mktemp -d)"
if "$tool_bin" log "$outside_log_tmp" >"$outside_log_tmp/log.out" 2>&1; then
  echo "log should fail outside an IndexSearch project" >&2
  exit 1
fi
grep -q 'not in an IndexSearch project' "$outside_log_tmp/log.out"
if (cd "$outside_log_tmp" && "$tool_bin" log >log-cwd.out 2>&1); then
  echo "log should fail when the current directory is outside an IndexSearch project" >&2
  exit 1
fi
grep -q 'not in an IndexSearch project' "$outside_log_tmp/log-cwd.out"
if "$tool_bin" project-log "$outside_log_tmp" >"$outside_log_tmp/project-log.out" 2>&1; then
  echo "project-log should no longer be a management command" >&2
  exit 1
fi
grep -q 'unknown command' "$outside_log_tmp/project-log.out"
color_help="$(env -u NO_COLOR CLICOLOR_FORCE=1 "$bin" --help)"
grep -q $'\033\\[' <<<"$color_help"
plain_help="$(NO_COLOR=1 CLICOLOR_FORCE=1 "$bin" --help)"
! grep -q $'\033\\[' <<<"$plain_help"

write_project_config "$tmp" <<'CFG'
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

"$tool_bin" index "$tmp" >/dev/null
"$bin" -q -F needle "$tmp"
grep -q 'project-service-listen' "$tmp/.indexsearch/project.log"
daemon_pid_after_index="$(sed -n 's/^pid=//p' "$tmp/.indexsearch/search-daemon.txt")"
result="$("$bin" -n -i needle "$tmp")"
grep -q "$tmp/src/a.cc:2:needle here" <<<"$result"
grep -q "$tmp/src/b.txt:1:Needle there" <<<"$result"
! grep -q 'c.bin' <<<"$result"
! grep -q 'out/d.cc' <<<"$result"
sorted_result="$("$bin" --sort path -n -i needle "$tmp")"
[[ "$sorted_result" == "$tmp/src/a.cc:2:needle here"* ]]
mkdir -p "$tmp/src/sub"
printf 'needle nested\n' > "$tmp/src/sub/nested.txt"
"$tool_bin" update --force-scan "$tmp" >/dev/null
daemon_pid_after_delta="$(sed -n 's/^pid=//p' "$tmp/.indexsearch/search-daemon.txt")"
[[ "$daemon_pid_after_delta" == "$daemon_pid_after_index" ]]
"$bin" -q -F 'needle nested' "$tmp"
"$tool_bin" compact "$tmp" >/dev/null
daemon_pid_after_compact="$(sed -n 's/^pid=//p' "$tmp/.indexsearch/search-daemon.txt")"
[[ "$daemon_pid_after_compact" == "$daemon_pid_after_index" ]]
"$bin" -q -F 'needle nested' "$tmp"
grep -q 'project-service-reload reason=index-replaced' "$tmp/.indexsearch/project.log"
subdir_result="$(cd "$tmp/src/sub" && "$bin" -n -F needle)"
[[ "$subdir_result" == 'nested.txt:1:needle nested' ]]
subdir_dot_result="$(cd "$tmp/src/sub" && "$bin" -n -F needle .)"
[[ "$subdir_dot_result" == './nested.txt:1:needle nested' ]]
subdir_files="$(cd "$tmp/src/sub" && "$bin" --files)"
[[ "$subdir_files" == 'nested.txt' ]]
invert_result="$("$bin" -n -v -F needle "$tmp/src/sub" || true)"
if grep -q "$tmp/src/sub/nested.txt:1:needle nested" <<<"$invert_result"; then
  echo "invert match should omit matching lines" >&2
  exit 1
fi
without_match="$("$bin" --files-without-match -F needle "$tmp/src")"
grep -q "$tmp/src/b.txt" <<<"$without_match"
! grep -q "$tmp/src/a.cc" <<<"$without_match"
count_matches="$("$bin" --count-matches -F e "$tmp/src/sub")"
grep -q "$tmp/src/sub/nested.txt:5" <<<"$count_matches"
line_regexp="$("$bin" -x -F 'needle nested' "$tmp/src/sub")"
grep -q "$tmp/src/sub/nested.txt:needle nested" <<<"$line_regexp"
include_zero="$("$bin" --count --include-zero -F missing_symbol "$tmp/src/sub")"
grep -q "$tmp/src/sub/nested.txt:0" <<<"$include_zero"
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

grep_basic_alt="$("$grep_bin" -n 'hello_world\|Needle' "$tmp/src/a.cc" "$tmp/src/b.txt")"
grep -q 'a.cc:1:hello_world = 1' <<<"$grep_basic_alt"
grep -q 'b.txt:1:Needle there' <<<"$grep_basic_alt"
grep_extended_alt="$("$grep_bin" -E -n 'hello_world|Needle' "$tmp/src/a.cc" "$tmp/src/b.txt")"
grep -q 'a.cc:1:hello_world = 1' <<<"$grep_extended_alt"
grep -q 'b.txt:1:Needle there' <<<"$grep_extended_alt"
grep_no_filename="$("$grep_bin" -h -n 'hello_world' "$tmp/src/a.cc")"
[[ "$grep_no_filename" == '1:hello_world = 1' ]]
grep_without_match="$("$grep_bin" -L -F needle "$tmp/src/a.cc" "$tmp/src/b.txt")"
grep -qx 'src/b.txt' <<<"$grep_without_match"
! grep -qx 'src/a.cc' <<<"$grep_without_match"

files="$("$bin" --files "$tmp")"
grep -q 'src/a.cc' <<<"$files"
grep -q 'src/b.txt' <<<"$files"

multi_root_tmp="$(mktemp -d)"
write_project_config "$multi_root_tmp" <<'CFG'
[IndexSearch.files.include]
*.txt
CFG
mkdir -p "$multi_root_tmp/src"
printf 'needle from second root\n' > "$multi_root_tmp/src/second.txt"
"$tool_bin" index "$multi_root_tmp" >/dev/null
multi_root_result="$("$bin" -n -F needle "$tmp/src" "$multi_root_tmp/src")"
grep -q "$tmp/src/a.cc:2:needle here" <<<"$multi_root_result"
grep -q "$multi_root_tmp/src/second.txt:1:needle from second root" <<<"$multi_root_result"
multi_root_mixed_result="$(cd "$(dirname "$tmp")" && "$bin" -n -F needle "$(basename "$tmp")/src" "$multi_root_tmp/src")"
grep -q "$(basename "$tmp")/src/a.cc:2:needle here" <<<"$multi_root_mixed_result"
grep -q "$multi_root_tmp/src/second.txt:1:needle from second root" <<<"$multi_root_mixed_result"
if [[ -x "$tool_bin" ]]; then
  backend_multi_root_result="$("$tool_bin" search -n -F needle "$tmp/src" "$multi_root_tmp/src")"
  grep -q "$tmp/src/a.cc:2:needle here" <<<"$backend_multi_root_result"
  grep -q "$multi_root_tmp/src/second.txt:1:needle from second root" <<<"$backend_multi_root_result"
fi
"$tool_bin" stop "$multi_root_tmp" >/dev/null 2>&1 || true

printf 'needle changed\nfresh_symbol\n' > "$tmp/src/a.cc"
printf 'fresh_symbol added\n' > "$tmp/src/new.cc"
rm "$tmp/src/b.txt"
update_out="$("$tool_bin" update --force-scan "$tmp")"
grep -Eq 'reused|updated index from project service' <<<"$update_out"

fresh="$("$bin" -n fresh_symbol "$tmp")"
grep -q "$tmp/src/a.cc:2:fresh_symbol" <<<"$fresh"
grep -q "$tmp/src/new.cc:1:fresh_symbol added" <<<"$fresh"
! "$bin" -q -F 'Needle there' "$tmp"

status="$("$tool_bin" status "$tmp")"
grep -q 'config_stale: false' <<<"$status"
grep -q 'index_size:' <<<"$status"

git_tmp="$(mktemp -d)"

write_project_config "$git_tmp" <<'CFG'
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

"$tool_bin" index "$git_tmp" >/dev/null
printf 'git_symbol new\n' > "$git_tmp/src/a.cc"
rm "$git_tmp/src/b.txt"
git -C "$git_tmp" add -A
git -C "$git_tmp" -c user.name=IndexSearch -c user.email=indexsearch@example.invalid commit -qm update
[[ -z "$(git -C "$git_tmp" status --porcelain)" ]]
git_update="$("$tool_bin" update --git "$git_tmp")"
grep -q 'modified' <<<"$git_update"
find "$git_tmp/.indexsearch/deltas" -name '*.bin' | grep -q .

git_result="$("$bin" -n git_symbol "$git_tmp")"
grep -q "$git_tmp/src/a.cc:1:git_symbol new" <<<"$git_result"
! "$bin" -q -F 'remove_me' "$git_tmp"

printf 'untracked_symbol\n' > "$git_tmp/src/untracked.cc"
"$tool_bin" update --git "$git_tmp" >/dev/null
"$bin" -q -F 'untracked_symbol' "$git_tmp"

daemon_record="$git_tmp/.indexsearch/search-daemon.txt"
"$bin" -q -F 'git_symbol new' "$git_tmp"
assert_daemon_capabilities "$daemon_record"
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

compact_out="$("$tool_bin" compact "$git_tmp")"
grep -q 'compacted' <<<"$compact_out"
[[ ! -d "$git_tmp/.indexsearch/deltas" ]]
"$bin" -q -F 'git_symbol new' "$git_tmp"
"$bin" -q -F 'untracked_symbol' "$git_tmp"
"$bin" -q -F 'daemon_delta_symbol' "$git_tmp"

watch_tmp="$(mktemp -d)"
write_project_config "$watch_tmp" <<'CFG'
[IndexSearch.paths.ignore]
.git/

[IndexSearch.files.ignore]
*.bin

[IndexSearch.files.include]
*.txt
CFG
printf 'watch_first\n' > "$watch_tmp/a.txt"
"$bin" -q -F watch_first "$watch_tmp"
for _ in 1 2 3 4 5; do
  [[ -f "$watch_tmp/.indexsearch/search-daemon.txt" ]] && break
  sleep 1
done
assert_daemon_capabilities "$watch_tmp/.indexsearch/search-daemon.txt"
watch_pid="$(awk -F= '$1 == "pid" { print $2 }' "$watch_tmp/.indexsearch/search-daemon.txt")"
listed_watch="$("$tool_bin" projects)"
grep -q "pid=$watch_pid" <<<"$listed_watch"
grep -q 'service=is-daemon' <<<"$listed_watch"
grep -q 'protocol=1' <<<"$listed_watch"
grep -q 'capabilities=.*search' <<<"$listed_watch"
grep -q 'capabilities=.*update' <<<"$listed_watch"
mkdir -p "$watch_tmp/sub"
"$bin" -q -F watch_first "$watch_tmp/sub" || true
listed_watch_after_sub="$("$tool_bin" projects)"
[[ "$(grep -c "$watch_tmp" <<<"$listed_watch_after_sub")" == "1" ]]
printf 'watch_preflush\n' > "$watch_tmp/preflush.txt"
sleep 1
if "$bin" -q -F watch_preflush "$watch_tmp"; then
  :
else
  "$tool_bin" update "$watch_tmp" >/dev/null
  "$bin" -q -F watch_preflush "$watch_tmp"
fi
sleep 1
printf 'watch_second\n' > "$watch_tmp/a.txt"
for _ in 1 2 3 4 5 6 7; do
  sleep 1
  if "$bin" -q -F watch_second "$watch_tmp"; then
    break
  fi
done
if ! "$bin" -q -F watch_second "$watch_tmp"; then
  "$tool_bin" update "$watch_tmp" >/dev/null
  "$bin" -q -F watch_second "$watch_tmp"
fi
"$bin" -F watch_second "$watch_tmp" >/dev/null
watch_log="$("$tool_bin" log "$watch_tmp")"
grep -q 'startup-index' <<<"$watch_log"
grep -q 'auto-update' <<<"$watch_log"
grep -q 'search-request' <<<"$watch_log"
grep -q 'search-result code=0' <<<"$watch_log"
grep -q 'matched_files=1' <<<"$watch_log"
daemon_update="$("$tool_bin" update "$watch_tmp")"
grep -q 'project service current' <<<"$daemon_update"
if grep -q 'scanned' <<<"$daemon_update"; then
  echo "project-service update should not scan" >&2
  exit 1
fi
printf 'ignored\n' > "$watch_tmp/ignored.bin"
sleep 2
watch_log="$("$tool_bin" log "$watch_tmp")"
if grep -q 'auto-update-noop' <<<"$watch_log"; then
  echo "log should omit no-op updates" >&2
  exit 1
fi
"$tool_bin" stop "$watch_tmp" >/dev/null

if [[ -x "$daemon_bin" ]]; then
  compact_watch_tmp="$(mktemp -d)"
  config_watch_tmp="$(mktemp -d)"
  write_project_config "$compact_watch_tmp" <<'CFG'
[IndexSearch.files.include]
*.txt
CFG
  printf 'compact_first\n' > "$compact_watch_tmp/a.txt"
  "$tool_bin" index "$compact_watch_tmp" >/dev/null
  "$tool_bin" stop "$compact_watch_tmp" >/dev/null 2>&1 || true
  "$daemon_bin" search-daemon --detach --idle-seconds 1 --compact-delta-count 1 "$compact_watch_tmp" >/dev/null
  for _ in 1 2 3 4 5; do
    [[ -f "$compact_watch_tmp/.indexsearch/search-daemon.txt" ]] && break
    sleep 1
  done
  compact_pid_before="$(awk -F= '$1 == "pid" { print $2 }' "$compact_watch_tmp/.indexsearch/search-daemon.txt")"
  printf 'compact_second\n' > "$compact_watch_tmp/b.txt"
  for _ in 1 2 3 4 5; do
    sleep 1
    compact_log="$("$tool_bin" log "$compact_watch_tmp")"
    grep -q 'auto-compact' <<<"$compact_log" && break
  done
  compact_log="$("$tool_bin" log "$compact_watch_tmp")"
  grep -q 'auto-compact' <<<"$compact_log"
  grep -q 'project-service-reload reason=compact' <<<"$compact_log"
  ! grep -q 'project-service-restart-required reason=compact' <<<"$compact_log"
  "$bin" -q -F compact_second "$compact_watch_tmp"
  compact_pid_after="$(awk -F= '$1 == "pid" { print $2 }' "$compact_watch_tmp/.indexsearch/search-daemon.txt")"
  [[ "$compact_pid_before" == "$compact_pid_after" ]]
  "$tool_bin" stop "$compact_watch_tmp" >/dev/null

  mkdir -p "$config_watch_tmp/.indexsearch"
  cat > "$config_watch_tmp/.indexsearch/is-project-config.txt" <<'CFG'
[IndexSearch.files.include]
*.txt
CFG
  printf 'config_first\n' > "$config_watch_tmp/a.txt"
  "$bin" -q -F config_first "$config_watch_tmp"
  cat > "$config_watch_tmp/.indexsearch/is-project-config.txt" <<'CFG'
[IndexSearch.files.include]
*.cc
CFG
  printf 'config_second\n' > "$config_watch_tmp/b.cc"
  for _ in 1 2 3 4 5; do
    sleep 1
    if "$bin" -q -F config_second "$config_watch_tmp"; then
      break
    fi
  done
  "$bin" -q -F config_second "$config_watch_tmp"
  config_log="$("$tool_bin" log "$config_watch_tmp")"
  [[ "$(grep -c 'startup-index' <<<"$config_log")" -ge 2 ]]
  ! "$bin" -q -F config_first "$config_watch_tmp"
  "$tool_bin" stop "$config_watch_tmp" >/dev/null
  rm -rf "$compact_watch_tmp" "$config_watch_tmp"
fi

offline_watch_tmp="$(mktemp -d)"
write_project_config "$offline_watch_tmp" <<'CFG'
[IndexSearch.files.include]
*.txt
CFG
printf 'offline_old\n' > "$offline_watch_tmp/a.txt"
"$tool_bin" index "$offline_watch_tmp" >/dev/null
"$tool_bin" stop "$offline_watch_tmp" >/dev/null 2>&1 || true
printf 'offline_new\n' > "$offline_watch_tmp/a.txt"
printf 'offline_added\n' > "$offline_watch_tmp/added.txt"
"$bin" -q -F offline_new "$offline_watch_tmp"
"$bin" -q -F offline_added "$offline_watch_tmp"
! "$bin" -q -F offline_old "$offline_watch_tmp"
offline_watch_log="$("$tool_bin" log "$offline_watch_tmp")"
grep -q 'startup-update' <<<"$offline_watch_log"
"$tool_bin" stop "$offline_watch_tmp" >/dev/null

implicit_watch_tmp="$(mktemp -d)"
write_project_config "$implicit_watch_tmp" <<'CFG'
[IndexSearch.files.include]
*.txt
CFG
printf 'implicit_first\n' > "$implicit_watch_tmp/a.txt"
"$bin" -q -F implicit_first "$implicit_watch_tmp"
implicit_watch_log="$("$tool_bin" log "$implicit_watch_tmp")"
grep -q 'startup-index' <<<"$implicit_watch_log"
printf 'implicit_second\n' > "$implicit_watch_tmp/a.txt"
for _ in 1 2 3 4 5; do
  sleep 1
  if "$bin" -q -F implicit_second "$implicit_watch_tmp"; then
    break
  fi
done
if ! "$bin" -q -F implicit_second "$implicit_watch_tmp"; then
  "$tool_bin" update "$implicit_watch_tmp" >/dev/null
  "$bin" -q -F implicit_second "$implicit_watch_tmp"
fi
"$tool_bin" stop "$implicit_watch_tmp" >/dev/null

clean_tmp="$(mktemp -d)"
all_home="$(mktemp -d)"
all_watch_a="$(mktemp -d)"
all_watch_b="$(mktemp -d)"
registry_home="$(mktemp -d)"
mkdir -p "$clean_tmp/.indexsearch"
cat > "$clean_tmp/.indexsearch/is-project-config.txt" <<'CFG'
[IndexSearch.files.include]
*.txt
CFG
printf 'clean_symbol\n' > "$clean_tmp/a.txt"
"$bin" -q -F clean_symbol "$clean_tmp"
clean_dry="$("$tool_bin" clean --dry-run "$clean_tmp")"
grep -q 'would remove' <<<"$clean_dry"
"$tool_bin" clean --yes "$clean_tmp" >/dev/null
[[ -d "$clean_tmp/.indexsearch" ]]
[[ -f "$clean_tmp/.indexsearch/is-project-config.txt" ]]
[[ ! -f "$clean_tmp/.indexsearch/index.bin" ]]
! "$tool_bin" projects | grep -q "$clean_tmp"
"$tool_bin" clean --yes --full "$clean_tmp" >/dev/null
[[ ! -d "$clean_tmp/.indexsearch" ]]

mkdir -p "$registry_home/.indexsearch/projects" "$registry_home/work/subdir"
cat > "$registry_home/.indexsearch/projects/fake.project" <<'EOF'
id=fake
pid=999999
root=/tmp/fake-indexsearch-root
EOF
registry_clean="$(HOME="$registry_home" "$tool_bin" clean --yes "$registry_home/work/subdir")"
grep -q 'cleaned 0 index directories' <<<"$registry_clean"
[[ -f "$registry_home/.indexsearch/projects/fake.project" ]]

for all_watch_tmp in "$all_watch_a" "$all_watch_b"; do
  write_project_config "$all_watch_tmp" <<'CFG'
[IndexSearch.files.include]
*.txt
CFG
  printf 'all_watch_symbol\n' > "$all_watch_tmp/a.txt"
  HOME="$all_home" "$bin" -q -F all_watch_symbol "$all_watch_tmp"
done
all_list_before="$(HOME="$all_home" "$tool_bin" projects)"
grep -q "$all_watch_a" <<<"$all_list_before"
grep -q "$all_watch_b" <<<"$all_list_before"
all_stop="$(HOME="$all_home" INDEXSEARCH_SKIP_STALE_DAEMON_KILL=1 "$tool_bin" stop --all)"
grep -q "$all_watch_a" <<<"$all_stop"
grep -q "$all_watch_b" <<<"$all_stop"
[[ -z "$(HOME="$all_home" "$tool_bin" projects)" ]]

install_tmp="$(mktemp -d)"
install_project="$(mktemp -d)"
skills_home="$(mktemp -d)"
skills_project="$(mktemp -d)"
INDEXSEARCH_SKIP_STALE_DAEMON_KILL=1 "$tool_bin" install --dir "$install_tmp" >/dev/null
install_exe="$install_tmp/indexsearch"
install_alias="$install_tmp/is"
install_grep="$install_tmp/isgrep"
install_daemon="$install_tmp/is-daemon"
install_tool="$install_tmp/istool"
if [[ "$(uname -s)" == MINGW* || "$(uname -s)" == MSYS* ]]; then
  install_exe="$install_exe.exe"
  install_alias="$install_alias.exe"
  install_grep="$install_grep.exe"
  install_daemon="$install_daemon.exe"
  install_tool="$install_tool.exe"
fi
"$install_tool" --version >/dev/null
"$install_exe" --version >/dev/null
"$install_alias" --version >/dev/null
"$install_grep" --version >/dev/null
"$install_daemon" --version >/dev/null
[[ -x "$install_alias" || -L "$install_alias" ]]
[[ -x "$install_grep" || -L "$install_grep" ]]
[[ -x "$install_daemon" || -L "$install_daemon" ]]
[[ -x "$install_tool" || -L "$install_tool" ]]
write_project_config "$install_project" <<'CFG'
[IndexSearch.files.include]
*.txt
CFG
printf 'install_reinstall_symbol\n' > "$install_project/a.txt"
"$install_alias" -q -F install_reinstall_symbol "$install_project"
install_projects_before="$("$install_tool" projects)"
grep -q "$install_project" <<<"$install_projects_before"
grep -q 'alive=true' <<<"$install_projects_before"
install_reinstall_out="$(INDEXSEARCH_SKIP_STALE_DAEMON_KILL=1 "$tool_bin" install --dir "$install_tmp")"
grep -q 'stopped [0-9][0-9]* running project service' <<<"$install_reinstall_out"
"$install_alias" --version >/dev/null
HOME="$skills_home" "$tool_bin" install-skills --target all --scope user >/dev/null
[[ -f "$skills_home/.codex/skills/indexsearch/SKILL.md" ]]
[[ -f "$skills_home/.codex/skills/indexsearch/scripts/prefer-isgrep-hook.py" ]]
[[ -f "$skills_home/.claude/skills/indexsearch/SKILL.md" ]]
[[ -f "$skills_home/.claude/skills/indexsearch/scripts/prefer-isgrep-hook.py" ]]
grep -q 'prefer-isgrep-hook.py' "$skills_home/.claude/settings.json"
hook_payload='{"tool_name":"Bash","tool_input":{"command":"INDEXSEARCH_ALLOW_GREP=1 grep Foo ."}}'
hook_status=0
hook_err="$skills_home/hook.err"
printf '%s\n' "$hook_payload" \
  | python3 "$skills_home/.claude/skills/indexsearch/scripts/prefer-isgrep-hook.py" \
    >/dev/null 2>"$hook_err" || hook_status=$?
[[ "$hook_status" -eq 2 ]]
grep -q 'Use `isgrep`' "$hook_err"
grep -q 'Exit code 1' "$hook_err"
grep -q 'bare `A|B` alternation' "$hook_err"
hook_payload='{"tool_name":"Bash","tool_input":{"command":"rtk grep -n \"Foo|Bar\" ."}}'
hook_status=0
printf '%s\n' "$hook_payload" \
  | python3 "$skills_home/.claude/skills/indexsearch/scripts/prefer-isgrep-hook.py" \
    >/dev/null 2>"$hook_err" || hook_status=$?
[[ "$hook_status" -eq 2 ]]
grep -q 'Blocked bare `rtk grep`' "$hook_err"
grep -q 'add `-E`' "$hook_err"
grep -q 'IndexSearch Agent Instructions' "$skills_home/.config/opencode/AGENTS.md"
"$tool_bin" install-skills --target all --scope project --project "$skills_project" --ue-template >/dev/null
[[ -f "$skills_project/AGENTS.md" ]]
[[ -f "$skills_project/CLAUDE.md" ]]
[[ -f "$skills_project/.claude/skills/indexsearch/SKILL.md" ]]
[[ -f "$skills_project/.claude/skills/indexsearch/scripts/prefer-isgrep-hook.py" ]]
grep -q 'prefer-isgrep-hook.py' "$skills_project/.claude/settings.json"
[[ -f "$skills_project/.cursor/rules/indexsearch.mdc" ]]
[[ -f "$skills_project/.indexsearch/is-project-config.txt" ]]

"$tool_bin" stop "$no_cfg_tmp" >/dev/null 2>&1 || true
"$tool_bin" stop "$tmp" >/dev/null 2>&1 || true
"$tool_bin" stop "$git_tmp" >/dev/null 2>&1 || true
