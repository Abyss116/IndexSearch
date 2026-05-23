# IndexSearch

IndexSearch is a Rust command line search tool for large source trees. It uses a
prebuilt binary trigram index plus stored text snapshots, so searches can avoid
walking the filesystem. Its project file is `index-search-project.txt`:

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

The search command intentionally follows common `rg` output conventions so it
can stand in for `rg` in workflows where a pre-indexed tree is faster.

## Quick Start

Prebuilt binaries are attached to the
[latest GitHub Release](https://github.com/Abyss116/IndexSearch/releases/latest).
Continuous builds are also available from the
[latest GitHub Actions run on `main`](https://github.com/Abyss116/IndexSearch/actions/workflows/build.yml?query=branch%3Amain).

Direct release downloads:

- [Linux x86_64](https://github.com/Abyss116/IndexSearch/releases/latest/download/indexsearch-linux-x86_64.tar.gz)
- [macOS arm64](https://github.com/Abyss116/IndexSearch/releases/latest/download/indexsearch-macos-aarch64.tar.gz)
- [macOS x86_64](https://github.com/Abyss116/IndexSearch/releases/latest/download/indexsearch-macos-x86_64.tar.gz)
- [Windows x86_64](https://github.com/Abyss116/IndexSearch/releases/latest/download/indexsearch-windows-x86_64.zip)

After extracting the archive, install the binary and short `is` alias:

```bash
./indexsearch install
```

Each release archive also includes the Unreal Engine template, agent skill, and
cross-platform agent-instruction installer.

Or build from source:

```bash
cargo build --release
./target/release/indexsearch install
```

Then use it from any shell:

```bash
is index .
is update .
is -n "SomeSymbol" .
is -i -w -g "*.cpp" "render pass" .
```

The index is written to `.indexsearch/index.bin`.
If `index-search-project.txt` does not exist, `index`, `update`, `watch`, and
search-time auto-indexing create a default one before building the index. Edit
that file and rerun `is index .` or `is update .` to rebuild with new rules.

`index` always rebuilds from scratch. `update` scans the current tree, compares
each indexed path against the previous index using stored `mtime` and `size`,
reuses unchanged file snapshots from the old index, reads changed or new files,
and drops deleted or newly ignored files from the next index. The compact index
file is then replaced in one step after the old mmap is released.

For Git worktrees, `update --git` avoids the full tree scan by asking Git for
changed tracked paths and writing only a small delta segment:

```bash
is update --git .
```

Use `--git-untracked` when new untracked files should be included too. Delta
segments live under `.indexsearch/deltas/`; searches read the base index plus
all deltas, with newer deltas overriding older file contents and tombstones
hiding deleted files. If the tree is not a Git repository, or the config
changed, the command falls back to the regular full update/rebuild path.

Successful index writes store the current Git `HEAD` in `.indexsearch/state.txt`.
That lets `update --git` catch clean committed changes from `git pull`,
`checkout`, or `rebase` by diffing the last indexed commit against the current
commit, then layering any working-tree changes on top.

Use `compact` when you want to fold accumulated deltas back into the base index:

```bash
is compact .
```

Compaction is explicit for now, so update latency stays predictable. A watcher
service can use the same knobs: write deltas on file events, then compact during
idle periods or after delta count/size thresholds.

## Watcher Direction

A persistent watcher should be implemented as a separate foreground/service
command using the same delta writer as `update --git`:

```bash
is watch .
is list-watches
is watch-log .
is unwatch .
```

Each watched root runs as its own background process and is tracked under
`~/.indexsearch/watches/`, so multiple projects can be watched at once.
Overlapping watches are normalized: if a parent directory is already watched,
watching a child directory is treated as already covered; if a new parent watch
is started, existing child watches are stopped.
Per-project watcher activity is appended to `.indexsearch/watch.log`; use
`is watch-log .` to inspect initial indexing, automatic delta updates,
and idle compaction timings.

The policy is:

- collect filesystem events for the configured root and debounce them briefly;
- write one delta segment for each settled batch;
- coordinate search/update/compact with `.indexsearch/index.lock`, where search
  takes a shared lock and writers take an exclusive lock;
- compact only during idle windows or when configured delta count/size thresholds
  are exceeded.

This keeps updates automatic without making every edit pay the cost of rewriting
the base index.

Useful watcher knobs:

```bash
is watch . --idle-seconds 5 --compact-delta-count 16 --compact-delta-bytes 256mb
```

With a watcher running, Git is no longer required for ordinary file edits inside
the watched root. Git update remains useful for catching changes that happened
while the watcher was not running, such as a pull before starting the service.
If the root has never been indexed, `indexsearch watch .` builds the first base
index before starting the background watcher.

Watcher compaction defaults are `--idle-seconds 5`,
`--compact-delta-count 16`, and `--compact-delta-bytes 256mb`. Compaction is
checked only after the watcher has been idle for the configured idle interval,
and it runs when either the delta count or total delta bytes reaches the
configured threshold.

## Install

Install the current executable into the user bin directory and create the short
`is` alias:

```bash
./indexsearch install
```

By default this installs to `~/.local/bin` on macOS/Linux and
`%USERPROFILE%\.local\bin` on Windows. On Unix, `is` is a symlink to
`indexsearch`; on Windows it is a small `is.cmd` shim. Use `--dir PATH` to
override.

## Unreal Engine Template

Use the bundled UE-oriented project template when indexing Unreal Engine source
trees or UE game projects:

```bash
cp templates/unreal-engine/index-search-project.txt /path/to/UnrealEngine/index-search-project.txt
cd /path/to/UnrealEngine
is watch .
```

The template keeps source, shader, config, plugin, project, script, and build
rule files searchable while skipping common UE generated folders, binary assets,
archives, object files, and debug artifacts.

The same template is also bundled inside the agent skill at
`skills/indexsearch/assets/unreal-engine-index-search-project.txt`.

## Agent Skill

This repository includes an IndexSearch agent skill package:

- `skills/indexsearch/SKILL.md` for Codex and Claude Code style skill loaders.
- `agent-rules/AGENTS.md` for tools that read `AGENTS.md`, including OpenCode
  and Cursor.
- `agent-rules/CLAUDE.md` for Claude Code project instructions.
- `agent-rules/cursor/indexsearch.mdc` for Cursor Project Rules.

Install the skill or rules with the cross-platform helper:

```bash
python3 scripts/install-agent-instructions.py --target codex --scope user
python3 scripts/install-agent-instructions.py --target claude --scope user
python3 scripts/install-agent-instructions.py --target opencode --scope user
python3 scripts/install-agent-instructions.py --target all --scope project --project /path/to/project --ue-template
```

For project installs, the helper can also copy the UE template as
`index-search-project.txt` with `--ue-template`. Existing generated instruction
blocks are replaced in place; unrelated content is preserved.

## Build From Source

Requirements:

- Rust stable toolchain with Cargo.
- A C toolchain that can link Rust binaries for your platform.

Build, test, and run locally:

```bash
cargo build --release
cargo test --locked
./target/release/indexsearch --version
```

On macOS/Linux, `./tests/smoke.sh` runs an end-to-end CLI smoke test.

The release binary is written to `target/release/indexsearch` on macOS/Linux and
`target\release\indexsearch.exe` on Windows.

To install from a source checkout:

```bash
cargo run --release -- install
```

## Release Builds

GitHub Actions builds downloadable binaries for Linux x86_64, macOS arm64,
macOS x86_64, and Windows x86_64 on every push to `main` and every pull request.
Those builds are available as
[workflow artifacts](https://github.com/Abyss116/IndexSearch/actions/workflows/build.yml).

Tagged versions create a GitHub Release and upload the platform archives:

```bash
git tag v0.2.2
git push origin v0.2.2
```

## Design Notes

- `index-search-project.txt` is the primary project file name.
- The index stores searchable file contents plus a trigram inverted table in a
  compact binary layout. Query-time index loading uses `mmap`, so paths,
  postings, and file contents are read as views instead of copied into new
  strings.
- Query execution extracts literal fragments from simple regexes, intersects
  trigram postings first, then verifies matches against the stored text. Literal
  searches use `memchr`/`aho-corasick`; regex searches use Rust's `regex`
  crate, which is built on the same `regex-automata` family used by ripgrep.
- Results are collected in file order, with candidate-file verification spread
  across hardware threads via `rayon`.

## Supported rg-like Flags

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
- `-H`, `--with-filename`
- `-I`, `--no-filename`
- `-l`, `--files-with-matches`
- `-c`, `--count`
- `-o`, `--only-matching`
- `-q`, `--quiet`
- `--files`
- `--json`
- `--vimgrep`
- `-m NUM`, `--max-count NUM`
- `--max-filesize SIZE`
- `--hidden`
- `--follow`
- `--stats`

Unsupported flags are rejected instead of silently changing semantics.

## Commands

```bash
./indexsearch index [PATH]
./indexsearch update [PATH]
./indexsearch compact [PATH]
./indexsearch watch [PATH]
./indexsearch list-watches
./indexsearch watch-log [PATH]
./indexsearch unwatch <ID|PATH>
./indexsearch install [--dir PATH]
./indexsearch status [PATH]
./indexsearch search [OPTIONS] PATTERN [PATH ...]
```

The explicit `search` subcommand is optional. `./indexsearch PATTERN` searches
directly, like `rg PATTERN`.
