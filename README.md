# IndexSearch

[![Build](https://github.com/Abyss116/IndexSearch/actions/workflows/release.yml/badge.svg?branch=main)](https://github.com/Abyss116/IndexSearch/actions/workflows/release.yml?query=branch%3Amain)
[![License: MIT OR Apache-2.0](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg)](#license)

IndexSearch is a Rust command line search tool for very large source trees. It
keeps a persistent binary index plus compressed text snapshots, so repeated
searches avoid walking the filesystem. The CLI intentionally follows common
`rg` output conventions closely enough to stand in for `rg` on large indexed
codebases.

The short command is `is`; the full command is `indexsearch`. Both user-facing
commands are lightweight frontends. They talk to `is-daemon`, the per-project
backend service that owns indexing, filesystem watching, idle compaction, and
daemon-backed search.

## Install

Prebuilt binaries are attached to the
[latest GitHub Release](https://github.com/Abyss116/IndexSearch/releases/latest).

Install or update to the latest release with one command:

```bash
# macOS / Linux
curl -fsSL https://raw.githubusercontent.com/Abyss116/IndexSearch/main/install.sh | sh
```

```powershell
# Windows PowerShell
irm https://raw.githubusercontent.com/Abyss116/IndexSearch/main/install.ps1 | iex
```

These scripts download the latest GitHub Release, run `indexsearch install`,
and install `indexsearch`, `is`, and `is-daemon` into the target bin directory.
Re-running the command updates the local install.

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
[GitHub Actions build workflow](https://github.com/Abyss116/IndexSearch/actions/workflows/release.yml).

After extracting a direct-download archive, you can copy the extracted binaries
into a user-writable bin directory:

```bash
./indexsearch install
```

This self-copy install copies the full `is-daemon` backend and creates
lightweight `indexsearch` and `is` frontends in `~/.local/bin` on macOS/Linux or
`%USERPROFILE%\.local\bin` on Windows. Use `indexsearch install --dir PATH` to
override the install directory. Package manager installs already put the three
binaries on PATH and do not need this step. Release archives include
`indexsearch` and `is-daemon`; `install` creates the shorter `is` frontend in
the target bin directory. On Windows `is` is a native `is.exe`, not an `is.cmd`
wrapper, so PowerShell metacharacters inside quoted patterns are not re-parsed
by `cmd.exe`.

PowerShell still parses unquoted redirection characters before IndexSearch can
see them. Quote those patterns or put `--` before the pattern:

```powershell
is -- ">>>>"
is -F ">>>>"
```

If quoted patterns such as `is ">>>>"` still fail with a `cmd.exe` message like
`>> was unexpected`, an old `is.cmd` shim is being found first on PATH. Check
with `Get-Command is -All`, remove the stale `.cmd`, or call `is.exe` /
`indexsearch.exe` directly.

If Windows reports `Access is denied` during `install`, an older
`is-daemon.exe` is probably still running or temporarily locked by the OS. Stop
that process and run `indexsearch.exe install` again:

```powershell
Get-Process is-daemon -ErrorAction SilentlyContinue | Stop-Process -Force
.\indexsearch.exe install
```

Newer releases also write a versioned backend such as
`is-daemon-0.3.x.exe`, so a locked old backend no longer prevents installing
the new frontend.

## Quick Start

```bash
cd /path/to/large/repo
is index .
is -n "SomeSymbol" .
is -i -w -g "*.cpp" "render pass" .
is --files .
```

If `index-search-project.txt` does not exist, `index`, `update`, `watch`, and
interactive first search can create a default config before building the index.
Edit that file and rerun `is index .` or `is update .` to rebuild with new
rules.

## Profiling

Use `--profile` or `--instrument` to print internal timing lines to stderr.
This is intended for diagnosing platform-specific costs such as Windows
filesystem scanning, file reads, compression, index writes, daemon RPC, and
output copying.

PowerShell examples:

```powershell
is index --profile . 2>&1 | Tee-Object index-profile.txt
is update --profile . 2>&1 | Tee-Object update-profile.txt
is --profile -n -g "*.cpp" Nanite . 2>&1 | Tee-Object search-profile.txt
is --profile --no-daemon -n -g "*.cpp" Nanite . 2>&1 | Tee-Object search-no-daemon-profile.txt
```

Useful search comparisons are `--profile` with the daemon, `--profile
--no-daemon` without it, and `--profile --auto-update` when checking refresh
cost. The `profile:` lines are stable text output and can be pasted into an
issue or chat for analysis. For parallel indexing, per-file phases such as
`index_file_read`, `index_tokenize`, and `index_compress` are accumulated
worker time, while `index_process_total` is wall time.

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
  index open; the lightweight frontend only resolves the project,
  starts/connects to the daemon, sends the original rg-like search arguments,
  and passes stdout to the daemon so it can write search output directly to the
  caller. Unix/macOS uses descriptor passing over a Unix socket; Windows uses
  `DuplicateHandle` with the daemon process. Control messages and stderr still
  use the daemon connection.
- Default search output is optimized for speed and follows the index candidate
  order. Use `--sort path` when deterministic path ordering is more important
  than the lowest latency.

There is experimental chunk/bloom scaffolding in the codebase, but the release
index described here does not rely on chunk-level postings yet.

## Search Freshness

`is index .` rebuilds the base index from scratch.

`is update .` refreshes an existing index. If a project daemon/watch is running,
`update` asks it to flush pending filesystem events and reports whether the
watcher was already current; this avoids a full tree scan in the normal watched
case. Without a usable daemon, `update` falls back to comparing stored path,
`mtime`, and size metadata, reusing unchanged snapshots, reading only changed or
new files, and dropping deleted or newly ignored files. If an index was just
built or refreshed, immediate follow-up `update` or `watch` commands reuse that
recent sync state instead of scanning the tree again. Use `is update
--force-scan .` to bypass these shortcuts and force the filesystem
reconciliation path.

For Git worktrees:

```bash
is update --git .
```

`update --git` records the last indexed `HEAD` and can catch clean committed
changes from `git pull`, `checkout`, and `rebase`; it also includes current
local changes and untracked files by default. It writes small delta segments
under `.indexsearch/deltas/` when possible. Use `is compact .` to fold deltas
back into the base index.

For active large repositories:

```bash
is watch .
is list-watches
is watch-log .
is unwatch .
```

`watch`, `list-watches`, `watch-log`, and `unwatch` remain the user-facing
names because the main reason to manage the service manually is filesystem
watching. Internally, a watched project is now served by one `is-daemon`
process: it owns search RPC, filesystem watching, startup sync, and idle
compaction.

The service first synchronizes the index with the current filesystem state,
then writes batched delta updates on file events and can compact during idle
periods. If no base index exists, `is watch .` builds it first; otherwise it
performs a filesystem-based incremental update so edits made while the service
was stopped are included. Overlapping watches are normalized so a parent watch
covers child directories. `watch-log` records real index/update/compact
activity and omits no-op filesystem events.

Ordinary searches also try to make this seamless. When `is PATTERN` runs inside
an existing project, the lightweight frontend checks whether an active parent
watch already covers that root. If not, it silently starts `is watch` first,
which creates or refreshes the index before the search daemon serves the query.

Useful watcher knobs:

```bash
is watch . --idle-seconds 5 --compact-delta-count 16 --compact-delta-bytes 256mb
```

## Search Daemon

Hot searches automatically try the per-project daemon service. The frontend
first resolves the project root from the current path, ancestor `.indexsearch`
directories, or `index-search-project.txt`, ensures the matching service is
running, and then connects to it. The daemon keeps the mmap-backed index open,
serves requests over localhost or a Unix socket, and watches the project for
incremental updates. `indexsearch` and `is` are intentionally much smaller than
the full backend and do only enough client-side work to locate the project,
validate or start `is-daemon`, pass through arguments, and stream response
frames to stdout/stderr.

Use either form to bypass the daemon:

```bash
is --no-daemon -F "SomeSymbol" .
INDEXSEARCH_NO_DAEMON=1 is -F "SomeSymbol" .
```

With the lightweight frontend, bypassing the daemon still launches the full
`is-daemon` backend once and runs the search there. This mode is mainly for
profiling and diagnostics; normal searches should use the per-project daemon so
the index stays mmaped and watched.

Daemon records live in `.indexsearch/search-daemon.txt`. If `indexsearch
install` replaces `is-daemon`, or if the base index is
rebuilt/compacted, the next search detects the fingerprint mismatch and starts a
fresh daemon. If no project exists above the current directory, interactive
`is` asks whether to create one in the current directory, defaulting to yes;
non-interactive use prints the explicit `indexsearch index .` / `is watch .`
hint instead.

## Unreal Engine

This repository includes a UE-oriented template:

```bash
cp templates/unreal-engine/index-search-project.txt /path/to/UnrealEngine/index-search-project.txt
cd /path/to/UnrealEngine
is watch .
```

The template keeps source, shader, config, plugin, project, script, build-rule,
and `*.log` files searchable while skipping generated folders, binary assets,
archives, object files, and debug artifacts. If the first interactive `is`
inside a tree discovers an Unreal Engine root or a `.uproject` root, the
auto-created `index-search-project.txt` uses this UE template instead of the
minimal generic config. The same template is bundled inside the agent skill at
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
- Search timings are median wall-clock time: 7 runs for IndexSearch/qgrep and 3
  runs for `rg`.
- Match counts differ slightly where the tools' glob and output semantics are
  not perfectly identical; the `*.cpp` constrained row matches exactly.

### Index And Update

| Operation | IndexSearch | qgrep | Notes |
| --- | ---: | ---: | --- |
| Fresh index | 10.90s | 21.50s | IndexSearch timing: scan 3.98s, process 5.99s, write 0.94s |
| No-change update | 0.27s | 4.19s | Git changed-path check, no file scan work |
| Compact delta | 2.59s | n/a | Segment-merged 196,961 visible files into a new base index |

### Search

| Workload | Pattern | Matches `is/qgrep/rg` | `is` | qgrep | `rg` | `is` vs qgrep |
| --- | --- | ---: | ---: | ---: | ---: | ---: |
| Literal: common token | `Nanite` | 14664 / 14672 / 13013 | 5.54ms | 20.69ms | 3075.97ms | 3.7x |
| Literal: long symbol | `SkeletalMeshComponent` | 7606 / 7593 / 7605 | 5.17ms | 17.57ms | 2993.50ms | 3.4x |
| Literal: missing | `DefinitelyMissingIndexSearchNeedle` | 0 / 0 / 0 | 2.59ms | 12.16ms | 3073.61ms | 4.7x |
| Case-insensitive literal | `skeletalmeshcomponent` | 7616 / 7603 / 7615 | 5.64ms | 18.36ms | 2987.83ms | 3.3x |
| Word regex | `\bActor\b` | 23675 / 23677 / 23665 | 10.34ms | 52.20ms | 3032.03ms | 5.0x |
| Regex: alternation | `(Nanite\|Lumen\|SkeletalMeshComponent)` | 34500 / 34498 / 31439 | 24.34ms | 120.64ms | 3058.48ms | 5.0x |
| Regex: prefix/suffix | `Skeletal[A-Za-z0-9_]*Component` | 7930 / 7917 / 7929 | 10.13ms | 20.19ms | 3026.95ms | 2.0x |
| Regex: qualified call | `[A-Za-z_][A-Za-z0-9_]*::[A-Za-z0-9_]+\(` | 1487547 / 1487316 / 1481806 | 82.48ms | 349.38ms | 3156.30ms | 4.2x |
| Glob: `*.cpp` literal | `Nanite` in `*.cpp` | 10061 / 10061 / 10061 | 4.84ms | 20.58ms | 1294.54ms | 4.3x |

For `-q` existence checks, IndexSearch stops as soon as a verified match is
found. Quiet timings are median wall-clock time across 31 IndexSearch runs and
7 qgrep runs:

| Workload | Pattern | `is -q` | qgrep search to `/dev/null` | `is` vs qgrep |
| --- | --- | ---: | ---: | ---: |
| Quiet literal hit | `Nanite` | 2.35ms | 20.14ms | 8.6x |
| Quiet literal miss | `DefinitelyMissingIndexSearchNeedle` | 2.36ms | 11.91ms | 5.0x |
| Quiet word regex | `\bActor\b` | 2.52ms | 53.44ms | 21.2x |
| Quiet qualified regex | `[A-Za-z_][A-Za-z0-9_]*::[A-Za-z0-9_]+\(` | 2.45ms | 345.91ms | 141.1x |

Both `indexsearch` and `is` are lightweight frontends; the installed full
backend is `is-daemon`. Large search stdout is written directly from the daemon
into the frontend's stdout on Unix/macOS and Windows, avoiding an extra RPC
copy; stderr and control messages remain framed. On Windows this direct stdout
path is enabled by default; set `INDEXSEARCH_WINDOWS_DIRECT_STDOUT=0` only when
diagnosing handle-passing problems.

To reproduce the search benchmark:

```bash
python3 scripts/benchmark-ue.py /path/to/UnrealEngine --prepare-qgrep \
  --search-repeats 7 --rg-repeats 3
```

For changes that may affect search performance, compare against one or more
historical revisions with the same checkout and index:

```bash
python3 scripts/benchmark-history.py /path/to/UnrealEngine \
  --refs b42de13 HEAD --case qualified-call
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
./target/release/is-daemon --version
./target/release/indexsearch install-skills --help
```

On macOS/Linux, `./tests/smoke.sh` runs an end-to-end CLI smoke test.

GitHub Actions builds Linux x86_64, macOS arm64, macOS x86_64, and Windows
x86_64 binaries on every push to `main` and every pull request. Tagged versions
also create a GitHub Release with platform archives.

Tagged releases also contain optional package-manager publication jobs:

- `HOMEBREW_TAP_TOKEN` updates
  [`Abyss116/homebrew-indexsearch`](https://github.com/Abyss116/homebrew-indexsearch).
- `RELEASE_TOKEN` is optional; if set, release publication uses it instead of
  the workflow `GITHUB_TOKEN`.
- `WINGET_TOKEN` submits WinGet version updates to
  [`microsoft/winget-pkgs`](https://github.com/microsoft/winget-pkgs) after
  `Abyss116.IndexSearch` has been accepted. Initial package submission is kept
  manual to avoid duplicate "new package" PRs while moderation is pending.

If either secret is absent, that publication job is skipped and the release
artifacts are still produced.

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
- `--force-scan`
- `--stats`
- `--profile`, `--instrument`
- `--no-daemon`

Unsupported flags are rejected instead of silently changing semantics. Use `rg`
for PCRE-specific behavior or unsupported flags.

## Commands

```bash
is index [PATH]
is update [--git] [--force-scan] [PATH]
is compact [PATH]
is clean [--yes] [PATH]
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

`is clean [PATH]` stops any matching project service found on the parent path
chain and removes `.indexsearch/`, including deltas and logs. It keeps
`index-search-project.txt`. Pass `--yes` for non-interactive cleanup.

Interactive indexing, update, compaction, and first-search service startup show
a live progress line on stderr. Set `INDEXSEARCH_NO_PROGRESS=1` to disable it.

## License

IndexSearch is distributed under the terms of both the MIT license and the
Apache License 2.0. You may choose either license; see `LICENSE-MIT` and
`LICENSE-APACHE`.

The references to ripgrep and qgrep in this repository are compatibility and
benchmark references only; their source code is not vendored into IndexSearch.
