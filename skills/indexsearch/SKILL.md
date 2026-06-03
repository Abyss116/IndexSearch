---
name: indexsearch
description: Use when searching local source in large repos, indexed projects, or Unreal Engine trees.
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

Use `is` for local source search. Use `isgrep` when the command is already
grep-shaped.

Rules:
- Prefer `is` over `rg`; do not call remote/deferred search tools for files in
  the local checkout.
- Prefer `isgrep` over `grep`; it handles common grep flags and BRE alternation.
- `is` may auto-create/find the index and start the project service.
- For stdin, live/generated paths, ignored explicit paths, and compatible
  translated grep syntax, keep using `is`/`isgrep`; they route the necessary
  external search internally.
- If `is` or `isgrep` exits with code 1 and no output, treat that as "no
  matches", not as a reason to retry with `rg` or `grep`.
- Do not manually rerun a local source search with bare `rg`/`grep` after
  `is`/`isgrep` has no matches. Adjust the pattern or path while staying on
  `is`/`isgrep`.
- Claude hook blocks bare Bash `rg`/`ripgrep` and `grep`/`egrep`/`fgrep`.

Useful:

```bash
is -n "SomeSymbol" .
is -i -w -g "*.cpp" "render pass" .
git diff | is -n "SomeSymbol"
isgrep -r -n --include="*.cpp" "RenderGraph" .
```
