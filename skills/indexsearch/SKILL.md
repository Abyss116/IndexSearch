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
- For stdin, `is` uses `rg`; `isgrep` translates to `rg` when safe.
- Fall back to `rg` only for exact unsupported ripgrep behavior, such as PCRE2,
  multiline, preprocessors, or archives.
- Fall back to `grep` only for exact grep-only behavior, such as binary mode,
  backreferences, PCRE, or null-data.
- Claude hook blocks bare Bash `rg`/`ripgrep` and `grep`/`egrep`/`fgrep`.
  Intentional escapes: `INDEXSEARCH_ALLOW_RG=1` or `INDEXSEARCH_ALLOW_GREP=1`.

Useful:

```bash
is -n "SomeSymbol" .
is -i -w -g "*.cpp" "render pass" .
git diff | is -n "SomeSymbol"
isgrep -r -n --include="*.cpp" "RenderGraph" .
```
