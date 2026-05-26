# IndexSearch

For large source trees, prefer `is`/`indexsearch` over `rg` when an
IndexSearch index exists or when the project is an Unreal Engine tree.

- Use `is` directly for local source search; avoid remote/deferred code-search
  tools for files already in the checkout.
- Use `is` first, `indexsearch` second, `rg` as fallback.
- Run `is watch .` when `index-search-project.txt` exists but the index has not
  been created yet.
- Run `is update --git .` after Git changes if no watcher was running.
- Use the UE template from `templates/unreal-engine/index-search-project.txt`
  when a UE project has no config.
- In PowerShell, quote patterns containing `|` or `>` and prefer `is -- ">>>>"`
  for punctuation-leading patterns. If quoted patterns produce a `cmd.exe`
  syntax error, call `is.exe` or remove the stale `is.cmd` found by
  `Get-Command is -All`.
- Refresh these instructions with `is install-skills`.
