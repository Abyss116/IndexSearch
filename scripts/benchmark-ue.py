#!/usr/bin/env python3
"""Benchmark IndexSearch, qgrep, ripgrep, and grep on an Unreal Engine checkout."""

from __future__ import annotations

import argparse
import fnmatch
import json
import os
import re
import shutil
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
    "**/DerivedDataCache/**",
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
    "Engine/Programs/UnrealBuildTool/Log*",
    "*.bak",
    "*.cpp.txt",
    "*.gen.cpp",
    "*.gen.h",
    "*.udd",
    "*.uasset",
    "**/ThirdParty/**/*.json",
]

GREP_BIN = os.environ.get("GREP", shutil.which("grep") or "/usr/bin/grep")
GREP_BATCH_SIZE = int(os.environ.get("GREP_BATCH_SIZE", "512"))

GREP_EXCLUDE_DIRS = [
    ".indexsearch",
    ".git",
    ".vs",
    ".vscode",
    "Cooked",
    "DerivedDataCache",
    "Intermediate",
    "Saved",
    "Staged",
    "Content",
    "Binaries",
    "Logs",
    "Private_Projects",
    "obj",
    "bin",
    "enc_temp_folder",
    "LocalBuilds",
]

GREP_EXCLUDE_FILES = [
    "*.bak",
    "*.cpp.txt",
    "*.gen.cpp",
    "*.gen.h",
    "*.udd",
    "*.uasset",
]


