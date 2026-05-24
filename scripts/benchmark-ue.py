#!/usr/bin/env python3
"""Benchmark IndexSearch, qgrep, and ripgrep on an Unreal Engine checkout."""

from __future__ import annotations

import argparse
import json
import os
import statistics
import subprocess
import time
from pathlib import Path


INCLUDES = [
    "*.bat",
    "*.c",
    "*.cc",
    "*.cpp",
    "*.cs",
    "*.cxx",
    "*.h",
    "*.hh",
    "*.hpp",
    "*.ini",
    "*.inl",
    "*.ispc",
    "*.isph",
    "*.txt",
    "*.uplugin",
    "*.uproject",
    "*.usf",
    "*.ush",
    "*.verse",
    "*.json",
    "*.xml",
]

EXCLUDES = [
    ".indexsearch/**",
    ".git/**",
    ".vs/**",
    ".vscode/**",
    "**/Cooked/**",
    "**/DerivedDataCache/0/**",
    "**/DerivedDataCache/1/**",
    "**/DerivedDataCache/2/**",
    "**/DerivedDataCache/3/**",
    "**/DerivedDataCache/4/**",
    "**/DerivedDataCache/5/**",
    "**/DerivedDataCache/6/**",
    "**/DerivedDataCache/7/**",
    "**/DerivedDataCache/8/**",
    "**/DerivedDataCache/9/**",
    "**/Intermediate/**",
    "**/Saved/**",
    "**/Staged/**",
    "**/Content/**",
    "**/Binaries/**",
    "**/Logs/**",
    "**/Private_Projects/**",
    "**/obj/**",
    "**/bin/**",
    "**/enc_temp_folder/**",
    "**/LocalBuilds/**",
    "index-search-project.txt",
    "Engine/Programs/UnrealBuildTool/Log*",
    "*.bak",
    "*.cpp.txt",
    "*.gen.cpp",
    "*.gen.h",
    "*.udd",
    "*.uasset",
    "**/ThirdParty/**/*.json",
]


def qgrep_config(root: Path, path: Path) -> None:
    text = f"""path {root}
include \\.(bat|c|cc|cpp|cs|cxx|h|hh|hpp|ini|inl|ispc|isph|txt|uplugin|uproject|usf|ush|verse|json|xml)$
exclude (^|.*/)\\.indexsearch/
exclude (^|.*/)\\.git/
exclude (^|.*/)\\.vs/
exclude (^|.*/)\\.vscode/
exclude (^|.*/)Cooked/
exclude (^|.*/)DerivedDataCache/[0-9]
exclude (^|.*/)Intermediate/
exclude (^|.*/)Saved/
exclude (^|.*/)Staged/
exclude (^|.*/)Content/
exclude (^|.*/)Binaries/
exclude (^|.*/)Logs/
exclude (^|.*/)Private_Projects/
exclude (^|.*/)obj/
exclude (^|.*/)bin/
exclude (^|.*/)enc_temp_folder/
exclude (^|.*/)LocalBuilds/
exclude index-search-project\\.txt$
exclude Engine/Programs/UnrealBuildTool/Log.*
exclude \\.bak$
exclude \\.cpp\\.txt$
exclude \\.gen\\.cpp$
exclude \\.gen\\.h$
exclude \\.udd$
exclude \\.uasset$
exclude (^|.*/)ThirdParty/.*\\.json$
"""
    path.write_text(text)


def rg_flags(include_globs: bool = True) -> list[str]:
    flags = ["--hidden", "--no-ignore-vcs"]
    if include_globs:
        for glob in INCLUDES:
            flags += ["-g", glob]
    for glob in EXCLUDES:
        flags += ["-g", "!" + glob]
    return flags


def run(cmd: list[str]) -> int:
    return subprocess.run(cmd, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL).returncode


def measure(cmd: list[str], repeats: int) -> dict[str, float]:
    run(cmd)
    values = []
    for _ in range(repeats):
        start = time.perf_counter()
        run(cmd)
        values.append((time.perf_counter() - start) * 1000)
    return {
        "min_ms": min(values),
        "median_ms": statistics.median(values),
        "max_ms": max(values),
    }


