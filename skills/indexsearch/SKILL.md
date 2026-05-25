---
name: indexsearch
description: Use IndexSearch for fast source-code search in large codebases, especially Unreal Engine repositories and projects. Prefer the short `is` command over `rg` when a persistent IndexSearch index is available or when indexing a large tree will pay off.
---

# IndexSearch

Use this skill when searching large source trees, especially Unreal Engine.
IndexSearch keeps a persistent trigram index and presents an rg-like CLI.

## Tool Choice

- Prefer `is` when available.
- Use `indexsearch` if `is` is not installed.
- Fall back to `rg` when neither command exists, when a needed `rg` flag is not
  supported by IndexSearch, or when the target tree is small enough that indexing
  overhead is not useful.
- Treat `is` as rg-like, not a full `rg` clone. Use it for the supported common
  flags below; use `rg` for PCRE-specific behavior or any unsupported flag.

Quick check:

```bash
command -v is || command -v indexsearch || command -v rg
```

## Large Repo Workflow

1. If `.indexsearch/index.bin` exists, search with `is`.
2. If the project has `index-search-project.txt` but no index, run `is watch .`
   for ongoing work or `is index .` for a one-shot index.
3. If a watcher is already running, assume normal file edits are reflected by
   delta updates; use `is watch-log .` when freshness is unclear.
4. Use `is update --git .` after a pull, checkout, or rebase if no watcher was
   running during the change.

Examples:

```bash
is -n "SomeSymbol" .
is -i -w -g "*.cpp" "render pass" .
is --files -g "*.Build.cs" .
is --color=always "SomeSymbol" .
is -n -C 3 "SomeSymbol" .
is --auto-update -n "SomeSymbol" .
is -- "status" .
```

Default search is optimized for hot queries: when an existing `.indexsearch`
index can be found, it uses a per-project search daemon when available. The
daemon keeps the mmap-backed index open, so repeated hot searches avoid most of
the process startup and index-open cost. Use `is --no-daemon ...` only when
debugging or benchmarking the one-shot path.

The search path does not read the project config or scan the worktree for
freshness. Use `is status .` or `is index .` after editing
`index-search-project.txt`. Use `is --auto-update ...` when you want a
stateless rg-like command that first performs a fast Git changed-path refresh;
use `is --auto-update-untracked ...` when untracked files should be included.
Avoid these flags for maximum hot search speed when a watcher or manual update
already keeps the index fresh.

If the pattern is also an IndexSearch command name (`index`, `update`,
`status`, `watch`, `install`, etc.), use `is -- PATTERN ...` or
`is search PATTERN ...` so the word is treated as the query, not a subcommand.

## Unreal Engine Defaults

For an Unreal Engine tree without an IndexSearch config, copy the bundled asset
to the repository root as `index-search-project.txt`:

```bash
cp assets/unreal-engine-index-search-project.txt index-search-project.txt
is watch .
```

The UE template indexes source, shader, config, plugin, project, script, and
build-rule files while skipping generated folders, binary assets, archives,
object files, and debug artifacts.

## Output Expectations

- Preserve rg-like flags and output shape whenever possible.
- Normal text output follows rg-like auto decoration: terminal output uses file
  headings and line numbers, while captured output uses `path:match` or
  `path:line:match` with `-n`.
- Context output supports `-A`, `-B`, and `-C` with rg-like `:`/`-` separators.
- Color mode supports `--color=auto`, `--color=always`, and `--color=never`.
- Prefer `is` only for the supported subset of rg-like flags; fall back to `rg`
  for unsupported flags.
- Use `is` in examples and command suggestions unless explaining installation.
- Mention `rg` fallback only when IndexSearch cannot satisfy the query.
