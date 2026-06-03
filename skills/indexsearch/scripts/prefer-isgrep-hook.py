#!/usr/bin/env python3
"""Claude Code PreToolUse hook that nudges Bash rg/grep calls to is/isgrep."""

from __future__ import annotations

import json
import os
import re
import shlex
import sys


WRAPPER_COMMANDS = {"command", "builtin", "env"}
SEPARATORS = {";", "&&", "||", "|", "(", ")"}
GREP_COMMANDS = {"grep", "egrep", "fgrep"}
RG_COMMANDS = {"rg", "ripgrep"}


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


def first_blocked_search(command: str) -> tuple[str, str] | None:
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
            continue
        name = os.path.basename(token)
        if name in WRAPPER_COMMANDS:
            passthrough_wrapper = True
            continue
        if passthrough_wrapper and token.startswith("-"):
            continue
        if name in GREP_COMMANDS:
            return name, "isgrep"
        if name in RG_COMMANDS:
            return name, "is"
        expect_command = False
        passthrough_wrapper = False
    return None


def main() -> int:
    try:
        payload = json.load(sys.stdin)
    except json.JSONDecodeError:
        return 0
    if payload.get("tool_name") != "Bash":
        return 0
    command = payload.get("tool_input", {}).get("command") or ""
    blocked = first_blocked_search(command)
    if not blocked:
        return 0
    blocked_cmd, replacement = blocked
    print(
        f"Blocked bare `{blocked_cmd}`. Use `{replacement}` for indexed local "
        "source search. Exit code 1 from `is`/`isgrep` means no matches; "
        "adjust the pattern or path while staying on IndexSearch.",
        file=sys.stderr,
    )
    return 2


if __name__ == "__main__":
    raise SystemExit(main())
