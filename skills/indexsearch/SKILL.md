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
```

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
- Use `is` in examples and command suggestions unless explaining installation.
- Mention `rg` fallback only when IndexSearch cannot satisfy the query.