def count_lines(cmd: list[str]) -> int:
    proc = subprocess.run(cmd, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL)
    if not proc.stdout:
        return 0
    return proc.stdout.count(b"\n") + (0 if proc.stdout.endswith(b"\n") else 1)


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("root", nargs="?", default="/Users/abyss/Projects/UnrealEngine")
    parser.add_argument("--qgrep-config", default="/tmp/ue-qgrep.cfg")
    parser.add_argument("--search-repeats", type=int, default=5)
    parser.add_argument("--rg-repeats", type=int, default=3)
    parser.add_argument("--prepare-qgrep", action="store_true")
    args = parser.parse_args()

    root = Path(args.root).resolve()
    qcfg = Path(args.qgrep_config)
    qgrep_config(root, qcfg)
    if args.prepare_qgrep:
        subprocess.run(["qgrep", "update", str(qcfg)], check=True)

    base_rg = rg_flags(True)
    rg_cpp = rg_flags(False)
    cases = [
        (
            "Literal: common token",
            "Nanite",
            ["is", "-F", "Nanite", str(root)],
            ["qgrep", "search", str(qcfg), "l", "Nanite"],
            ["rg", *base_rg, "-F", "Nanite", str(root)],
        ),
        (
            "Literal: long symbol",
            "SkeletalMeshComponent",
            ["is", "-F", "SkeletalMeshComponent", str(root)],
            ["qgrep", "search", str(qcfg), "l", "SkeletalMeshComponent"],
            ["rg", *base_rg, "-F", "SkeletalMeshComponent", str(root)],
        ),
        (
            "Literal: missing",
            "DefinitelyMissingIndexSearchNeedle",
            ["is", "-F", "DefinitelyMissingIndexSearchNeedle", str(root)],
            ["qgrep", "search", str(qcfg), "l", "DefinitelyMissingIndexSearchNeedle"],
            ["rg", *base_rg, "-F", "DefinitelyMissingIndexSearchNeedle", str(root)],
        ),
        (
            "Case-insensitive literal",
            "skeletalmeshcomponent",
            ["is", "-i", "-F", "skeletalmeshcomponent", str(root)],
            ["qgrep", "search", str(qcfg), "il", "skeletalmeshcomponent"],
            ["rg", *base_rg, "-i", "-F", "skeletalmeshcomponent", str(root)],
        ),
        (
            "Word regex",
            r"\bActor\b",
            ["is", "-w", "Actor", str(root)],
            ["qgrep", "search", str(qcfg), "", r"\bActor\b"],
            ["rg", *base_rg, "-w", "Actor", str(root)],
        ),
        (
            "Regex: alternation",
            "(Nanite|Lumen|SkeletalMeshComponent)",
            ["is", "(Nanite|Lumen|SkeletalMeshComponent)", str(root)],
            ["qgrep", "search", str(qcfg), "", "(Nanite|Lumen|SkeletalMeshComponent)"],
            ["rg", *base_rg, "(Nanite|Lumen|SkeletalMeshComponent)", str(root)],
        ),
        (
            "Regex: prefix/suffix",
            "Skeletal[A-Za-z0-9_]*Component",
            ["is", "Skeletal[A-Za-z0-9_]*Component", str(root)],
            ["qgrep", "search", str(qcfg), "", "Skeletal[A-Za-z0-9_]*Component"],
            ["rg", *base_rg, "Skeletal[A-Za-z0-9_]*Component", str(root)],
        ),
        (
            "Regex: qualified call",
            r"[A-Za-z_][A-Za-z0-9_]*::[A-Za-z0-9_]+\(",
            ["is", r"[A-Za-z_][A-Za-z0-9_]*::[A-Za-z0-9_]+\(", str(root)],
            ["qgrep", "search", str(qcfg), "", r"[A-Za-z_][A-Za-z0-9_]*::[A-Za-z0-9_]+\("],
            ["rg", *base_rg, r"[A-Za-z_][A-Za-z0-9_]*::[A-Za-z0-9_]+\(", str(root)],
        ),
        (
            "Glob: *.cpp literal",
            "Nanite in *.cpp",
            ["is", "-F", "-g", "*.cpp", "Nanite", str(root)],
            ["qgrep", "search", str(qcfg), r"lfi\.cpp$", "Nanite"],
            ["rg", *rg_cpp, "-g", "*.cpp", "-F", "Nanite", str(root)],
        ),
    ]

    subprocess.run(["is", "-F", "Nanite", str(root)], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    rows = []
    for name, pattern, is_cmd, qgrep_cmd, rg_cmd in cases:
        row = {
            "case": name,
            "pattern": pattern,
            "matches": {
                "indexsearch": count_lines(is_cmd),
                "qgrep": count_lines(qgrep_cmd),
                "rg": count_lines(rg_cmd),
            },
            "timings": {
                "indexsearch": measure(is_cmd, args.search_repeats),
                "qgrep": measure(qgrep_cmd, args.search_repeats),
                "rg": measure(rg_cmd, args.rg_repeats),
            },
        }
        rows.append(row)
        print(json.dumps(row))

    print(json.dumps({"rows": rows}, indent=2))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
