#!/usr/bin/env bash
set -euo pipefail

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

no_cfg_tmp="$(mktemp -d)"
trap 'rm -rf "$tmp" "$no_cfg_tmp"' EXIT
printf 'auto_config_symbol\n' > "$no_cfg_tmp/a.txt"
./indexsearch index "$no_cfg_tmp" >/dev/null
[[ -f "$no_cfg_tmp/index-search-project.txt" ]]
./indexsearch -q -F auto_config_symbol "$no_cfg_tmp"
help_out="$(./indexsearch --help)"
grep -q 'Commands' <<<"$help_out"
grep -q 'Common Search Options' <<<"$help_out"
! grep -q '<init|' <<<"$help_out"
install_help="$(./indexsearch install --help)"
grep -q 'copy this executable and the is alias' <<<"$install_help"
grep -q -- '--dir PATH' <<<"$install_help"
watch_help="$(./indexsearch watch --help)"
grep -q -- '--compact-delta-count NUM' <<<"$watch_help"
color_help="$(env -u NO_COLOR CLICOLOR_FORCE=1 ./indexsearch --help)"
grep -q $'\033\\[' <<<"$color_help"
plain_help="$(NO_COLOR=1 CLICOLOR_FORCE=1 ./indexsearch --help)"
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

./indexsearch index "$tmp" >/dev/null
result="$(./indexsearch -n -i needle "$tmp")"
grep -q "$tmp/src/a.cc:2:needle here" <<<"$result"
grep -q "$tmp/src/b.txt:1:Needle there" <<<"$result"
! grep -q 'c.bin' <<<"$result"
! grep -q 'out/d.cc' <<<"$result"
mkdir -p "$tmp/src/sub"
printf 'needle nested\n' > "$tmp/src/sub/nested.txt"
./indexsearch update "$tmp" >/dev/null
subdir_result="$(cd "$tmp/src/sub" && "$OLDPWD/indexsearch" --no-daemon -n -F needle)"
[[ "$subdir_result" == 'nested.txt:1:needle nested' ]]
subdir_dot_result="$(cd "$tmp/src/sub" && "$OLDPWD/indexsearch" --no-daemon -n -F needle .)"
[[ "$subdir_dot_result" == './nested.txt:1:needle nested' ]]
subdir_files="$(cd "$tmp/src/sub" && "$OLDPWD/indexsearch" --no-daemon --files)"
[[ "$subdir_files" == 'nested.txt' ]]
default_plain="$(./indexsearch -F needle "$tmp")"
grep -q "$tmp/src/a.cc:needle here" <<<"$default_plain"
! grep -q "$tmp/src/a.cc:2:needle here" <<<"$default_plain"
heading_search="$(./indexsearch --heading -n -F needle "$tmp")"
[[ "$heading_search" == *"$tmp/src/a.cc"$'\n2:needle here'* ]]
no_heading_search="$(./indexsearch --no-heading -n -F needle "$tmp")"
grep -q "$tmp/src/a.cc:2:needle here" <<<"$no_heading_search"
context_search="$(./indexsearch -n -C1 -F needle "$tmp")"
grep -q "$tmp/src/a.cc-1-hello_world = 1" <<<"$context_search"
grep -q "$tmp/src/a.cc:2:needle here" <<<"$context_search"
grep -q "$tmp/src/a.cc-3-FExample::Call()" <<<"$context_search"
default_context_search="$(./indexsearch -C1 -F needle "$tmp")"
grep -q "$tmp/src/a.cc-hello_world = 1" <<<"$default_context_search"
grep -q "$tmp/src/a.cc:needle here" <<<"$default_context_search"
plain_search="$(./indexsearch --color=auto -F needle "$tmp")"
! grep -q $'\033\\[' <<<"$plain_search"
colored_search="$(env -u NO_COLOR CLICOLOR_FORCE=1 ./indexsearch --color=auto -n -F needle "$tmp")"
grep -q $'\033\\[35m' <<<"$colored_search"
grep -q $'\033\\[32m2\033\\[0m' <<<"$colored_search"
grep -q $'\033\\[1;31mneedle\033\\[0m' <<<"$colored_search"
always_colored_search="$(NO_COLOR=1 ./indexsearch --color=always -F needle "$tmp")"
grep -q $'\033\\[1;31mneedle\033\\[0m' <<<"$always_colored_search"
quiet_hit="$(./indexsearch -q -F needle "$tmp")"
[[ -z "$quiet_hit" ]]
./indexsearch -q -F needle "$tmp"
! ./indexsearch -q -F missing_symbol "$tmp"

single="$(./indexsearch -I -n -F 'hello_world' "$tmp/src/a.cc")"
[[ "$single" == '1:hello_world = 1' ]]

regex="$(./indexsearch -n 'hello.*1' "$tmp")"
grep -q "$tmp/src/a.cc:1:hello_world = 1" <<<"$regex"
qualified_regex="$(./indexsearch -n 'F[A-Za-z0-9_]+::[A-Za-z0-9_]+\(' "$tmp")"
grep -q "$tmp/src/a.cc:3:FExample::Call()" <<<"$qualified_regex"
generic_qualified_regex="$(./indexsearch -n '[A-Za-z_][A-Za-z0-9_]*::[A-Za-z0-9_]+\(' "$tmp")"
grep -q "$tmp/src/a.cc:4:QExample::Call()" <<<"$generic_qualified_regex"
alternation_regex="$(./indexsearch -n '\b(Render|Shader|Nanite|Lumen)[A-Za-z0-9_]*\b' "$tmp")"
grep -q "$tmp/src/a.cc:5:RenderThing" <<<"$alternation_regex"
wordspan_regex="$(./indexsearch -n 'Skeletal[A-Za-z0-9_]*Component' "$tmp")"
grep -q "$tmp/src/a.cc:6:SkeletalMeshComponent" <<<"$wordspan_regex"