def qgrep_config(root: Path, path: Path) -> None:
    text = f"""path {root}
include \\.(bat|c|cc|cpp|cs|cxx|h|hh|hpp|ini|inl|ispc|isph|txt|uplugin|uproject|usf|ush|verse|json|xml)$
exclude (^|.*/)\\.indexsearch/
exclude (^|.*/)\\.git/
exclude (^|.*/)\\.vs/
exclude (^|.*/)\\.vscode/
exclude (^|.*/)Cooked/
exclude (^|.*/)DerivedDataCache/
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


def grep_file_allowed(root: Path, path: Path, include_globs: list[str]) -> bool:
    name = path.name
    if not any(fnmatch.fnmatchcase(name, glob) for glob in include_globs):
        return False
    if any(fnmatch.fnmatchcase(name, glob) for glob in GREP_EXCLUDE_FILES):
        return False

    rel = path.relative_to(root).as_posix()
    if rel.startswith("Engine/Programs/UnrealBuildTool/Log"):
        return False
    if "/ThirdParty/" in f"/{rel}" and name.endswith(".json"):
        return False
    return True


def collect_grep_files(root: Path, include_globs: list[str]) -> list[str]:
    files: list[str] = []
    for dirpath, dirnames, filenames in os.walk(root):
        dirnames[:] = [name for name in dirnames if name not in GREP_EXCLUDE_DIRS]
        base = Path(dirpath)
        for filename in filenames:
            path = base / filename
            if grep_file_allowed(root, path, include_globs):
                files.append(str(path))
    return files


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


def measure_is_stats(cmd: list[str], repeats: int) -> dict[str, float]:
    stats_cmd = [*cmd, "--stats"]
    run(stats_cmd)
    values = []
    for _ in range(repeats):
        proc = subprocess.run(stats_cmd, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE, text=True)
        match = re.search(r"^([0-9]+(?:\.[0-9]+)?)ms$", proc.stderr, re.MULTILINE)
        if not match:
            raise RuntimeError(f"missing is --stats timing for command: {' '.join(stats_cmd)}")
        values.append(float(match.group(1)))
    return {
        "min_ms": min(values),
        "median_ms": statistics.median(values),
        "max_ms": max(values),
    }


def run_grep(files: list[str], grep_args: list[str], pattern: str) -> None:
    for start in range(0, len(files), GREP_BATCH_SIZE):
        batch = files[start : start + GREP_BATCH_SIZE]
        subprocess.run(
            [GREP_BIN, "-I", "-n", *grep_args, "--", pattern, *batch],
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
        )


def measure_grep(files: list[str], grep_args: list[str], pattern: str, repeats: int) -> dict[str, float]:
    values = []
    for _ in range(repeats):
        start = time.perf_counter()
        run_grep(files, grep_args, pattern)
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


def count_grep_lines(files: list[str], grep_args: list[str], pattern: str) -> int:
    count = 0
    for start in range(0, len(files), GREP_BATCH_SIZE):
        batch = files[start : start + GREP_BATCH_SIZE]
        proc = subprocess.run(
            [GREP_BIN, "-I", "-n", *grep_args, "--", pattern, *batch],
            stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL,
        )
        if proc.stdout:
            count += proc.stdout.count(b"\n") + (0 if proc.stdout.endswith(b"\n") else 1)
    return count


def speedup(tool: dict[str, float], baseline: dict[str, float]) -> float | None:
    target = tool["median_ms"]
    if target <= 0:
        return None
    return baseline["median_ms"] / target


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("root", nargs="?", default="/Users/abyss/Projects/UnrealEngine")
    parser.add_argument("--qgrep-config", default="/tmp/ue-qgrep.cfg")
    parser.add_argument("--search-repeats", type=int, default=5)
    parser.add_argument("--rg-repeats", type=int, default=3)
    parser.add_argument("--grep-repeats", type=int, default=1)
    parser.add_argument("--case", action="append", help="Run only the named benchmark case; may be repeated.")
    parser.add_argument("--prepare-qgrep", action="store_true")
    args = parser.parse_args()

    root = Path(args.root).resolve()
    qcfg = Path(args.qgrep_config)
    qgrep_config(root, qcfg)
    if args.prepare_qgrep:
        subprocess.run(
            ["qgrep", "update", str(qcfg)],
            check=True,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
        )

    base_rg = rg_flags(True)
    rg_cpp = rg_flags(False)
    os.chdir(root)
    os.environ["PWD"] = str(root)
    search_root = "."
    cases = [
        (
            "Literal: common token",
            "Nanite",
            ["is", "-F", "Nanite"],
            ["qgrep", "search", str(qcfg), "l", "Nanite"],
            ["rg", *base_rg, "-F", "Nanite", search_root],
            ["-F"],
            "Nanite",
            INCLUDES,
        ),
        (
            "Literal: long symbol",
            "SkeletalMeshComponent",
            ["is", "-F", "SkeletalMeshComponent"],
            ["qgrep", "search", str(qcfg), "l", "SkeletalMeshComponent"],
            ["rg", *base_rg, "-F", "SkeletalMeshComponent", search_root],
            ["-F"],
            "SkeletalMeshComponent",
            INCLUDES,
        ),
        (
            "Literal: missing",
            "DefinitelyMissingIndexSearchNeedle",
            ["is", "-F", "DefinitelyMissingIndexSearchNeedle"],
            ["qgrep", "search", str(qcfg), "l", "DefinitelyMissingIndexSearchNeedle"],
            ["rg", *base_rg, "-F", "DefinitelyMissingIndexSearchNeedle", search_root],
            ["-F"],
            "DefinitelyMissingIndexSearchNeedle",
            INCLUDES,
        ),
        (
            "Case-insensitive literal",
            "skeletalmeshcomponent",
            ["is", "-i", "-F", "skeletalmeshcomponent"],
            ["qgrep", "search", str(qcfg), "il", "skeletalmeshcomponent"],
            ["rg", *base_rg, "-i", "-F", "skeletalmeshcomponent", search_root],
            ["-i", "-F"],
            "skeletalmeshcomponent",
            INCLUDES,
        ),
        (
            "Word regex",
            r"\bActor\b",
            ["is", "-w", "Actor"],
            ["qgrep", "search", str(qcfg), "", r"\bActor\b"],
            ["rg", *base_rg, "-w", "Actor", search_root],
            ["-w"],
            "Actor",
            INCLUDES,
        ),
        (
            "Regex: alternation",
            "(Nanite|Lumen|SkeletalMeshComponent)",
            ["is", "(Nanite|Lumen|SkeletalMeshComponent)"],
            ["qgrep", "search", str(qcfg), "", "(Nanite|Lumen|SkeletalMeshComponent)"],
            ["rg", *base_rg, "(Nanite|Lumen|SkeletalMeshComponent)", search_root],
            ["-E"],
            "(Nanite|Lumen|SkeletalMeshComponent)",
            INCLUDES,
        ),
        (
            "Regex: prefix/suffix",
            "Skeletal[A-Za-z0-9_]*Component",
            ["is", "Skeletal[A-Za-z0-9_]*Component"],
            ["qgrep", "search", str(qcfg), "", "Skeletal[A-Za-z0-9_]*Component"],
            ["rg", *base_rg, "Skeletal[A-Za-z0-9_]*Component", search_root],
            ["-E"],
            "Skeletal[A-Za-z0-9_]*Component",
            INCLUDES,
        ),
        (
            "Regex: qualified call",
            r"[A-Za-z_][A-Za-z0-9_]*::[A-Za-z0-9_]+\(",
            ["is", r"[A-Za-z_][A-Za-z0-9_]*::[A-Za-z0-9_]+\("],
            ["qgrep", "search", str(qcfg), "", r"[A-Za-z_][A-Za-z0-9_]*::[A-Za-z0-9_]+\("],
            ["rg", *base_rg, r"[A-Za-z_][A-Za-z0-9_]*::[A-Za-z0-9_]+\(", search_root],
            ["-E"],
            r"[A-Za-z_][A-Za-z0-9_]*::[A-Za-z0-9_]+\(",
            INCLUDES,
        ),
        (
            "Glob: *.cpp literal",
            "Nanite in *.cpp",
            ["is", "-F", "-g", "*.cpp", "Nanite"],
            ["qgrep", "search", str(qcfg), r"lfi\.cpp$", "Nanite"],
            ["rg", *rg_cpp, "-g", "*.cpp", "-F", "Nanite", search_root],
            ["-F"],
            "Nanite",
            ["*.cpp"],
        ),
    ]
    if args.case:
        selected = set(args.case)
        known = {case[0] for case in cases}
        unknown = selected - known
        if unknown:
            parser.error(f"unknown benchmark case(s): {', '.join(sorted(unknown))}")
        cases = [case for case in cases if case[0] in selected]

    subprocess.run(["is", "-F", "Nanite"], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    rows = []
    for name, pattern, is_cmd, qgrep_cmd, rg_cmd, grep_args, grep_pattern, grep_include_globs in cases:
        timings = {
            "indexsearch": measure_is_stats(is_cmd, args.search_repeats),
            "qgrep": measure(qgrep_cmd, args.search_repeats),
            "rg": measure(rg_cmd, args.rg_repeats),
        }
        matches = {
            "indexsearch": count_lines(is_cmd),
            "qgrep": count_lines(qgrep_cmd),
            "rg": count_lines(rg_cmd),
        }
        grep_files = collect_grep_files(root, grep_include_globs)
        timings["grep"] = measure_grep(grep_files, grep_args, grep_pattern, args.grep_repeats)
        matches["grep"] = count_grep_lines(grep_files, grep_args, grep_pattern)
        row = {
            "case": name,
            "pattern": pattern,
            "grep_files": len(grep_files),
            "matches": matches,
            "timings": timings,
            "speedups": {
                "vs_qgrep": speedup(timings["indexsearch"], timings["qgrep"]),
                "vs_rg": speedup(timings["indexsearch"], timings["rg"]),
                "vs_grep": speedup(timings["indexsearch"], timings["grep"]),
            },
        }
        rows.append(row)
        print(json.dumps(row), flush=True)

    print(json.dumps({"rows": rows}, indent=2), flush=True)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
