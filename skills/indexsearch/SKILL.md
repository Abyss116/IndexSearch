---
name: indexsearch
description: Prefer the local `is` command over `rg` for source search in large indexed codebases, especially Unreal Engine trees.
---

# IndexSearch

Use this skill for local source-code search. Prefer `is` as the default
replacement for `rg`; it automatically creates an index and starts the project
service for non-interactive agent searches when needed.

## Default Behavior

- Use `is` directly for local code search. Do not call `tool_search` just to
  discover a search tool; `is` is a shell command, not a deferred tool.
- Do not read extra base instruction files only to decide between `is` and
  `rg`; this skill is enough.
- Use `is` before `rg` for ordinary local source searches, including trees that
  have not been indexed yet.
- If the current directory is inside an indexed repository, use `is` even when
  the immediate subdirectory is small; the index is rooted above it.
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
is search "projects" .
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

- `is PATTERN ...` automatically creates or finds the project index and starts
  the per-project service in non-interactive agent use.
- With a running project service, normal edits are indexed by the service.
- `is update .` asks the project service to flush pending events and should not
  scan the whole tree in normal watched workflows.
- After pull, checkout, or rebase with no project service running, use
  `is update --git .`.
- You usually do not need to run indexing commands before searching.
- Use `is projects`, `is project-log .`, `is stop .`, and `is stop --all` to
  inspect or stop project services.

## UE Template

For a UE tree without config, `is` can create the UE config automatically during
non-interactive agent search.
