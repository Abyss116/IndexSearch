#!/usr/bin/env python3
"""Claude Code PreToolUse hook that nudges Bash grep calls to isgrep."""

from __future__ import annotations

import json
import os
import re
import shlex
import sys


WRAPPER_COMMANDS = {"command", "builtin", "env"}
SEPARATORS = {";", "&&", "||", "|", "(", ")"}


def shell_tokens(command: str) -> list[str]:
    try:
        lexer = shlex.shlex(command, posix=True, punctuation_chars=";&|()")
        lexer.whitespace_split = True
        lexer.commenters = ""
        return list(lexer)
    except ValueError:
        return []


def is_assignment(token: str) -> bool:
    return re.match(r"^[A-Za-z_][A-Za-z0-9_]*=", token) is not None


def assignment_allows_grep(token: str) -> bool:
    if not token.startswith("INDEXSEARCH_ALLOW_GREP="):
        return False
    value = token.split("=", 1)[1].strip().lower()
    return value not in {"", "0", "false", "no"}


def first_blocked_grep(command: str) -> str | None:
    tokens = shell_tokens(command)
    expect_command = True
    passthrough_wrapper = False
    for token in tokens:
        if token in SEPARATORS:
            expect_command = True
            passthrough_wrapper = False
            continue
        if not expect_command:
            continue
        if is_assignment(token):
            if assignment_allows_grep(token):
                return None
            continue
        name = os.path.basename(token)
        if passthrough_wrapper or name in WRAPPER_COMMANDS:
            if name in WRAPPER_COMMANDS:
                passthrough_wrapper = True
                continue
        if name in {"grep", "egrep", "fgrep"}:
            return name
        expect_command = False
        passthrough_wrapper = False
    return None


def main() -> int:
    if os.environ.get("INDEXSEARCH_ALLOW_GREP"):
        return 0
    try:
        payload = json.load(sys.stdin)
    except json.JSONDecodeError:
        return 0
    if payload.get("tool_name") != "Bash":
        return 0
    command = payload.get("tool_input", {}).get("command") or ""
    blocked = first_blocked_grep(command)
    if not blocked:
        return 0
    print(
        f"Blocked bare `{blocked}`. Use `isgrep` for indexed local source search; "
        "set INDEXSEARCH_ALLOW_GREP=1 only when exact grep stdin/binary/PCRE "
        "semantics are required.",
        file=sys.stderr,
    )
    return 2


if __name__ == "__main__":
    raise SystemExit(main())
