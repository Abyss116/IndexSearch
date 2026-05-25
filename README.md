# IndexSearch

[![Build](https://github.com/Abyss116/IndexSearch/actions/workflows/build.yml/badge.svg?branch=main)](https://github.com/Abyss116/IndexSearch/actions/workflows/build.yml?query=branch%3Amain)
[![License: MIT OR Apache-2.0](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg)](#license)

IndexSearch is a Rust command line search tool for very large source trees. It
keeps a persistent binary trigram index plus stored text snapshots, so repeated
searches avoid walking the filesystem. The CLI intentionally follows common
`rg` output conventions closely enough to stand in for `rg` on large indexed
codebases.

The short command is `is`; the full command is `indexsearch`.

## Install

Prebuilt binaries are attached to the
[latest GitHub Release](https://github.com/Abyss116/IndexSearch/releases/latest).

Homebrew:

```bash
brew tap Abyss116/indexsearch
brew install indexsearch
```

WinGet, after the manifest is accepted by the Windows Package Manager community
repository:

```powershell
winget install --id Abyss116.IndexSearch -e
```

Direct downloads:

- [Linux x86_64](https://github.com/Abyss116/IndexSearch/releases/latest/download/indexsearch-linux-x86_64.tar.gz)
- [macOS arm64](https://github.com/Abyss116/IndexSearch/releases/latest/download/indexsearch-macos-aarch64.tar.gz)
- [macOS x86_64](https://github.com/Abyss116/IndexSearch/releases/latest/download/indexsearch-macos-x86_64.tar.gz)
- [Windows x86_64](https://github.com/Abyss116/IndexSearch/releases/latest/download/indexsearch-windows-x86_64.zip)

Continuous builds are available from the
[GitHub Actions build workflow](https://github.com/Abyss116/IndexSearch/actions/workflows/build.yml).

After extracting a direct-download archive, you can copy the extracted binary
into a user-writable bin directory:

```bash
./indexsearch install
```

This self-copy install puts `indexsearch` and the short `is` alias into
`~/.local/bin` on macOS/Linux or `%USERPROFILE%\.local\bin` on Windows. Use
`indexsearch install --dir PATH` to override the install directory. Package
manager installs already put `indexsearch` and `is` on PATH and do not need
this step. On Windows the alias is a native `is.exe`, not an `is.cmd` wrapper,
so PowerShell metacharacters inside quoted patterns are not re-parsed by
`cmd.exe`.

## Quick Start

```bash
cd /path/to/large/repo
is index .
is -n "SomeSymbol" .
is -i -w -g "*.cpp" "render pass" .
is --files .
```

If `index-search-project.txt` does not exist, `index`, `update`, `watch`, and
search-time auto-indexing create a default config before building the index.
Edit that file and rerun `is index .` or `is update .` to rebuild with new
rules.

Minimal project file:

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

## Search Freshness

`is index .` rebuilds the base index from scratch.

`is update .` refreshes an existing index. It compares stored path, `mtime`, and
size metadata, reuses unchanged snapshots, reads only changed or new files, and
drops deleted or newly ignored files.

For Git worktrees:

```bash
is update --git .
is update --git-untracked .
```

`update --git` records the last indexed `HEAD` and can catch clean committed
changes from `git pull`, `checkout`, and `rebase`. It writes small delta
segments under `.indexsearch/deltas/` when possible. Use `is compact .` to fold
deltas back into the base index.

For active large repositories:

```bash
is watch .
is list-watches
is watch-log .
is unwatch .
```

The watcher writes batched delta updates on file events and can compact during
idle periods. If no base index exists, `is watch .` builds it first. Overlapping
watches are normalized so a parent watch covers child directories.

Useful watcher knobs:

```bash
is watch . --idle-seconds 5 --compact-delta-count 16 --compact-delta-bytes 256mb
```

## Search Daemon

Hot searches automatically try a per-project search daemon when an existing
index is present. The daemon keeps the mmap-backed index open and serves
requests over localhost. This trims repeated index-open work, but every shell
command still starts a small client process, parses arguments, reads the daemon
record, checks the binary/index fingerprint, and performs local IPC. That fixed
client-side cost is why daemon speedups are useful but not dramatic for already
sub-10ms searches.

Use either form to bypass the daemon:

```bash
is --no-daemon -F "SomeSymbol" .
INDEXSEARCH_NO_DAEMON=1 is -F "SomeSymbol" .
```

Daemon records live in `.indexsearch/search-daemon.txt`. If `indexsearch
install` replaces the executable, or if the base index is rebuilt/compacted, the
next search detects the fingerprint mismatch, stops the old daemon, and starts a
fresh one.

## Unreal Engine

This repository includes a UE-oriented template:

```bash
cp templates/unreal-engine/index-search-project.txt /path/to/UnrealEngine/index-search-project.txt
cd /path/to/UnrealEngine
is watch .
```

The template keeps source, shader, config, plugin, project, script, and
build-rule files searchable while skipping generated folders, binary assets,
archives, object files, and debug artifacts. The same template is bundled inside
the agent skill at
`skills/indexsearch/assets/unreal-engine-index-search-project.txt`.

## Agent Skill

The repository includes reusable agent instructions:

- `skills/indexsearch/SKILL.md` for Codex and Claude Code style skill loaders.
- `agent-rules/AGENTS.md` for tools that read `AGENTS.md`, including OpenCode
  and Cursor.
- `agent-rules/CLAUDE.md` for Claude Code project instructions.
- `agent-rules/cursor/indexsearch.mdc` for Cursor Project Rules.

Install them with:

```bash
is install-skills
is install-skills --target codex --scope user
is install-skills --target claude --scope user
is install-skills --target opencode --scope user
is install-skills --target all --scope project --project /path/to/project --ue-template
```

## Performance

Benchmarks below were run on a local Unreal Engine checkout at
`/Users/abyss/Projects/UnrealEngine` on macOS, with hot filesystem cache and
stdout redirected to `/dev/null`.

- Repository size: 289 GB.
- IndexSearch indexed files: 196,919.
- qgrep indexed files: 196,900 using near-identical UE-oriented include/exclude
  rules translated from `index-search-project.txt`.
- Search timings are median wall-clock time: 5 runs for IndexSearch/qgrep and 3
  runs for `rg`.
- Match counts differ slightly where the tools' glob and output semantics are
  not perfectly identical; the `*.cpp` constrained row matches exactly.

### Index And Update

| Operation | IndexSearch | qgrep | Notes |
| --- | ---: | ---: | --- |
| Fresh index | 12.62s | 21.50s | IndexSearch timing: scan 3.38s, process 6.16s, write 3.08s |
| No-change update | 0.34s | 4.19s | IndexSearch reused the existing index with no file scan work |
| No-change `update --git` | 0.50s | n/a | Git changed-path check only |

### Search

| Workload | Pattern | Matches `is/qgrep/rg` | `is` | qgrep | `rg` | `is` vs qgrep |
| --- | --- | ---: | ---: | ---: | ---: | ---: |
| Literal: common token | `Nanite` | 14664 / 14672 / 13013 | 6.77ms | 20.19ms | 3040.87ms | 3.0x |
| Literal: long symbol | `SkeletalMeshComponent` | 7593 / 7593 / 7592 | 6.30ms | 17.99ms | 2997.93ms | 2.9x |
| Literal: missing | `DefinitelyMissingIndexSearchNeedle` | 0 / 0 / 0 | 3.70ms | 10.95ms | 3001.30ms | 3.0x |
| Case-insensitive literal | `skeletalmeshcomponent` | 7603 / 7603 / 7602 | 6.64ms | 19.01ms | 3041.92ms | 2.9x |
| Word regex | `\bActor\b` | 23674 / 23677 / 23664 | 33.01ms | 55.19ms | 3009.37ms | 1.7x |
| Regex: alternation | `(Nanite\|Lumen\|SkeletalMeshComponent)` | 34487 / 34498 / 31426 | 26.69ms | 120.20ms | 2986.67ms | 4.5x |
| Regex: prefix/suffix | `Skeletal[A-Za-z0-9_]*Component` | 7917 / 7917 / 7916 | 11.20ms | 21.27ms | 3013.93ms | 1.9x |
| Regex: qualified call | `[A-Za-z_][A-Za-z0-9_]*::[A-Za-z0-9_]+\(` | 1487173 / 1487316 / 1481426 | 124.77ms | 365.74ms | 3112.35ms | 2.9x |
| Glob: `*.cpp` literal | `Nanite` in `*.cpp` | 10061 / 10061 / 10061 | 6.44ms | 21.25ms | 1320.87ms | 3.3x |

For `-q` existence checks, IndexSearch stops as soon as a verified match is
found. Quiet timings are median wall-clock time across 31 IndexSearch runs and
7 qgrep runs:

| Workload | Pattern | `is -q` | qgrep search to `/dev/null` | `is` vs qgrep |
| --- | --- | ---: | ---: | ---: |
| Quiet literal hit | `Nanite` | 3.67ms | 20.99ms | 5.7x |
| Quiet literal miss | `DefinitelyMissingIndexSearchNeedle` | 3.27ms | 11.78ms | 3.6x |
| Quiet word regex | `\bActor\b` | 4.03ms | 53.23ms | 13.2x |
| Quiet qualified regex | `[A-Za-z_][A-Za-z0-9_]*::[A-Za-z0-9_]+\(` | 3.28ms | 341.42ms | 104.1x |

To reproduce the search benchmark:

```bash
python3 scripts/benchmark-ue.py /path/to/UnrealEngine --prepare-qgrep
```

## Build From Source

Requirements:

- Rust stable toolchain with Cargo.
- A C toolchain that can link Rust binaries for your platform.

```bash
cargo build --release
cargo test --locked
./target/release/indexsearch --version
./target/release/indexsearch install-skills --help
```

On macOS/Linux, `./tests/smoke.sh` runs an end-to-end CLI smoke test.

GitHub Actions builds Linux x86_64, macOS arm64, macOS x86_64, and Windows
x86_64 binaries on every push to `main` and every pull request. Tagged versions
also create a GitHub Release with platform archives.

Tagged releases also contain optional package-manager publication jobs:

- `HOMEBREW_TAP_TOKEN` updates
  [`Abyss116/homebrew-indexsearch`](https://github.com/Abyss116/homebrew-indexsearch).
- `WINGET_TOKEN` submits the WinGet manifest PR to
  [`microsoft/winget-pkgs`](https://github.com/microsoft/winget-pkgs).

If either secret is absent, that publication job is skipped and the release
artifacts are still produced.

## License

IndexSearch is distributed under the terms of both the MIT license and the
Apache License 2.0. You may choose either license.

The references to ripgrep and qgrep in this repository are compatibility and
benchmark references only; their source code is not vendored into IndexSearch.

## Supported rg-like Flags

Normal search output follows `rg`'s auto decoration behavior. When stdout is a
terminal, each matching file is printed once, followed by `line:match` rows,
with a blank line between files. When stdout is captured or piped, output uses
`path:match`, or `path:line:match` with `-n`. Use `--heading`/`--no-heading` and
`-n`/`-N` to override. ANSI colors are enabled automatically for terminals and
can be controlled with `--color auto|always|never`.

- `-i`, `--ignore-case`
- `-s`, `--case-sensitive`
- `-S`, `--smart-case`
- `-F`, `--fixed-strings`
- `-w`, `--word-regexp`
- `-e PATTERN`, `--regexp PATTERN`
- `-g GLOB`
- `-n`, `--line-number`
- `-N`, `--no-line-number`
- `--column`
- `-A NUM`, `--after-context NUM`
- `-B NUM`, `--before-context NUM`
- `-C NUM`, `--context NUM`
- `-H`, `--with-filename`
- `-I`, `--no-filename`
- `--heading`
- `--no-heading`
- `-l`, `--files-with-matches`
- `-c`, `--count`
- `-o`, `--only-matching`
- `-q`, `--quiet`
- `--files`
- `--json`
- `--vimgrep`
- `--color auto|always|never`
- `-m NUM`, `--max-count NUM`
- `--max-filesize SIZE`
- `--hidden`
- `--follow`
- `--no-auto-index`
- `--auto-update`
- `--auto-update-untracked`
- `--stats`
- `--no-daemon`

Unsupported flags are rejected instead of silently changing semantics. Use `rg`
for PCRE-specific behavior or unsupported flags.

## Commands

```bash
is index [PATH]
is update [--git] [--git-untracked] [PATH]
is compact [PATH]
is watch [PATH]
is list-watches
is watch-log [PATH]
is unwatch <ID|PATH>
is install [--dir PATH]
is install-skills [OPTIONS]
is status [PATH]
is search [OPTIONS] PATTERN [PATH ...]
```

`is install` is only for self-copy installs from a direct-download archive or a
local source build. Homebrew and WinGet installs already expose `indexsearch`
and `is` through their normal command locations.

The explicit `search` subcommand is optional. `is PATTERN` searches directly,
like `rg PATTERN`. If the pattern is also an IndexSearch command name such as
`index`, `status`, or `watch`, use `is -- PATTERN [PATH ...]` or
`is search PATTERN [PATH ...]`.
