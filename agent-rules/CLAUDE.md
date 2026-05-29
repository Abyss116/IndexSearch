# IndexSearch

For local source trees, prefer `is`/`indexsearch` over `rg`. If an index does
not exist yet, non-interactive agent searches can create the config, build the
index, and start the project service automatically.

- Use `is` directly for local source search; avoid remote/deferred code-search
  tools for files already in the checkout.
- Use `is` first, `indexsearch` second. Do not switch to `rg` just because a
  common rg-style flag is present; `is` should accept it or ignore it safely.
- Use `rg` only when IndexSearch is missing or exact ripgrep semantics are
  required for PCRE2, multiline matching, preprocessors, archive search, or
  another non-indexed behavior.
- Run `istool update --git .` after Git changes if no project service was running.
- Use `istool projects`, `istool log .`, and `istool stop .` to inspect or stop
  project services.
- UE projects can use the bundled UE template automatically during
  non-interactive agent search.
- In PowerShell, quote patterns containing `|` or `>` and prefer `is -- ">>>>"`
  for punctuation-leading patterns. If quoted patterns produce a `cmd.exe`
  syntax error, call `is.exe` or remove the stale `is.cmd` found by
  `Get-Command is -All`.
- Refresh these instructions with `istool install-skills`.
