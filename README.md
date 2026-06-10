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
brew trust Abyss116/indexsearch
brew install indexsearch
```

`brew trust` is a one-time local trust grant for the third-party tap. Homebrew
may require it before future `brew update` / `brew upgrade` runs can update the
formula.

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
asks whether to create one in the current directory. Choosing `n` runs the
current search through the external compatible searcher instead. `istool index .`
always rebuilds the base index explicitly.

When `is` or `isgrep` is used non-interactively by coding agents outside an
IndexSearch project, it does not create project config. The one-off search is
routed internally through `rg` or `grep` so the command still returns results
without leaving `.indexsearch/` behind.

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
**/Intermediate/

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
paths are routed internally through `rg`, for example `is Error Saved/Logs`,
while ordinary project searches stay fast and stable. The bundled Unreal Engine
template uses this for logs and shader debug output, while excluding `Saved/`
from persistent indexing.

Use `**/Name/` for a directory name that should match at any depth, such as
`**/DerivedDataCache/`. Directory patterns with a trailing slash also cover the
directory's full subtree.

When a command names multiple explicit paths, IndexSearch splits the work: paths
covered by `IndexSearch.paths.ignore`, `IndexSearch.paths.live`, or file
include/exclude rules are routed internally through the external stream searcher,
while indexed paths continue through the daemon. `is` uses `rg`; `isgrep`
translates compatible grep syntax to `rg` as well, and invokes system `grep`
internally for grep-only features that cannot be translated safely.

For Unreal Engine source trees, copy the bundled template into the project
root's `.indexsearch/` directory:

```bash
mkdir -p /path/to/UnrealEngine/.indexsearch
cp templates/unreal-engine/is-project-config.txt /path/to/UnrealEngine/.indexsearch/is-project-config.txt
cd /path/to/UnrealEngine
istool index .
```

When creating a project, IndexSearch writes `.indexsearch/.gitignore` with `*`
so local index files stay out of Git status even in repositories that use
allow-list style root `.gitignore` rules.

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
Rust regex alternation used by IndexSearch. For rg-style, RTK grep-style, or
extended-regex patterns with bare `A|B` alternation, keep or add `-E`; plain
`isgrep "A|B"` follows grep BRE semantics and searches for a literal `|`.
It also maps grep-specific flags whose meanings conflict with `is`, such as
`grep -h` and `grep -L`. For grep semantics that the indexed backend cannot
provide, such as PCRE mode, backreferences, or null-data mode, `isgrep` falls
back to the system `grep` when available.

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
`is` or `isgrep`; they route stdin, live/generated paths, ignored explicit
paths, and compatible translated grep syntax through the necessary external
search internally. Exit code 1 with no output means no matches, not a reason to
rerun the same local search with bare `rg` or `grep`.

## Profiling

```bash
istool index --profile .
istool update --profile .
is --profile -n -g "*.cpp" Nanite .
```

`profile:` lines are printed to stderr and are intended for sharing performance
reports from large repositories.

## Performance Snapshot

Local Unreal Engine benchmark on macOS, hot filesystem cache. The `is` column
uses median `is --stats` search time with stdout discarded, matching the timing
shown by the CLI. qgrep, `rg`, and grep columns are median process wall times.
The grep column uses macOS `/usr/bin/grep` over the same pre-enumerated
benchmark file set because BSD grep does not provide recursive include/exclude
glob options.

| Workload | `is` | qgrep | `rg` | grep | vs qgrep | vs `rg` | vs grep |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| Fresh index / qgrep update | 10.56s | 18.28s | n/a | n/a | 1.7x | n/a | n/a |
| `Nanite` | 3.71ms | 23.47ms | 2784.08ms | 32528.27ms | 6.3x | 751x | 8772x |
| `SkeletalMeshComponent` | 3.33ms | 18.04ms | 2769.81ms | 33534.75ms | 5.4x | 832x | 10067x |
| missing literal | 0.08ms | 14.86ms | 2767.36ms | 33634.95ms | 186x | 34592x | 420437x |
| qualified-call regex | 77.61ms | 358.93ms | 3030.85ms | 85606.64ms | 4.6x | 39x | 1103x |
| `Nanite` in `*.cpp` | 1.96ms | 24.03ms | 967.51ms | 5492.97ms | 12.2x | 493x | 2797x |

The unrestricted rows use each tool's closest CLI-equivalent filtering rules,
so matched-line counts can differ slightly. The `*.cpp` row is a stricter
same-file-set comparison and matched 10,136 lines for all four tools.

To reproduce the table:

```bash
python3 scripts/benchmark-ue.py /path/to/UnrealEngine --prepare-qgrep \
  --search-repeats 5 --rg-repeats 3 --grep-repeats 1 \
  --case 'Literal: common token' \
  --case 'Literal: long symbol' \
  --case 'Literal: missing' \
  --case 'Regex: qualified call' \
  --case 'Glob: *.cpp literal'
```

## Build

```bash
cargo build --release
cargo test --locked
./tests/smoke.sh
./target/release/istool --version
```

Local builds append build metadata to the displayed version, for example
`0.4.8+build.1770000000.g8d2644d.dirty`. The package/release version remains
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
