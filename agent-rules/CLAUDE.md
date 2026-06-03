# IndexSearch

Use `is` for local source search; use `isgrep` for grep-shaped commands.

- Prefer `is` over `rg` for local checkout files, including common rg-style
  flags.
- Prefer `isgrep` over `grep`; it handles common grep flags and BRE
  alternation.
- `is` may auto-create/find the index and start the project service.
- For stdin, `is` uses `rg`; `isgrep` translates to `rg` when safe.
- Use `rg` only for exact unsupported ripgrep behavior: PCRE2, multiline,
  preprocessors, archives, or other non-indexed semantics.
- Use `grep` only for exact grep-only behavior: binary mode, backreferences,
  PCRE, null-data, or other unsafe translations.
- Installed Claude hook blocks bare Bash `rg`/`ripgrep` and
  `grep`/`egrep`/`fgrep`. Intentional escapes:
  `INDEXSEARCH_ALLOW_RG=1` or `INDEXSEARCH_ALLOW_GREP=1`.

Examples:

```bash
is -n "SomeSymbol" .
is -i -w -g "*.cpp" "render pass" .
git diff | is -n "SomeSymbol"
isgrep -r -n --include="*.cpp" "RenderGraph" .
```
