# IndexSearch

[![Build](https://github.com/Abyss116/IndexSearch/actions/workflows/build.yml/badge.svg?branch=main)](https://github.com/Abyss116/IndexSearch/actions/workflows/build.yml?query=branch%3Amain)
[![License: MIT OR Apache-2.0](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg)](#license)

IndexSearch is a Rust command line search tool for very large source trees. It
keeps a persistent binary index plus compressed text snapshots, so repeated
searches avoid walking the filesystem. The CLI intentionally follows common
`rg` output conventions closely enough to stand in for `rg` on large indexed
codebases.

The short command is `is`; the full command is `indexsearch`. Release archives
include both executables: `indexsearch` is the full indexer/backend, while `is`
is a small search frontend that talks to the per-project daemon and delegates
management commands back to `indexsearch`.

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

This self-copy install puts `indexsearch` and `is` into `~/.local/bin` on
macOS/Linux or `%USERPROFILE%\.local\bin` on Windows. If the archive contains
the lightweight `is` frontend, it is installed directly; otherwise `install`
falls back to an alias for `indexsearch`. Use `indexsearch install --dir PATH`
to override the install directory. Package manager installs already put
`indexsearch` and `is` on PATH and do not need this step. On Windows the alias
is a native `is.exe`, not an `is.cmd` wrapper, so PowerShell metacharacters
inside quoted patterns are not re-parsed by `cmd.exe`.

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

## How It Works

The current index is file-oriented, not a full suffix array and not a
chunk-posting index. Its hot path is:

- Walk the configured project once, honoring `index-search-project.txt`, hidden
  path rules, file globs, and the max file size.
- Read searchable text files in parallel, skip binary-looking files, and store
  each file's relative path, size, mtime, and LZ4-compressed content snapshot.
- Build a case-folded trigram posting table from each file. Search intersects
  the rarest required trigrams to get candidate file ids before decompressing.
- Add a few general source-code postings on top of trigrams: identifier prefix
  keys, selected 6-byte word-fragment keys, and qualified-call keys for patterns
  such as `Type::Method(`. These are generic identifier indexes, not UE-specific
  hard-coded symbols.
- For each candidate, decompress only the stored snapshot and verify with fast
  literal scanners, Aho-Corasick literal sets, specialized source-pattern
  matchers, or Rust's regex engine, depending on the query.
- Keep updates as base index plus delta segments. Git-aware update and watcher
  updates can write tiny deltas for changed paths; `compact` atomically folds
  those deltas into a new base index.
- Use a per-project search daemon for hot searches. The daemon keeps the mmap
  index open; the `is` frontend only resolves the project, starts/connects to
  the daemon, sends the original rg-like search arguments, and writes the daemon
  response.

There is experimental chunk/bloom scaffolding in the codebase, but the release
index described here does not rely on chunk-level postings yet.

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
requests over localhost. The `is` executable is intentionally much smaller than
the full backend and does only enough client-side work to locate the index,
validate or start the daemon, pass through arguments, and copy the response.
Use `indexsearch` directly when you need the full management surface or want to
compare the old full-client path.

Use either form to bypass the daemon:

```bash
is --no-daemon -F "SomeSymbol" .
INDEXSEARCH_NO_DAEMON=1 is -F "SomeSymbol" .
```

Daemon records live in `.indexsearch/search-daemon.txt`. If `indexsearch
install` replaces the backend executable, or if the base index is
rebuilt/compacted, the next search detects the fingerprint mismatch and starts a
fresh daemon. If no index exists above the current directory, interactive `is`
asks whether to create one in the current directory; non-interactive use prints
the explicit `indexsearch index .` / `is watch .` hint instead.

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
- IndexSearch indexed files: 196,961.
- qgrep indexed files: 196,900 using near-identical UE-oriented include/exclude
  rules translated from `index-search-project.txt`.
- Search timings are median wall-clock time: 7 runs for IndexSearch/qgrep and 2
  runs for `rg`.
- Match counts differ slightly where the tools' glob and output semantics are
  not perfectly identical; the `*.cpp` constrained row matches exactly.

### Index And Update

| Operation | IndexSearch | qgrep | Notes |
| --- | ---: | ---: | --- |
| Fresh index | 13.53s | 21.50s | IndexSearch timing: scan 3.64s, process 6.67s, write 3.21s |
| No-change update | 0.27s | 4.19s | Git changed-path check, no file scan work |
| Compact 2 deltas | 8.61s | n/a | Folded 196,961 visible files into a new base index |

### Search

| Workload | Pattern | Matches `is/qgrep/rg` | `is` | qgrep | `rg` | `is` vs qgrep |
| --- | --- | ---: | ---: | ---: | ---: | ---: |
| Literal: common token | `Nanite` | 14664 / 14672 / 13013 | 7.02ms | 21.61ms | 3060.36ms | 3.1x |
| Literal: long symbol | `SkeletalMeshComponent` | 7606 / 7593 / 7605 | 7.34ms | 19.61ms | 3027.68ms | 2.7x |
| Literal: missing | `DefinitelyMissingIndexSearchNeedle` | 0 / 0 / 0 | 2.63ms | 12.72ms | 3005.92ms | 4.8x |
| Case-insensitive literal | `skeletalmeshcomponent` | 7616 / 7603 / 7615 | 7.62ms | 19.76ms | 2935.81ms | 2.6x |
| Word regex | `\bActor\b` | 23675 / 23677 / 23665 | 22.37ms | 52.76ms | 2981.87ms | 2.4x |
| Regex: alternation | `(Nanite\|Lumen\|SkeletalMeshComponent)` | 34500 / 34498 / 31439 | 38.39ms | 118.74ms | 2979.52ms | 3.1x |
| Regex: prefix/suffix | `Skeletal[A-Za-z0-9_]*Component` | 7930 / 7917 / 7929 | 13.69ms | 23.93ms | 2969.24ms | 1.7x |
| Regex: qualified call | `[A-Za-z_][A-Za-z0-9_]*::[A-Za-z0-9_]+\(` | 1487547 / 1487316 / 1481806 | 239.01ms | 345.83ms | 3056.53ms | 1.4x |
| Glob: `*.cpp` literal | `Nanite` in `*.cpp` | 10061 / 10061 / 10061 | 5.41ms | 21.78ms | 1237.28ms | 4.0x |

For `-q` existence checks, IndexSearch stops as soon as a verified match is
found. Quiet timings are median wall-clock time across 31 IndexSearch runs and
7 qgrep runs:

| Workload | Pattern | `is -q` | qgrep search to `/dev/null` | `is` vs qgrep |
| --- | --- | ---: | ---: | ---: |
| Quiet literal hit | `Nanite` | 4.61ms | 20.60ms | 4.5x |
| Quiet literal miss | `DefinitelyMissingIndexSearchNeedle` | 2.39ms | 12.14ms | 5.1x |
| Quiet word regex | `\bActor\b` | 2.73ms | 55.19ms | 20.2x |
| Quiet qualified regex | `[A-Za-z_][A-Za-z0-9_]*::[A-Za-z0-9_]+\(` | 2.78ms | 347.91ms | 125.1x |

The lightweight `is` frontend reduces fixed process/client overhead compared
with invoking the full `indexsearch` binary for daemon-backed search:

| Workload | Full `indexsearch` client | Lightweight `is` frontend | Speedup |
| --- | ---: | ---: | ---: |
| `-q -F Nanite` | 6.17ms | 4.61ms | 1.3x |
| `-q -F DefinitelyMissingIndexSearchNeedle` | 3.53ms | 2.20ms | 1.6x |
| `-q -w Actor` | 3.86ms | 2.65ms | 1.5x |

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
./target/release/is --version
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
Apache License 2.0. You may choose either license; see `LICENSE-MIT` and
`LICENSE-APACHE`.

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
