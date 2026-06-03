# IndexSearch

[![Build](https://github.com/Abyss116/IndexSearch/actions/workflows/release.yml/badge.svg?branch=main)](https://github.com/Abyss116/IndexSearch/actions/workflows/release.yml?query=branch%3Amain)
[![License: MIT OR Apache-2.0](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg)](#license)

IndexSearch is a fast indexed search tool for very large source trees. It is
designed to feel close to `rg`, but repeated searches use a persistent project
index and a per-project daemon instead of walking the filesystem every time.

Use the short command:

```bash
is -n "SomeSymbol" .
is -i -w -g "*.cpp" "render pass" .
is --files .
isgrep -n "A\\|B" file.txt
```

## Install

macOS / Linux:

```bash
curl -fsSL https://raw.githubusercontent.com/Abyss116/IndexSearch/main/install.sh | sh
```

Windows PowerShell:

```powershell
irm https://raw.githubusercontent.com/Abyss116/IndexSearch/main/install.ps1 | iex
```

Homebrew:

```bash
brew tap Abyss116/indexsearch
brew install indexsearch
```

WinGet, after package-manager moderation has accepted the manifest:

```powershell
winget install --id Abyss116.IndexSearch -e
```

Direct downloads:

- [Linux x86_64](https://github.com/Abyss116/IndexSearch/releases/latest/download/indexsearch-linux-x86_64.tar.gz)
- [macOS arm64](https://github.com/Abyss116/IndexSearch/releases/latest/download/indexsearch-macos-aarch64.tar.gz)
- [macOS x86_64](https://github.com/Abyss116/IndexSearch/releases/latest/download/indexsearch-macos-x86_64.tar.gz)
- [Windows x86_64](https://github.com/Abyss116/IndexSearch/releases/latest/download/indexsearch-windows-x86_64.zip)

After extracting a direct-download archive, run:

```bash
./istool install
```

The installer places `istool`, `indexsearch`, `is`, `isgrep`, and `is-daemon` in a
user-writable bin directory. On Windows, `is` is a native `is.exe`, not an
`is.cmd` wrapper.

## Quick Start

```bash
cd /path/to/large/repo
istool index .
is -n "SomeSymbol" .
```

The first `is PATTERN` inside a project automatically starts a per-project
daemon. That daemon keeps the index mmaped, watches filesystem changes, serves
future searches, and can compact deltas while idle.

If no IndexSearch project exists above the current directory, interactive `is`
asks whether to create one in the current directory. `istool index .` always
rebuilds the base index explicitly.

When `is` is used non-interactively by coding agents, missing project config is
created automatically and the first search builds the index before returning
results.

Manage project services:

```bash
istool projects
istool log .
istool stop .
istool stop --all
```

`stop --all` stops registered project services and also makes a best-effort pass
over stale `is-daemon` / `search-daemon` processes left by older versions.

Shell completion scripts can be generated from `istool`:

```powershell
istool completions powershell >> $PROFILE
```

```bash
istool completions bash > ~/.local/share/bash-completion/completions/istool
istool completions zsh > ~/.zfunc/_istool
istool completions fish > ~/.config/fish/completions/istool.fish
```

## Configuration

Project rules live in `.indexsearch/is-project-config.txt`.

```ini
[IndexSearch.paths.ignore]
.git/
out/

[IndexSearch.paths.live]
run-logs/
shader-debug/

[IndexSearch.files.ignore]
*.png
*.pdb

[IndexSearch.files.include]
*
```

`IndexSearch.paths.live` is for high-churn or generated text directories that
should not be persisted into the project index. Explicit searches inside those
paths fall back to `rg`, for example `is Error Saved/Logs`, while ordinary
project searches stay fast and stable. The bundled Unreal Engine template uses
this for logs and shader debug output, while excluding `Saved/` from persistent
indexing.

When a command names multiple explicit paths, IndexSearch splits the work: paths
covered by `IndexSearch.paths.ignore`, `IndexSearch.paths.live`, or file
include/exclude rules fall back to the external stream searcher, while indexed
paths continue through the daemon. `is` uses `rg`; `isgrep` translates compatible
grep syntax to `rg` as well, and only falls back to system `grep` for grep-only
features that cannot be translated safely.

For Unreal Engine source trees, copy the bundled template into the project
root's `.indexsearch/` directory:

```bash
mkdir -p /path/to/UnrealEngine/.indexsearch
cp templates/unreal-engine/is-project-config.txt /path/to/UnrealEngine/.indexsearch/is-project-config.txt
cd /path/to/UnrealEngine
istool index .
```

When indexing a Git project, IndexSearch also adds an anchored local ignore to
`.git/info/exclude`, for example `/.indexsearch/` at the repository root or
`/nested/project/.indexsearch/` for a project rooted in a subdirectory.

If the first interactive search discovers an Unreal Engine root or a `.uproject`
root, the generated config uses the UE template automatically.

## Updating

```bash
istool update .
istool update --git .
istool compact .
```

`update` refreshes an existing index. If the daemon is running, it first flushes
pending filesystem events, so normal edit-and-search workflows stay current
without a full tree scan.

By default, `update` reconciles against the filesystem for correctness across
Git and non-Git trees. `update --git` remains an explicit fast path after
`git pull`, checkout, or rebase. `compact` folds delta indexes back into the
base index.

## rg-Like Usage

Common flags:

```bash
is -F "literal" .
is -i "case insensitive" .
is -w Actor .
is -g "*.cpp" Nanite .
is -n -C 3 "SomeSymbol" .
is --json "SomeSymbol" .
is -v -F "exclude this line" .
is --files-without-match "TODO" .
is --count-matches -F "Tick" .
is -x -F "exact whole line" .
```

When stdout is a terminal, output is grouped by file like `rg --heading`. When
stdout is captured or piped, output uses flat `path:line:match` rows. Use
`--heading`, `--no-heading`, `-n`, and `-N` to override.

For a pattern that starts with punctuation or looks like an option, use `--`:

```bash
is -- "--help" .
```

Unsupported rg flags are logged in the project log and ignored when that is
safe, so agent and editor integrations can keep running. Use `rg` for PCRE-only
patterns, multiline matching, preprocessors, archive search, or other behavior
that must exactly match ripgrep.

## grep-Compatible Usage

Use `isgrep` when replacing an existing `grep` command or when you want grep
option spellings:

```bash
isgrep -n "IndexGraph\\|agent interface\\|context\\|files" MEMORY.md
isgrep -E -n "IndexGraph|agent interface|context|files" MEMORY.md
isgrep -r -n --include="*.rs" "SomeSymbol" .
```

`isgrep` defaults to grep Basic Regex syntax, so `A\|B` is translated to the
Rust regex alternation used by IndexSearch. It also maps grep-specific flags
whose meanings conflict with `is`, such as `grep -h` and `grep -L`. For grep
semantics that the indexed backend cannot provide, such as PCRE mode,
backreferences, or null-data mode, `isgrep` falls back to the system `grep`
when available.

For pipeline input, the persistent project index is not used. `is` forwards
rg-style stdin searches to `rg`; `isgrep` translates compatible grep-style
stdin searches to `rg` too, and uses system `grep` only for grep-only semantics:

```bash
git diff | is -n "SomeSymbol"
git diff | isgrep -n "SomeSymbol"
```

Claude Code installs get an additional guardrail: `istool install-skills
--target claude` copies a `PreToolUse` hook that blocks bare Bash `rg`/`ripgrep`
and `grep`/`egrep`/`fgrep` commands. Retry ordinary local source searches with
`is` or `isgrep`. Set `INDEXSEARCH_ALLOW_RG=1` or `INDEXSEARCH_ALLOW_GREP=1`
only when exact ripgrep or grep semantics are intentionally required.

## Profiling

```bash
istool index --profile .
istool update --profile .
is --profile -n -g "*.cpp" Nanite .
```

`profile:` lines are printed to stderr and are intended for sharing performance
reports from large repositories.

## Performance Snapshot

Local Unreal Engine benchmark on macOS, hot filesystem cache, stdout redirected
to `/dev/null`:

| Workload | `is` | qgrep | `rg` |
| --- | ---: | ---: | ---: |
| Fresh index | 10.57s | about 21s | n/a |
| Compact | 2.31s | n/a | n/a |
| `Nanite` | 7.61ms | 21.52ms | 3139.98ms |
| `SkeletalMeshComponent` | 7.46ms | 19.53ms | 3216.95ms |
| missing literal | 2.63ms | 13.64ms | 3194.67ms |
| qualified-call regex | 85.05ms | 357.42ms | 3217.22ms |

To reproduce the search benchmark:

```bash
python3 scripts/benchmark-ue.py /path/to/UnrealEngine --prepare-qgrep \
  --search-repeats 7 --rg-repeats 3
```

## Build

```bash
cargo build --release
cargo test --locked
./tests/smoke.sh
./target/release/istool --version
```

Local builds append build metadata to the displayed version, for example
`0.4.7+build.1770000000.g8d2644d.dirty`. The package/release version remains
plain SemVer for package managers.

Tagged pushes create GitHub Releases with Linux, macOS, and Windows archives.

## Agent Skills

Bundled instructions for Codex, Claude Code, OpenCode, and Cursor can be
installed with:

```bash
istool install-skills
istool install-skills --target all --scope project --project /path/to/project --ue-template
```

Claude Code is the strictest target because it supports a `PreToolUse` hook for
Bash commands. Codex and OpenCode receive skill/`AGENTS.md` instructions, and
Cursor receives an always-on rule file; those surfaces guide the agent but do
not provide the same hard interception as Claude's hook.

## License

IndexSearch is distributed under the terms of both the MIT license and the
Apache License 2.0. You may choose either license; see `LICENSE-MIT` and
`LICENSE-APACHE`.

The references to ripgrep and qgrep are compatibility and benchmark references
only; their source code is not vendored into IndexSearch.
