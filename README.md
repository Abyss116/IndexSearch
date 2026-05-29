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

The installer places `istool`, `indexsearch`, `is`, and `is-daemon` in a
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

Project rules live in `index-search-project.txt`.

```ini
[IndexSearch.paths.ignore]
.git/
out/

[IndexSearch.files.ignore]
*.png
*.pdb

[IndexSearch.files.include]
*
```

For Unreal Engine source trees, copy the bundled template:

```bash
cp templates/unreal-engine/index-search-project.txt /path/to/UnrealEngine/index-search-project.txt
cd /path/to/UnrealEngine
istool index .
```

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

Tagged pushes create GitHub Releases with Linux, macOS, and Windows archives.

## Agent Skills

Bundled instructions for Codex, Claude Code, OpenCode, and Cursor can be
installed with:

```bash
istool install-skills
istool install-skills --target all --scope project --project /path/to/project --ue-template
```

## License

IndexSearch is distributed under the terms of both the MIT license and the
Apache License 2.0. You may choose either license; see `LICENSE-MIT` and
`LICENSE-APACHE`.

The references to ripgrep and qgrep are compatibility and benchmark references
only; their source code is not vendored into IndexSearch.