files="$(./indexsearch --files "$tmp")"
grep -q 'src/a.cc' <<<"$files"
grep -q 'src/b.txt' <<<"$files"

printf 'needle changed\nfresh_symbol\n' > "$tmp/src/a.cc"
printf 'fresh_symbol added\n' > "$tmp/src/new.cc"
rm "$tmp/src/b.txt"
update_out="$(./indexsearch update "$tmp")"
grep -q 'reused' <<<"$update_out"

fresh="$(./indexsearch -n fresh_symbol "$tmp")"
grep -q "$tmp/src/a.cc:2:fresh_symbol" <<<"$fresh"
grep -q "$tmp/src/new.cc:1:fresh_symbol added" <<<"$fresh"
! ./indexsearch -q -F 'Needle there' "$tmp"

status="$(./indexsearch status "$tmp")"
grep -q 'config_stale: false' <<<"$status"

git_tmp="$(mktemp -d)"
trap 'rm -rf "$tmp" "$git_tmp"' EXIT

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

./indexsearch index "$git_tmp" >/dev/null
printf 'git_symbol new\n' > "$git_tmp/src/a.cc"
rm "$git_tmp/src/b.txt"
git -C "$git_tmp" add -A
git -C "$git_tmp" -c user.name=IndexSearch -c user.email=indexsearch@example.invalid commit -qm update
[[ -z "$(git -C "$git_tmp" status --porcelain)" ]]
git_update="$(./indexsearch update --git "$git_tmp")"
grep -q 'modified' <<<"$git_update"
find "$git_tmp/.indexsearch/deltas" -name '*.bin' | grep -q .

git_result="$(./indexsearch -n git_symbol "$git_tmp")"
grep -q "$git_tmp/src/a.cc:1:git_symbol new" <<<"$git_result"
! ./indexsearch -q -F 'remove_me' "$git_tmp"

printf 'untracked_symbol\n' > "$git_tmp/src/untracked.cc"
./indexsearch update --git-untracked "$git_tmp" >/dev/null
./indexsearch -q -F 'untracked_symbol' "$git_tmp"

compact_out="$(./indexsearch compact "$git_tmp")"
grep -q 'compacted' <<<"$compact_out"
[[ ! -d "$git_tmp/.indexsearch/deltas" ]]
./indexsearch -q -F 'git_symbol new' "$git_tmp"
./indexsearch -q -F 'untracked_symbol' "$git_tmp"

watch_tmp="$(mktemp -d)"
trap 'rm -rf "$tmp" "$git_tmp" "$watch_tmp"' EXIT
cat > "$watch_tmp/index-search-project.txt" <<'CFG'
[IndexSearch.paths.ignore]
.git/

[IndexSearch.files.ignore]
*.bin

[IndexSearch.files.include]
*.txt
CFG
printf 'watch_first\n' > "$watch_tmp/a.txt"
./indexsearch watch --idle-seconds 1 --compact-delta-count 100 "$watch_tmp" >/dev/null
mkdir -p "$watch_tmp/sub"
covered="$(./indexsearch watch "$watch_tmp/sub")"
grep -q 'watch already covered' <<<"$covered"
printf 'watch_second\n' > "$watch_tmp/a.txt"
for _ in 1 2 3 4 5; do
  sleep 1
  if ./indexsearch -q -F watch_second "$watch_tmp"; then
    break
  fi
done
./indexsearch -q -F watch_second "$watch_tmp"
watch_log="$(./indexsearch watch-log "$watch_tmp")"
grep -q 'initial-index' <<<"$watch_log"
grep -q 'auto-update' <<<"$watch_log"
./indexsearch unwatch "$watch_tmp" >/dev/null

install_tmp="$(mktemp -d)"
skills_home="$(mktemp -d)"
skills_project="$(mktemp -d)"
trap 'rm -rf "$tmp" "$git_tmp" "$watch_tmp" "$install_tmp" "$skills_home" "$skills_project"' EXIT
./indexsearch install --dir "$install_tmp" >/dev/null
"$install_tmp/indexsearch" --version >/dev/null
"$install_tmp/is" --version >/dev/null
if [[ "$(uname -s)" != MINGW* && "$(uname -s)" != MSYS* ]]; then
  [[ -L "$install_tmp/is" ]]
fi
HOME="$skills_home" ./indexsearch install-skills --target all --scope user >/dev/null
[[ -f "$skills_home/.codex/skills/indexsearch/SKILL.md" ]]
[[ -f "$skills_home/.claude/skills/indexsearch/SKILL.md" ]]
grep -q 'IndexSearch Agent Instructions' "$skills_home/.config/opencode/AGENTS.md"
./indexsearch install-skills --target all --scope project --project "$skills_project" --ue-template >/dev/null
[[ -f "$skills_project/AGENTS.md" ]]
[[ -f "$skills_project/CLAUDE.md" ]]
[[ -f "$skills_project/.claude/skills/indexsearch/SKILL.md" ]]
[[ -f "$skills_project/.cursor/rules/indexsearch.mdc" ]]
[[ -f "$skills_project/index-search-project.txt" ]]
