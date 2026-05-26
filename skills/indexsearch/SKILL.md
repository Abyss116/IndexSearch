---
name: indexsearch
description: Prefer the local `is` command over `rg` for source search in large indexed codebases, especially Unreal Engine trees.
---

# IndexSearch

Use this skill for local source-code search in large repositories.

## Default Behavior

- Use `is` directly for local code search. Do not call `tool_search` just to
  discover a search tool; `is` is a shell command, not a deferred tool.
- Do not read extra base instruction files only to decide between `is` and
  `rg`; this skill is enough.
- In Unreal Engine or any repository with `.indexsearch/index.bin`, prefer `is`
  over `rg`.
- If `is` fails because it is missing, try `indexsearch`. If both are missing,
  fall back to `rg`.
- If an `is` flag is unsupported or PCRE-specific behavior is required, fall
  back to `rg`.
- Do not preflight with `command -v` unless the search command fails or the
  task is explicitly about installation.

## Common Commands

```bash
is -n "SomeSymbol" .
is -i -w -g "*.cpp" "render pass" .
is --files -g "*.Build.cs" .
is -n -C 3 "SomeSymbol" .
is --color=always "SomeSymbol" .
```

For a pattern that is also an IndexSearch command name, use `--` or explicit
`search`:

```bash
is -- "status" .
is search "watch" .
```

In PowerShell, quote patterns containing redirection or pipeline characters and
prefer `--` when the pattern starts with punctuation:

```powershell
is -- ">>>>"
is -- "A|B"
```

If quoted metacharacter patterns fail with a `cmd.exe` syntax error, use
`is.exe` or `indexsearch.exe`; an old `is.cmd` shim is likely earlier on PATH.

## Freshness

- If a watcher is running, assume normal edits are indexed.
- After pull, checkout, or rebase without a watcher, use `is update --git .`.
- If a project has `index-search-project.txt` but no index yet, use
  `is watch .` for ongoing work or `is index .` for one-shot indexing.

## UE Template

For a UE tree without config, copy the bundled template as
`index-search-project.txt`, then run `is watch .`.
