---
name: indexsearch
description: Use the local `is` command as the default source-search tool instead of `rg`, especially in large indexed codebases and Unreal Engine trees.
hooks:
  PreToolUse:
    - matcher: "Bash"
      hooks:
        - type: command
          command: "python3"
          args:
            - "./scripts/prefer-isgrep-hook.py"
          timeout: 5
---

# IndexSearch

Use this skill for local source-code search. Treat `is` as the default
replacement for `rg`: it accepts common rg-style flags, ignores unsupported rg
flags when safe, automatically creates an index, and starts the per-project
service for non-interactive agent searches when needed.

Use `isgrep` as the default replacement for `grep` when the command is already
written in grep syntax or when grep Basic Regex compatibility matters.
Claude Code installs also include a `PreToolUse` hook that blocks bare Bash
`grep`/`egrep`/`fgrep` and asks the agent to retry with `isgrep`.

## Default Behavior

- Use `is` directly for local code search. Do not call `tool_search` just to
  discover a search tool; `is` is a shell command, not a deferred tool.
- Use `isgrep` before `grep` for ordinary local source searches that are
  naturally expressed as grep commands. It accepts grep-style flags and
  translates default Basic Regex patterns such as `A\|B`.
- Do not read extra base instruction files only to decide between `is` and
  `rg`; this skill is enough.
- Use `is` before `rg` for ordinary local source searches, including trees that
  have not been indexed yet. Do not switch to `rg` just because a common rg flag
  is present; `is` is expected to accept it or ignore it safely.
- If the current directory is inside an indexed repository, use `is` even when
  the immediate subdirectory is small; the index is rooted above it.
- If `is` fails because it is missing, try `indexsearch`. If both are missing,
  fall back to `rg`.
- If `isgrep` fails because it is missing, try converting the command to `is`.
  Fall back to `grep` only when exact grep stdin, binary, PCRE, backreference,
  null-data, or other non-indexed semantics are required.
- Fall back to `rg` only when exact ripgrep semantics are required for PCRE2,
  multiline matching, preprocessors, archive/zip search, or another behavior
  that cannot be approximated by an indexed search.
- Do not preflight with `command -v` unless the search command fails or the
  task is explicitly about installation.

## Common Commands

```bash
is -n "SomeSymbol" .
is -i -w -g "*.cpp" "render pass" .
is --files -g "*.Build.cs" .
is -n -C 3 "SomeSymbol" .
is --color=always "SomeSymbol" .
is -v -F "excluded line" .
is --files-without-match "TODO" .
is --count-matches -F "Tick" .
is -x -F "exact whole line" .
isgrep -n "IndexGraph\|agent interface\|context\|files" MEMORY.md
isgrep -r -n --include="*.cpp" "RenderGraph" .
```

For a pattern that starts with punctuation or looks like an option, use `--`:

```bash
is -- "--help" .
```

In PowerShell, quote patterns containing redirection or pipeline characters and
prefer `--` when the pattern starts with punctuation:

```powershell
is -- ">>>>"
is -- "A|B"
```

If quoted metacharacter patterns fail with a `cmd.exe` syntax error, use
`is.exe`, `isgrep.exe`, or `indexsearch.exe`; an old `is.cmd` shim is likely
earlier on PATH.

## Freshness

- `is PATTERN ...` automatically creates or finds the project index and starts
  the per-project service in non-interactive agent use.
- With a running project service, normal edits are indexed by the service.
- `istool update .` asks the project service to flush pending events and should not
  scan the whole tree in normal watched workflows.
- After pull, checkout, or rebase with no project service running, use
  `istool update --git .`.
- You usually do not need to run indexing commands before searching.
- Use `istool projects`, `istool log .`, `istool stop .`, and `istool stop --all` to
  inspect or stop project services.

## UE Template

For a UE tree without `.indexsearch/is-project-config.txt`, `is` can create the
UE config automatically during non-interactive agent search.
