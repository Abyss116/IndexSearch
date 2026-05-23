use std::borrow::Cow;
use std::cmp::Ordering;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::env;
use std::fs::{self, File, OpenOptions};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::{Duration, Instant};

use aho_corasick::{AhoCorasick, AhoCorasickBuilder};
use anyhow::{Context, Result, anyhow, bail};
use fs2::FileExt;
use globset::{Glob, GlobSet, GlobSetBuilder};
use memchr::{memchr, memmem};
use memmap2::Mmap;
use notify::{Config as NotifyConfig, Event, RecommendedWatcher, RecursiveMode, Watcher};
use rayon::prelude::*;
use regex::bytes::{Regex, RegexBuilder};
use walkdir::{DirEntry, WalkDir};

const PROJECT_FILE: &str = "index-search-project.txt";
const INDEX_DIR: &str = ".indexsearch";
const INDEX_FILE: &str = "index.bin";
const DELTA_DIR: &str = "deltas";
const WATCH_DIR: &str = "watches";
const LOCK_FILE: &str = "index.lock";
const STATE_FILE: &str = "state.txt";
const WATCH_LOG_FILE: &str = "watch.log";
const DEFAULT_PROJECT_CONFIG: &str = "[IndexSearch.paths.ignore]\n.git/\n.hg/\n.svn/\n.indexsearch/\n\n\
[IndexSearch.files.ignore]\n*.png\n*.jpg\n*.jpeg\n*.gif\n*.pdf\n*.zip\n*.gz\n*.dll\n*.exe\n*.pdb\n*.o\n*.obj\n\n\
[IndexSearch.files.include]\n*\n";
const MAGIC: &[u8; 8] = b"ISIDXR02";
const VERSION: u32 = 2;
const DEFAULT_MAX_FILE_SIZE: u64 = 20 * 1024 * 1024;

#[derive(Clone)]
struct ProjectConfig {
    root: PathBuf,
    path: Option<PathBuf>,
    paths_ignore: MatcherSet,
    files_ignore: MatcherSet,
    files_include: MatcherSet,
    hash: u64,
}

#[derive(Clone, Default)]
struct MatcherSet {
    set: Option<GlobSet>,
}

impl MatcherSet {
    fn new(patterns: &[String]) -> Result<Self> {
        if patterns.is_empty() {
            return Ok(Self { set: None });
        }
        let mut builder = GlobSetBuilder::new();
        for pat in patterns {
            add_glob_pattern(&mut builder, pat)?;
        }
        Ok(Self {
            set: Some(builder.build()?),
        })
    }

    fn is_match(&self, rel: &str) -> bool {
        self.set.as_ref().is_some_and(|set| set.is_match(rel))
    }
}

#[derive(Default, Clone)]
struct Options {
    ignore_case: bool,
    smart_case: bool,
    fixed: bool,
    whole_word: bool,
    line_number: bool,
    column: bool,
    with_filename: Option<bool>,
    files_with_matches: bool,
    count: bool,
    files: bool,
    vimgrep: bool,
    stats: bool,
    quiet: bool,
    only_matching: bool,
    json: bool,
    auto_index: bool,
    git_update: bool,
    git_untracked: bool,
    hidden: bool,
    follow: bool,
    max_count: Option<usize>,
    max_filesize: u64,
    pattern: String,
    globs: Vec<String>,
    paths: Vec<String>,
}

#[derive(Clone)]
struct FileEntry {
    path: String,
    mtime: i64,
    size: u64,
    content: Vec<u8>,
}

struct BuiltIndex {
    root: PathBuf,
    config_hash: u64,
    files: Vec<FileEntry>,
    postings: BTreeMap<u32, Vec<u32>>,
}

#[derive(Default)]
struct Timings {
    git: f64,
    scan: f64,
    process: f64,
    write: f64,
}

struct DeltaSegment {
    index: MappedIndex,
    meta: DeltaMeta,
}

#[derive(Default)]
struct DeltaMeta {
    tombstones: HashSet<String>,
}

enum IndexLock {
    Shared(File),
    Exclusive(File),
}

impl Drop for IndexLock {
    fn drop(&mut self) {
        match self {
            IndexLock::Shared(file) | IndexLock::Exclusive(file) => {
                let _ = file.unlock();
            }
        }
    }
}

struct WatchRecord {
    id: String,
    root: PathBuf,
    pid: u32,
}

#[derive(Clone, Copy)]
struct WatchOptions {
    idle_seconds: u64,
    compact_delta_count: usize,
    compact_delta_bytes: u64,
}

impl Default for WatchOptions {
    fn default() -> Self {
        Self {
            idle_seconds: 5,
            compact_delta_count: 16,
            compact_delta_bytes: 256 * 1024 * 1024,
        }
    }
}

struct CurrentFile {
    ordinal: usize,
    path: PathBuf,
    rel: String,
    mtime: i64,
    size: u64,
}

#[derive(Default)]
struct UpdateStats {
    reused: u64,
    added: u64,
    updated: u64,
    removed: u64,
}

#[derive(Clone)]
struct ChangedPath {
    rel: String,
    deleted: bool,
}

#[derive(Default)]
struct IndexState {
    git_head: Option<String>,
}

#[derive(Clone, Copy)]
enum ChangeKind {
    Reused,
    Added,
    Updated,
}

#[derive(Clone, Copy)]
struct FileView<'a> {
    path: &'a [u8],
    content: &'a [u8],
}

#[derive(Clone, Copy)]
struct PostingView<'a> {
    data: &'a [u32],
}

struct MappedIndex {
    mmap: Mmap,
    root: PathBuf,
    config_hash: u64,
    file_count: usize,
    posting_count: usize,
    file_table_offset: usize,
    posting_table_offset: usize,
    postings_data_offset: usize,
    path_blob_offset: usize,
    content_blob_offset: usize,
}

#[derive(Clone, Copy)]
struct Header {
    config_hash: u64,
    file_count: u64,
    posting_count: u64,
    root_offset: u64,
    root_size: u64,
    file_table_offset: u64,
    posting_table_offset: u64,
    postings_data_offset: u64,
    path_blob_offset: u64,
    content_blob_offset: u64,
}

#[derive(Clone, Copy)]
struct FileRecord {
    path_offset: u64,
    path_size: u64,
    content_offset: u64,
    content_size: u64,
    mtime: i64,
    size: u64,
}

#[derive(Clone, Copy)]
struct PostingRecord {
    gram: u32,
    offset: u64,
    count: u64,
}

#[derive(Clone)]
struct MatchLine {
    line_no: usize,
    column: usize,
    line: Vec<u8>,
    matched: Vec<u8>,
}

#[derive(Clone)]
struct FileResult {
    path: String,
    matches: Vec<MatchLine>,
}

fn main() {
    if let Err(err) = run() {
        eprintln!("indexsearch: {err:#}");
        std::process::exit(2);
    }
}

fn run() -> Result<()> {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() {
        print_help();
        std::process::exit(2);
    }
    match args[0].as_str() {
        "index" => {
            args.remove(0);
            std::process::exit(command_index(&args)?);
        }
        "update" => {
            args.remove(0);
            std::process::exit(command_update(&args)?);
        }
        "compact" => {
            args.remove(0);
            std::process::exit(command_compact(&args)?);
        }
        "watch" => {
            args.remove(0);
            std::process::exit(command_watch(&args)?);
        }
        "watch-daemon" => {
            args.remove(0);
            std::process::exit(command_watch_daemon(&args)?);
        }
        "list-watches" | "watch-list" => {
            args.remove(0);
            std::process::exit(command_list_watches(&args)?);
        }
        "unwatch" => {
            args.remove(0);
            std::process::exit(command_unwatch(&args)?);
        }
        "watch-log" => {
            args.remove(0);
            std::process::exit(command_watch_log(&args)?);
        }
        "install" => {
            args.remove(0);
            std::process::exit(command_install(&args)?);
        }
        "status" => {
            args.remove(0);
            std::process::exit(command_status(&args)?);
        }
        "search" => {
            args.remove(0);
            std::process::exit(command_search(&args)?);
        }
        "-h" | "--help" => {
            print_help();
            Ok(())
        }
        "-V" | "--version" | "version" => {
            println!("indexsearch {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        _ => std::process::exit(command_search(&args)?),
    }
}

fn print_help() {
    println!(
        "usage: indexsearch [OPTIONS] PATTERN [PATH ...]\n       indexsearch <index|update|compact|watch|list-watches|unwatch|watch-log|install|status|search> [ARGS]\n\n\
Common rg-like options:\n\
  -i, --ignore-case        case insensitive search\n\
  -s, --case-sensitive     case sensitive search\n\
  -S, --smart-case         smart case search\n\
  -F, --fixed-strings      treat pattern as a literal\n\
  -w, --word-regexp        require word boundaries\n\
  -e, --regexp PATTERN     search pattern\n\
  -g, --glob GLOB          include/exclude path glob\n\
  -n, --line-number        show line numbers\n\
  -N, --no-line-number     suppress line numbers\n\
      --column             show columns\n\
  -H, --with-filename      always show file names\n\
  -I, --no-filename        never show file names\n\
  -l, --files-with-matches print matching file paths\n\
  -c, --count              print match counts per file\n\
  -o, --only-matching      print only matching text\n\
  -q, --quiet              suppress normal output\n\
      --files              print indexed searchable files\n\
      --json               print JSON Lines matches\n\
      --vimgrep            print path:line:column:line\n\
  -m, --max-count NUM      max matching lines per file\n\
      --max-filesize SIZE  skip larger files while indexing\n\
      --hidden             index/search hidden paths\n\
  -L, --follow             follow symlinks while indexing\n\
      --git                update from Git changed paths when possible\n\
      --git-untracked      include untracked files with --git update\n\
      --stats              print search stats\n\n\
Index/update options:\n\
      --max-filesize SIZE  skip larger files while indexing\n\
      --hidden             include hidden paths while indexing\n\
  -L, --follow             follow symlinks while indexing\n\
      --git                update from Git changed paths when possible\n\
      --git-untracked      include untracked files with --git update\n\n\
Watch options:\n\
      --idle-seconds NUM           idle seconds before checking auto compact (default: 5)\n\
      --compact-delta-count NUM    compact after this many delta segments (default: 16)\n\
      --compact-delta-bytes SIZE   compact after this total delta size (default: 256mb)\n\n\
Install options:\n\
      --dir PATH           install indexsearch and is into PATH"
    );
}

fn command_index(args: &[String]) -> Result<i32> {
    let (options, start) = parse_index_args(args)?;
    let cfg = load_or_create_config(&start)?;
    let _lock = acquire_exclusive_lock(&cfg.root)?;
    let timer = Instant::now();
    let mut timings = Timings::default();
    let mut scanned = 0;
    let mut skipped = 0;
    let index = build_index(
        &cfg,
        &options,
        &mut scanned,
        &mut skipped,
        Some(&mut timings),
    )?;
    let write_timer = Instant::now();
    save_index(&index, &index_path(&cfg.root))?;
    remove_delta_dir(&cfg.root)?;
    save_index_state(&cfg.root)?;
    timings.write += write_timer.elapsed().as_secs_f64();
    let elapsed = timer.elapsed().as_secs_f64();
    println!(
        "indexed {} files ({} skipped, {} scanned) in {:.3}s",
        index.files.len(),
        skipped,
        scanned,
        elapsed
    );
    print_timings(&timings);
    println!("root: {}", cfg.root.display());
    println!("index: {}", index_path(&cfg.root).display());
    Ok(0)
}

fn command_update(args: &[String]) -> Result<i32> {
    let (options, start) = parse_index_args(args)?;
    let cfg = load_or_create_config(&start)?;
    let _lock = acquire_exclusive_lock(&cfg.root)?;
    let path = index_path(&cfg.root);
    let timer = Instant::now();
    let mut timings = Timings::default();
    let mut scanned = 0;
    let mut skipped = 0;
    let old = MappedIndex::open(&path).ok();
    let (index, stats, rebuilt) = if let Some(ref old_index) = old {
        if old_index.config_hash == cfg.hash {
            let (index, stats) = if options.git_update {
                let git_timer = Instant::now();
                let changes = collect_git_changes(&cfg.root, options.git_untracked)?;
                timings.git += git_timer.elapsed().as_secs_f64();
                match changes {
                    Some(changes) if changes.is_empty() => {
                        println!(
                            "updated {} files (0 changed by git) in {:.3}s",
                            old_index.file_count,
                            timer.elapsed().as_secs_f64()
                        );
                        print_timings(&timings);
                        println!("root: {}", cfg.root.display());
                        println!("index: {}", path.display());
                        save_index_state(&cfg.root)?;
                        return Ok(0);
                    }
                    Some(changes) => {
                        let process_timer = Instant::now();
                        let (delta, meta, stats) = build_delta_index(
                            &cfg,
                            &options,
                            &changes,
                            &mut scanned,
                            &mut skipped,
                        )?;
                        timings.process += process_timer.elapsed().as_secs_f64();
                        let write_timer = Instant::now();
                        save_delta(&cfg.root, &delta, &meta)?;
                        save_index_state(&cfg.root)?;
                        timings.write += write_timer.elapsed().as_secs_f64();
                        let elapsed = timer.elapsed().as_secs_f64();
                        let visible_count = stats.reused + stats.updated + stats.added;
                        println!(
                            "updated {} files ({} reused, {} added, {} modified, {} removed, {} skipped, {} scanned) in {:.3}s",
                            visible_count,
                            stats.reused,
                            stats.added,
                            stats.updated,
                            stats.removed,
                            skipped,
                            scanned,
                            elapsed
                        );
                        print_timings(&timings);
                        println!("root: {}", cfg.root.display());
                        println!("index: {}", path.display());
                        println!("delta: {}", delta_dir(&cfg.root).display());
                        return Ok(0);
                    }
                    None => update_index(
                        &cfg,
                        &options,
                        old_index,
                        &mut scanned,
                        &mut skipped,
                        Some(&mut timings),
                    )?,
                }
            } else {
                update_index(
                    &cfg,
                    &options,
                    old_index,
                    &mut scanned,
                    &mut skipped,
                    Some(&mut timings),
                )?
            };
            (index, stats, false)
        } else {
            let index = build_index(
                &cfg,
                &options,
                &mut scanned,
                &mut skipped,
                Some(&mut timings),
            )?;
            (index, UpdateStats::default(), true)
        }
    } else {
        let index = build_index(
            &cfg,
            &options,
            &mut scanned,
            &mut skipped,
            Some(&mut timings),
        )?;
        (index, UpdateStats::default(), true)
    };
    drop(old);
    let write_timer = Instant::now();
    save_index(&index, &path)?;
    remove_delta_dir(&cfg.root)?;
    save_index_state(&cfg.root)?;
    timings.write += write_timer.elapsed().as_secs_f64();
    let elapsed = timer.elapsed().as_secs_f64();
    if rebuilt {
        println!(
            "indexed {} files ({} skipped, {} scanned) in {:.3}s",
            index.files.len(),
            skipped,
            scanned,
            elapsed
        );
    } else {
        println!(
            "updated {} files ({} reused, {} added, {} modified, {} removed, {} skipped, {} scanned) in {:.3}s",
            index.files.len(),
            stats.reused,
            stats.added,
            stats.updated,
            stats.removed,
            skipped,
            scanned,
            elapsed
        );
    }
    print_timings(&timings);
    println!("root: {}", cfg.root.display());
    println!("index: {}", path.display());
    Ok(0)
}

fn parse_index_args(args: &[String]) -> Result<(Options, PathBuf)> {
    let mut options = Options {
        max_filesize: DEFAULT_MAX_FILE_SIZE,
        ..Options::default()
    };
    let mut start = std::env::current_dir()?;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--hidden" => options.hidden = true,
            "-L" | "--follow" => options.follow = true,
            "--git" => options.git_update = true,
            "--git-untracked" => {
                options.git_update = true;
                options.git_untracked = true;
            }
            "--max-filesize" => {
                i += 1;
                options.max_filesize =
                    parse_size(args.get(i).context("missing --max-filesize value")?)?;
            }
            value => start = PathBuf::from(value),
        }
        i += 1;
    }
    Ok((options, start))
}

fn command_watch(args: &[String]) -> Result<i32> {
    let (watch_options, start) = parse_watch_args(args)?;
    let cfg = load_or_create_config(&start)?;
    fs::create_dir_all(watch_registry_dir())?;
    let id = watch_id(&cfg.root);
    let record_path = watch_record_path(&id);
    let requested_root = fs::canonicalize(&cfg.root).unwrap_or_else(|_| cfg.root.clone());
    let mut child_watches = Vec::new();
    for entry in fs::read_dir(watch_registry_dir())?.flatten() {
        let path = entry.path();
        if !path.extension().is_some_and(|ext| ext == "watch") {
            continue;
        }
        let Ok(record) = read_watch_record(&path) else {
            let _ = fs::remove_file(path);
            continue;
        };
        if !process_alive(record.pid) {
            let _ = fs::remove_file(path);
            continue;
        }
        let record_root = fs::canonicalize(&record.root).unwrap_or_else(|_| record.root.clone());
        if path_is_ancestor(&record_root, &requested_root) {
            println!(
                "watch already covered: {} pid={} covers {}",
                record.root.display(),
                record.pid,
                cfg.root.display()
            );
            return Ok(0);
        }
        if path_is_ancestor(&requested_root, &record_root) {
            child_watches.push((path, record));
        }
    }
    for (path, record) in child_watches {
        stop_process(record.pid);
        let _ = fs::remove_file(path);
        let _ = append_watch_log(
            &record.root,
            &format!(
                "watch-stop pid={} superseded_by={}",
                record.pid,
                cfg.root.display()
            ),
        );
        println!(
            "stopped child watch {} pid={} {}",
            record.id,
            record.pid,
            record.root.display()
        );
    }
    if record_path.exists() {
        let _ = fs::remove_file(&record_path);
    }

    if !index_path(&cfg.root).exists() {
        let _lock = acquire_exclusive_lock(&cfg.root)?;
        let timer = Instant::now();
        let mut timings = Timings::default();
        let mut scanned = 0;
        let mut skipped = 0;
        let index = build_index(
            &cfg,
            &Options {
                max_filesize: DEFAULT_MAX_FILE_SIZE,
                ..Options::default()
            },
            &mut scanned,
            &mut skipped,
            Some(&mut timings),
        )?;
        let write_timer = Instant::now();
        save_index(&index, &index_path(&cfg.root))?;
        save_index_state(&cfg.root)?;
        timings.write += write_timer.elapsed().as_secs_f64();
        append_watch_log(
            &cfg.root,
            &format!(
                "initial-index files={} skipped={} scanned={} elapsed={:.3}s {}",
                index.files.len(),
                skipped,
                scanned,
                timer.elapsed().as_secs_f64(),
                timing_summary(&timings)
            ),
        )?;
    }

    let exe = env::current_exe()?;
    let child = Command::new(exe)
        .arg("watch-daemon")
        .arg(&cfg.root)
        .arg("--idle-seconds")
        .arg(watch_options.idle_seconds.to_string())
        .arg("--compact-delta-count")
        .arg(watch_options.compact_delta_count.to_string())
        .arg("--compact-delta-bytes")
        .arg(watch_options.compact_delta_bytes.to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    let record = WatchRecord {
        id,
        root: cfg.root.clone(),
        pid: child.id(),
    };
    write_watch_record(&record)?;
    append_watch_log(
        &cfg.root,
        &format!(
            "watch-start pid={} idle_seconds={} compact_delta_count={} compact_delta_bytes={}",
            record.pid,
            watch_options.idle_seconds,
            watch_options.compact_delta_count,
            watch_options.compact_delta_bytes
        ),
    )?;
    println!(
        "watching {} pid={} id={}",
        cfg.root.display(),
        record.pid,
        record.id
    );
    Ok(0)
}

fn command_watch_daemon(args: &[String]) -> Result<i32> {
    let (watch_options, start) = parse_watch_args(args)?;
    let cfg = load_config(&start)?;
    run_watch_daemon(&cfg, watch_options)
}

fn command_list_watches(_args: &[String]) -> Result<i32> {
    fs::create_dir_all(watch_registry_dir())?;
    let mut records = Vec::new();
    for entry in fs::read_dir(watch_registry_dir())?.flatten() {
        let path = entry.path();
        if path.extension().is_some_and(|ext| ext == "watch") {
            if let Ok(record) = read_watch_record(&path) {
                records.push((path, record));
            }
        }
    }
    records.sort_by(|a, b| a.1.root.cmp(&b.1.root));
    for (path, record) in records {
        let alive = process_alive(record.pid);
        println!(
            "{}\tpid={}\talive={}\t{}",
            record.id,
            record.pid,
            alive,
            record.root.display()
        );
        if !alive {
            let _ = fs::remove_file(path);
        }
    }
    Ok(0)
}

fn command_unwatch(args: &[String]) -> Result<i32> {
    let target = args.first().context("unwatch requires an id or path")?;
    let target_path = fs::canonicalize(target).ok();
    let mut matched = Vec::new();
    fs::create_dir_all(watch_registry_dir())?;
    for entry in fs::read_dir(watch_registry_dir())?.flatten() {
        let path = entry.path();
        if path.extension().is_some_and(|ext| ext == "watch") {
            if let Ok(record) = read_watch_record(&path) {
                let by_id = record.id == *target;
                let by_path = target_path.as_ref().is_some_and(|p| *p == record.root);
                if by_id || by_path {
                    matched.push((path, record));
                }
            }
        }
    }
    if matched.is_empty() {
        eprintln!("indexsearch: no watch matched {target}");
        return Ok(1);
    }
    for (path, record) in matched {
        stop_process(record.pid);
        let _ = fs::remove_file(path);
        let _ = append_watch_log(&record.root, &format!("watch-stop pid={}", record.pid));
        println!(
            "unwatched {} pid={} {}",
            record.id,
            record.pid,
            record.root.display()
        );
    }
    Ok(0)
}

fn command_watch_log(args: &[String]) -> Result<i32> {
    let start = args
        .first()
        .map(PathBuf::from)
        .unwrap_or(env::current_dir()?);
    let cfg = load_config(&start)?;
    let path = watch_log_path(&cfg.root);
    let lines = fs::read_to_string(&path).unwrap_or_default();
    if lines.is_empty() {
        println!("watch log is empty: {}", path.display());
    } else {
        print!("{lines}");
    }
    Ok(0)
}

fn command_install(args: &[String]) -> Result<i32> {
    let mut dir = default_install_dir();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--dir" => {
                i += 1;
                dir = PathBuf::from(args.get(i).context("missing --dir value")?);
            }
            value => dir = PathBuf::from(value),
        }
        i += 1;
    }
    fs::create_dir_all(&dir)?;
    let src = env::current_exe()?;
    let exe_name = executable_name("indexsearch");
    let exe_path = dir.join(exe_name);
    let alias_path = if cfg!(windows) {
        dir.join("is.cmd")
    } else {
        dir.join("is")
    };
    install_executable(&src, &exe_path)?;
    install_alias(&exe_path, &alias_path)?;
    println!("installed: {}", exe_path.display());
    println!("alias: {}", alias_path.display());
    if !path_contains(&dir) {
        println!(
            "note: add {} to PATH to use indexsearch and is from any shell",
            dir.display()
        );
    }
    Ok(0)
}

fn parse_watch_args(args: &[String]) -> Result<(WatchOptions, PathBuf)> {
    let mut options = WatchOptions::default();
    let mut start = env::current_dir()?;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--idle-seconds" => {
                i += 1;
                options.idle_seconds = args
                    .get(i)
                    .context("missing --idle-seconds value")?
                    .parse()?;
            }
            "--compact-delta-count" => {
                i += 1;
                options.compact_delta_count = args
                    .get(i)
                    .context("missing --compact-delta-count value")?
                    .parse()?;
            }
            "--compact-delta-bytes" => {
                i += 1;
                options.compact_delta_bytes =
                    parse_size(args.get(i).context("missing --compact-delta-bytes value")?)?;
            }
            value => start = PathBuf::from(value),
        }
        i += 1;
    }
    Ok((options, start))
}

fn run_watch_daemon(cfg: &ProjectConfig, watch_options: WatchOptions) -> Result<i32> {
    fs::create_dir_all(watch_registry_dir())?;
    append_watch_log(
        &cfg.root,
        &format!("watch-daemon-start pid={}", std::process::id()),
    )?;
    write_watch_record(&WatchRecord {
        id: watch_id(&cfg.root),
        root: cfg.root.clone(),
        pid: std::process::id(),
    })?;

    let (tx, rx) = mpsc::channel::<notify::Result<Event>>();
    let mut watcher = RecommendedWatcher::new(
        move |res| {
            let _ = tx.send(res);
        },
        NotifyConfig::default(),
    )?;
    watcher.watch(&cfg.root, RecursiveMode::Recursive)?;

    let mut pending = HashSet::new();
    let idle = Duration::from_secs(watch_options.idle_seconds.max(1));
    loop {
        match rx.recv_timeout(idle) {
            Ok(Ok(event)) => collect_event_paths(cfg, event, &mut pending),
            Ok(Err(_)) => {}
            Err(RecvTimeoutError::Timeout) => {
                if !pending.is_empty() {
                    flush_watch_batch(cfg, &pending)?;
                    pending.clear();
                }
                maybe_compact_idle(cfg, watch_options)?;
            }
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }
    append_watch_log(
        &cfg.root,
        &format!("watch-daemon-stop pid={}", std::process::id()),
    )?;
    Ok(0)
}

fn collect_event_paths(cfg: &ProjectConfig, event: Event, pending: &mut HashSet<String>) {
    for path in event.paths {
        if path.starts_with(cfg.root.join(INDEX_DIR)) || path.is_dir() {
            continue;
        }
        if let Some(rel) = rel_path(&cfg.root, &path) {
            pending.insert(rel);
        }
    }
}

fn flush_watch_batch(cfg: &ProjectConfig, paths: &HashSet<String>) -> Result<()> {
    let _lock = acquire_exclusive_lock(&cfg.root)?;
    let timer = Instant::now();
    let options = Options {
        max_filesize: DEFAULT_MAX_FILE_SIZE,
        ..Options::default()
    };
    let changes: Vec<ChangedPath> = paths
        .iter()
        .map(|rel| ChangedPath {
            rel: rel.clone(),
            deleted: !cfg.root.join(rel).exists(),
        })
        .collect();
    if changes.is_empty() {
        return Ok(());
    }
    if !index_path(&cfg.root).exists() {
        let mut timings = Timings::default();
        let mut scanned = 0;
        let mut skipped = 0;
        let index = build_index(
            cfg,
            &options,
            &mut scanned,
            &mut skipped,
            Some(&mut timings),
        )?;
        let write_timer = Instant::now();
        save_index(&index, &index_path(&cfg.root))?;
        save_index_state(&cfg.root)?;
        timings.write += write_timer.elapsed().as_secs_f64();
        append_watch_log(
            &cfg.root,
            &format!(
                "auto-index files={} skipped={} scanned={} events={} elapsed={:.3}s {}",
                index.files.len(),
                skipped,
                scanned,
                paths.len(),
                timer.elapsed().as_secs_f64(),
                timing_summary(&timings)
            ),
        )?;
        return Ok(());
    }
    let mut scanned = 0;
    let mut skipped = 0;
    let process_timer = Instant::now();
    let (delta, meta, stats) =
        build_delta_index(cfg, &options, &changes, &mut scanned, &mut skipped)?;
    let process_elapsed = process_timer.elapsed().as_secs_f64();
    if stats.added == 0 && stats.updated == 0 && stats.removed == 0 {
        append_watch_log(
            &cfg.root,
            &format!(
                "auto-update-noop events={} scanned={} skipped={} elapsed={:.3}s process={:.3}s",
                paths.len(),
                scanned,
                skipped,
                timer.elapsed().as_secs_f64(),
                process_elapsed
            ),
        )?;
        return Ok(());
    }
    let write_timer = Instant::now();
    save_delta(&cfg.root, &delta, &meta)?;
    save_index_state(&cfg.root)?;
    let write_elapsed = write_timer.elapsed().as_secs_f64();
    append_watch_log(
        &cfg.root,
        &format!(
            "auto-update files={} reused={} added={} modified={} removed={} skipped={} scanned={} events={} elapsed={:.3}s process={:.3}s write={:.3}s",
            stats.reused + stats.added + stats.updated,
            stats.reused,
            stats.added,
            stats.updated,
            stats.removed,
            skipped,
            scanned,
            paths.len(),
            timer.elapsed().as_secs_f64(),
            process_elapsed,
            write_elapsed
        ),
    )?;
    Ok(())
}

fn maybe_compact_idle(cfg: &ProjectConfig, watch_options: WatchOptions) -> Result<()> {
    let files = delta_files(&cfg.root)?;
    if files.is_empty() {
        return Ok(());
    }
    let total_bytes = files
        .iter()
        .filter_map(|path| fs::metadata(path).ok().map(|m| m.len()))
        .sum::<u64>();
    if files.len() < watch_options.compact_delta_count
        && total_bytes < watch_options.compact_delta_bytes
    {
        return Ok(());
    }
    let _lock = acquire_exclusive_lock(&cfg.root)?;
    compact_root(cfg)
}

fn compact_root(cfg: &ProjectConfig) -> Result<()> {
    let timer = Instant::now();
    let path = index_path(&cfg.root);
    let base = MappedIndex::open(&path)?;
    if base.config_hash != cfg.hash {
        return Ok(());
    }
    let deltas = load_deltas(&cfg.root)?;
    if deltas.is_empty() {
        return Ok(());
    }
    let delta_count = deltas.len();
    let process_timer = Instant::now();
    let compacted = compact_segments(
        cfg,
        &base,
        &deltas,
        &Options {
            max_filesize: DEFAULT_MAX_FILE_SIZE,
            ..Options::default()
        },
    )?;
    let process_elapsed = process_timer.elapsed().as_secs_f64();
    drop(deltas);
    drop(base);
    let write_timer = Instant::now();
    save_compacted_index(&compacted, &path)?;
    retire_delta_dir(&cfg.root)?;
    save_index_state(&cfg.root)?;
    let write_elapsed = write_timer.elapsed().as_secs_f64();
    append_watch_log(
        &cfg.root,
        &format!(
            "auto-compact files={} deltas={} elapsed={:.3}s process={:.3}s write={:.3}s",
            compacted.files.len(),
            delta_count,
            timer.elapsed().as_secs_f64(),
            process_elapsed,
            write_elapsed
        ),
    )?;
    Ok(())
}

fn command_compact(args: &[String]) -> Result<i32> {
    let (options, start) = parse_index_args(args)?;
    let cfg = load_config(&start)?;
    let _lock = acquire_exclusive_lock(&cfg.root)?;
    let timer = Instant::now();
    let mut timings = Timings::default();
    let path = index_path(&cfg.root);
    let base = MappedIndex::open(&path)?;
    if base.config_hash != cfg.hash {
        eprintln!("indexsearch: index not found or stale: {}", path.display());
        return Ok(2);
    }
    let deltas = load_deltas(&cfg.root)?;
    if deltas.is_empty() {
        println!(
            "compacted 0 delta indexes in {:.3}s",
            timer.elapsed().as_secs_f64()
        );
        print_timings(&timings);
        println!("root: {}", cfg.root.display());
        println!("index: {}", path.display());
        return Ok(0);
    }
    let process_timer = Instant::now();
    let compacted = compact_segments(&cfg, &base, &deltas, &options)?;
    timings.process += process_timer.elapsed().as_secs_f64();
    drop(deltas);
    drop(base);
    let write_timer = Instant::now();
    save_compacted_index(&compacted, &path)?;
    retire_delta_dir(&cfg.root)?;
    save_index_state(&cfg.root)?;
    timings.write += write_timer.elapsed().as_secs_f64();
    println!(
        "compacted {} files into base index in {:.3}s",
        compacted.files.len(),
        timer.elapsed().as_secs_f64()
    );
    print_timings(&timings);
    println!("root: {}", cfg.root.display());
    println!("index: {}", path.display());
    Ok(0)
}

fn command_status(args: &[String]) -> Result<i32> {
    let start = args
        .first()
        .map(PathBuf::from)
        .unwrap_or(std::env::current_dir()?);
    let cfg = load_config(&start)?;
    let _lock = acquire_shared_lock(&cfg.root)?;
    let path = index_path(&cfg.root);
    let loaded = MappedIndex::open(&path);
    println!("root: {}", cfg.root.display());
    println!(
        "config: {}",
        cfg.path
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_default()
    );
    println!("index: {}", path.display());
    match loaded {
        Ok(index) => {
            let deltas = load_deltas(&cfg.root).unwrap_or_default();
            let visible_files = current_visible_paths(&cfg.root)
                .map(|paths| paths.len())
                .unwrap_or(index.file_count);
            println!("exists: true");
            println!("files: {}", visible_files);
            println!("base_files: {}", index.file_count);
            println!("trigrams: {}", index.posting_count);
            println!("deltas: {}", deltas.len());
            println!("config_stale: {}", index.config_hash != cfg.hash);
            Ok(0)
        }
        Err(_) => {
            println!("exists: false");
            Ok(1)
        }
    }
}

fn command_search(args: &[String]) -> Result<i32> {
    let options = parse_search_args(args)?;
    let start = options
        .paths
        .first()
        .map(PathBuf::from)
        .unwrap_or(std::env::current_dir()?);
    let cfg = if options.auto_index {
        load_or_create_config(&start)?
    } else {
        load_config(&start)?
    };
    let _lock = acquire_shared_lock(&cfg.root)?;
    let path = index_path(&cfg.root);
    let mut index = MappedIndex::open(&path);
    if index
        .as_ref()
        .map(|i| i.config_hash != cfg.hash)
        .unwrap_or(true)
    {
        if !options.auto_index {
            eprintln!("indexsearch: index not found or stale: {}", path.display());
            return Ok(2);
        }
        drop(index);
        let mut scanned = 0;
        let mut skipped = 0;
        let built = build_index(&cfg, &options, &mut scanned, &mut skipped, None)?;
        save_index(&built, &path)?;
        remove_delta_dir(&cfg.root)?;
        save_index_state(&cfg.root)?;
        index = MappedIndex::open(&path);
    }
    let index = index?;
    let deltas = load_deltas(&cfg.root)?;
    if options.files {
        if !options.quiet {
            print_visible_files(&index, &deltas, &options)?;
        }
        return Ok(0);
    }
    let timer = Instant::now();
    let mut searched = 0;
    let results = execute_search_segments(&index, &deltas, &options, &mut searched)?;
    if !options.quiet {
        print_results(&results, &options);
    }
    if options.stats {
        let match_count: usize = results.iter().map(|r| r.matches.len()).sum();
        eprintln!("{match_count} matches");
        eprintln!("{} matched files", results.len());
        eprintln!("{searched} candidate files");
        eprintln!("{:.6} seconds", timer.elapsed().as_secs_f64());
    }
    Ok(if results.is_empty() { 1 } else { 0 })
}

fn parse_search_args(args: &[String]) -> Result<Options> {
    let mut options = Options {
        auto_index: true,
        max_filesize: DEFAULT_MAX_FILE_SIZE,
        ..Options::default()
    };
    let mut regexps = Vec::new();
    let mut i = 0;
    while i < args.len() {
        let arg = &args[i];
        let mut need_value = |name: &str| -> Result<String> {
            i += 1;
            args.get(i)
                .cloned()
                .ok_or_else(|| anyhow!("missing value for {name}"))
        };
        match arg.as_str() {
            "-h" | "--help" => {
                print_help();
                std::process::exit(0);
            }
            "-i" | "--ignore-case" => options.ignore_case = true,
            "-s" | "--case-sensitive" => options.ignore_case = false,
            "-S" | "--smart-case" => options.smart_case = true,
            "-F" | "--fixed-strings" => options.fixed = true,
            "-w" | "--word-regexp" => options.whole_word = true,
            "-e" | "--regexp" => regexps.push(need_value(arg)?),
            "-g" | "--glob" => options.globs.push(need_value(arg)?),
            "-n" | "--line-number" => options.line_number = true,
            "-N" | "--no-line-number" => options.line_number = false,
            "--column" => options.column = true,
            "-H" | "--with-filename" => options.with_filename = Some(true),
            "-I" | "--no-filename" => options.with_filename = Some(false),
            "-l" | "--files-with-matches" => options.files_with_matches = true,
            "-c" | "--count" => options.count = true,
            "-o" | "--only-matching" => options.only_matching = true,
            "-q" | "--quiet" => options.quiet = true,
            "--files" => options.files = true,
            "--json" => options.json = true,
            "--vimgrep" => options.vimgrep = true,
            "--stats" => options.stats = true,
            "--hidden" => options.hidden = true,
            "-L" | "--follow" => options.follow = true,
            "--no-auto-index" => options.auto_index = false,
            "--no-heading" | "--no-messages" | "--no-ignore" | "--no-ignore-vcs" => {}
            "--color" | "--colors" | "--sort" | "--sortr" | "-j" | "--threads" => {
                let _ = need_value(arg)?;
            }
            "-m" | "--max-count" => options.max_count = Some(need_value(arg)?.parse()?),
            "--max-filesize" => options.max_filesize = parse_size(&need_value(arg)?)?,
            _ if arg.starts_with("--color=")
                || arg.starts_with("--sort=")
                || arg.starts_with("--sortr=") => {}
            _ if arg.starts_with('-') => bail!("unsupported option: {arg}"),
            _ if options.pattern.is_empty() && regexps.is_empty() && !options.files => {
                options.pattern = arg.clone();
            }
            _ => options.paths.push(arg.clone()),
        }
        i += 1;
    }
    if !regexps.is_empty() {
        if !options.pattern.is_empty() {
            options.paths.insert(0, options.pattern.clone());
        }
        options.pattern = regexps
            .into_iter()
            .map(|p| format!("(?:{p})"))
            .collect::<Vec<_>>()
            .join("|");
    }
    if !options.files && options.pattern.is_empty() {
        bail!("a search pattern is required");
    }
    if options.smart_case && !has_uppercase(&options.pattern) {
        options.ignore_case = true;
    }
    if options.vimgrep {
        options.line_number = true;
        options.column = true;
    }
    Ok(options)
}

fn load_config(start: &Path) -> Result<ProjectConfig> {
    load_config_inner(start, false)
}

fn load_or_create_config(start: &Path) -> Result<ProjectConfig> {
    load_config_inner(start, true)
}

fn load_config_inner(start: &Path, create_default: bool) -> Result<ProjectConfig> {
    let root = discover_root(start)?;
    let path = root.join(PROJECT_FILE);
    let text = if path.exists() {
        fs::read_to_string(&path)?
    } else {
        if create_default {
            fs::create_dir_all(&root)?;
            fs::write(&path, DEFAULT_PROJECT_CONFIG)?;
            eprintln!("indexsearch: created default config: {}", path.display());
        }
        DEFAULT_PROJECT_CONFIG.to_string()
    };
    let has_config = path.exists();
    let sections = parse_sections(&text);
    let paths_ignore = MatcherSet::new(&clean_section(sections.get("IndexSearch.paths.ignore")))?;
    let files_ignore = MatcherSet::new(&clean_section(sections.get("IndexSearch.files.ignore")))?;
    let mut include_lines = clean_section(sections.get("IndexSearch.files.include"));
    if include_lines.is_empty() {
        include_lines.push("*".to_string());
    }
    let files_include = MatcherSet::new(&include_lines)?;
    Ok(ProjectConfig {
        root,
        path: has_config.then_some(path),
        paths_ignore,
        files_ignore,
        files_include,
        hash: fnv1a(text.as_bytes()),
    })
}

fn discover_root(start: &Path) -> Result<PathBuf> {
    let mut path = fs::canonicalize(start).unwrap_or_else(|_| start.to_path_buf());
    if path.is_file() {
        path = path.parent().unwrap_or(Path::new(".")).to_path_buf();
    }
    let fallback = path.clone();
    loop {
        if path.join(PROJECT_FILE).exists() {
            return Ok(path);
        }
        if !path.pop() {
            break;
        }
    }
    Ok(fallback)
}

fn parse_sections(text: &str) -> BTreeMap<String, Vec<String>> {
    let mut sections = BTreeMap::new();
    let mut current = String::new();
    for line in text.lines() {
        let trimmed = line.trim().trim_start_matches('\u{feff}');
        if trimmed.starts_with('[') && trimmed.ends_with(']') && trimmed.len() > 2 {
            current = trimmed[1..trimmed.len() - 1].trim().to_string();
            continue;
        }
        sections
            .entry(current.clone())
            .or_insert_with(Vec::new)
            .push(line.to_string());
    }
    sections
}

fn clean_section(lines: Option<&Vec<String>>) -> Vec<String> {
    lines
        .into_iter()
        .flatten()
        .map(|l| l.trim().trim_start_matches('\u{feff}').replace('\\', "/"))
        .filter(|l| !l.is_empty() && !l.starts_with('#') && !l.starts_with('!'))
        .collect()
}

fn add_glob_pattern(builder: &mut GlobSetBuilder, raw: &str) -> Result<()> {
    let mut pat = raw.replace('\\', "/");
    let directory_only = pat.ends_with('/');
    while pat.starts_with('/') {
        pat.remove(0);
    }
    while pat.ends_with('/') {
        pat.pop();
    }
    if pat.is_empty() {
        return Ok(());
    }
    let has_slash = pat.contains('/');
    let mut variants = Vec::new();
    if has_slash {
        variants.push(pat.clone());
        if !pat.starts_with("**/") {
            variants.push(format!("**/{pat}"));
        }
        if directory_only {
            variants.push(format!("{pat}/**"));
            if !pat.starts_with("**/") {
                variants.push(format!("**/{pat}/**"));
            }
        }
    } else {
        variants.push(pat.clone());
        variants.push(format!("**/{pat}"));
        if directory_only {
            variants.push(format!("{pat}/**"));
            variants.push(format!("**/{pat}/**"));
        }
    }
    for variant in variants {
        builder.add(Glob::new(&variant)?);
    }
    Ok(())
}

fn build_index(
    cfg: &ProjectConfig,
    options: &Options,
    scanned: &mut u64,
    skipped: &mut u64,
    mut timings: Option<&mut Timings>,
) -> Result<BuiltIndex> {
    let scan_timer = Instant::now();
    let entries = scan_indexable_files(cfg, options, scanned, skipped)?;
    if let Some(timings) = timings.as_deref_mut() {
        timings.scan += scan_timer.elapsed().as_secs_f64();
    }
    let process_timer = Instant::now();
    let skipped_reads = AtomicU64::new(0);
    let mut files: Vec<(usize, FileEntry, Vec<u32>)> = entries
        .par_iter()
        .filter_map(|entry| {
            let bytes = fs::read(&entry.path).ok()?;
            if is_binary(&bytes) {
                skipped_reads.fetch_add(1, AtomicOrdering::Relaxed);
                return None;
            }
            let grams = file_trigrams(&bytes);
            Some((
                entry.ordinal,
                FileEntry {
                    path: entry.rel.clone(),
                    mtime: entry.mtime,
                    size: bytes.len() as u64,
                    content: bytes,
                },
                grams,
            ))
        })
        .collect();
    files.sort_by_key(|(idx, _, _)| *idx);

    let mut postings: BTreeMap<u32, Vec<u32>> = BTreeMap::new();
    let mut out_files = Vec::with_capacity(files.len());
    for (_, file, grams) in files {
        let id = out_files.len() as u32;
        for gram in grams {
            postings.entry(gram).or_default().push(id);
        }
        out_files.push(file);
    }
    *skipped += skipped_reads.load(AtomicOrdering::Relaxed);
    if let Some(timings) = timings {
        timings.process += process_timer.elapsed().as_secs_f64();
    }
    Ok(BuiltIndex {
        root: cfg.root.clone(),
        config_hash: cfg.hash,
        files: out_files,
        postings,
    })
}

fn update_index(
    cfg: &ProjectConfig,
    options: &Options,
    old: &MappedIndex,
    scanned: &mut u64,
    skipped: &mut u64,
    mut timings: Option<&mut Timings>,
) -> Result<(BuiltIndex, UpdateStats)> {
    let scan_timer = Instant::now();
    let entries = scan_indexable_files(cfg, options, scanned, skipped)?;
    if let Some(timings) = timings.as_deref_mut() {
        timings.scan += scan_timer.elapsed().as_secs_f64();
    }
    let process_timer = Instant::now();
    let mut old_files = HashMap::with_capacity(old.file_count);
    for id in 0..old.file_count {
        let file = old.file(id)?;
        let rec = old.file_record(id)?;
        old_files.insert(bytes_to_string(file.path), (id, rec.mtime, rec.size));
    }

    let skipped_reads = AtomicU64::new(0);
    let mut stats = UpdateStats::default();
    let mut seen_old = HashSet::with_capacity(entries.len());
    let mut jobs = Vec::with_capacity(entries.len());
    for entry in entries {
        if let Some(&(id, old_mtime, old_size)) = old_files.get(&entry.rel) {
            seen_old.insert(entry.rel.clone());
            if old_mtime == entry.mtime && old_size == entry.size {
                jobs.push((entry, Some(id), ChangeKind::Reused));
                continue;
            }
            jobs.push((entry, None, ChangeKind::Updated));
        } else {
            jobs.push((entry, None, ChangeKind::Added));
        }
    }
    stats.removed = old_files.len().saturating_sub(seen_old.len()) as u64;

    let mut files: Vec<(usize, ChangeKind, FileEntry, Vec<u32>)> = jobs
        .par_iter()
        .filter_map(|(entry, old_id, kind)| {
            let bytes = if let Some(old_id) = old_id {
                old.file(*old_id).ok()?.content.to_vec()
            } else {
                let bytes = fs::read(&entry.path).ok()?;
                if is_binary(&bytes) {
                    skipped_reads.fetch_add(1, AtomicOrdering::Relaxed);
                    return None;
                }
                bytes
            };
            let grams = file_trigrams(&bytes);
            Some((
                entry.ordinal,
                *kind,
                FileEntry {
                    path: entry.rel.clone(),
                    mtime: entry.mtime,
                    size: bytes.len() as u64,
                    content: bytes,
                },
                grams,
            ))
        })
        .collect();
    files.sort_by_key(|(idx, _, _, _)| *idx);

    let mut postings: BTreeMap<u32, Vec<u32>> = BTreeMap::new();
    let mut out_files = Vec::with_capacity(files.len());
    for (_, kind, file, grams) in files {
        match kind {
            ChangeKind::Reused => stats.reused += 1,
            ChangeKind::Added => stats.added += 1,
            ChangeKind::Updated => stats.updated += 1,
        }
        let id = out_files.len() as u32;
        for gram in grams {
            postings.entry(gram).or_default().push(id);
        }
        out_files.push(file);
    }
    *skipped += skipped_reads.load(AtomicOrdering::Relaxed);
    if let Some(timings) = timings {
        timings.process += process_timer.elapsed().as_secs_f64();
    }
    Ok((
        BuiltIndex {
            root: cfg.root.clone(),
            config_hash: cfg.hash,
            files: out_files,
            postings,
        },
        stats,
    ))
}

fn read_current_file_entry(
    cfg: &ProjectConfig,
    options: &Options,
    rel: &str,
) -> Result<Option<(FileEntry, Vec<u32>)>> {
    let rel = rel.replace('\\', "/");
    if (!options.hidden && is_hidden(&rel)) || !is_searchable(cfg, &rel) {
        return Ok(None);
    }
    let path = cfg.root.join(&rel);
    let Ok(meta) = fs::metadata(&path) else {
        return Ok(None);
    };
    if !meta.is_file() || meta.len() > options.max_filesize {
        return Ok(None);
    }
    let bytes = fs::read(&path)?;
    if is_binary(&bytes) {
        return Ok(None);
    }
    let grams = file_trigrams(&bytes);
    Ok(Some((
        FileEntry {
            path: rel,
            mtime: mtime_ns(&meta),
            size: bytes.len() as u64,
            content: bytes,
        },
        grams,
    )))
}

fn build_delta_index(
    cfg: &ProjectConfig,
    options: &Options,
    changes: &[ChangedPath],
    scanned: &mut u64,
    skipped: &mut u64,
) -> Result<(BuiltIndex, DeltaMeta, UpdateStats)> {
    *scanned = changes.len() as u64;
    let existing = current_visible_paths(&cfg.root)?;
    let mut files = Vec::new();
    let mut postings: BTreeMap<u32, Vec<u32>> = BTreeMap::new();
    let mut meta = DeltaMeta::default();
    let mut stats = UpdateStats::default();

    for change in changes {
        if change.deleted {
            if existing.contains(&change.rel) {
                stats.removed += 1;
            }
            meta.tombstones.insert(change.rel.clone());
            continue;
        }

        match read_current_file_entry(cfg, options, &change.rel)? {
            Some((entry, grams)) => {
                if existing.contains(&change.rel) {
                    stats.updated += 1;
                    meta.tombstones.insert(change.rel.clone());
                } else {
                    stats.added += 1;
                }
                let id = files.len() as u32;
                for gram in grams {
                    postings.entry(gram).or_default().push(id);
                }
                files.push(entry);
            }
            None => {
                *skipped += 1;
                if existing.contains(&change.rel) {
                    stats.removed += 1;
                }
                meta.tombstones.insert(change.rel.clone());
            }
        }
    }
    stats.reused = existing
        .len()
        .saturating_sub(stats.updated as usize)
        .saturating_sub(stats.removed as usize) as u64;

    Ok((
        BuiltIndex {
            root: cfg.root.clone(),
            config_hash: cfg.hash,
            files,
            postings,
        },
        meta,
        stats,
    ))
}

fn current_visible_paths(root: &Path) -> Result<HashSet<String>> {
    let base = MappedIndex::open(&index_path(root))?;
    let deltas = load_deltas(root)?;
    let mut paths = HashSet::new();
    for id in 0..base.file_count {
        paths.insert(bytes_to_string(base.file(id)?.path));
    }
    for delta in &deltas {
        for tombstone in &delta.meta.tombstones {
            paths.remove(tombstone);
        }
        for id in 0..delta.index.file_count {
            paths.insert(bytes_to_string(delta.index.file(id)?.path));
        }
    }
    Ok(paths)
}

fn compact_segments(
    cfg: &ProjectConfig,
    base: &MappedIndex,
    deltas: &[DeltaSegment],
    _options: &Options,
) -> Result<BuiltIndex> {
    let exclusions = segment_exclusions(base, deltas)?;
    let mut files = Vec::new();
    append_visible_files(base, &exclusions[0], &mut files)?;
    for (idx, delta) in deltas.iter().enumerate() {
        append_visible_files(&delta.index, &exclusions[idx + 1], &mut files)?;
    }
    let mut postings: BTreeMap<u32, Vec<u32>> = BTreeMap::new();
    for (id, file) in files.iter().enumerate() {
        for gram in file_trigrams(&file.content) {
            postings.entry(gram).or_default().push(id as u32);
        }
    }
    Ok(BuiltIndex {
        root: cfg.root.clone(),
        config_hash: cfg.hash,
        files,
        postings,
    })
}

fn append_visible_files(
    index: &MappedIndex,
    excluded_paths: &HashSet<String>,
    out: &mut Vec<FileEntry>,
) -> Result<()> {
    for id in 0..index.file_count {
        let file = index.file(id)?;
        let path = bytes_to_string(file.path);
        if excluded_paths.contains(&path) {
            continue;
        }
        let rec = index.file_record(id)?;
        out.push(FileEntry {
            path,
            mtime: rec.mtime,
            size: rec.size,
            content: file.content.to_vec(),
        });
    }
    Ok(())
}

fn print_visible_files(
    base: &MappedIndex,
    deltas: &[DeltaSegment],
    options: &Options,
) -> Result<()> {
    let exclusions = segment_exclusions(base, deltas)?;
    print_segment_files(base, &exclusions[0], options)?;
    for (idx, delta) in deltas.iter().enumerate() {
        print_segment_files(&delta.index, &exclusions[idx + 1], options)?;
    }
    Ok(())
}

fn print_segment_files(
    index: &MappedIndex,
    excluded_paths: &HashSet<String>,
    options: &Options,
) -> Result<()> {
    for id in 0..index.file_count {
        let file = index.file(id)?;
        let path = bytes_to_string(file.path);
        if excluded_paths.contains(&path) {
            continue;
        }
        if path_allowed(options, &index.root, &path)? {
            println!("{path}");
        }
    }
    Ok(())
}

fn segment_exclusions(base: &MappedIndex, deltas: &[DeltaSegment]) -> Result<Vec<HashSet<String>>> {
    let mut exclusions = vec![HashSet::new(); deltas.len() + 1];
    let mut shadowed = HashSet::new();
    for idx in (0..deltas.len()).rev() {
        exclusions[idx + 1] = shadowed.clone();
        add_segment_overlay(&deltas[idx], &mut shadowed)?;
    }
    exclusions[0] = shadowed;
    let _ = base;
    Ok(exclusions)
}

fn add_segment_overlay(delta: &DeltaSegment, shadowed: &mut HashSet<String>) -> Result<()> {
    for tombstone in &delta.meta.tombstones {
        shadowed.insert(tombstone.clone());
    }
    for id in 0..delta.index.file_count {
        shadowed.insert(bytes_to_string(delta.index.file(id)?.path));
    }
    Ok(())
}

fn collect_git_changes(root: &Path, include_untracked: bool) -> Result<Option<Vec<ChangedPath>>> {
    if !is_git_root(root)? {
        return Ok(None);
    }

    let current_head = current_git_head(root)?;
    let state = read_index_state(root)?;
    let Some(current_head) = current_head else {
        return Ok(None);
    };

    let mut changes = BTreeMap::new();
    match state.git_head {
        Some(previous_head) if previous_head != current_head => {
            let output = Command::new("git")
                .arg("-C")
                .arg(root)
                .args(["diff", "--name-status", "-z", &previous_head, &current_head])
                .output()?;
            if !output.status.success() {
                return Ok(None);
            }
            parse_git_name_status(&output.stdout, &mut changes);
        }
        Some(_) => {}
        None => return Ok(None),
    }

    let untracked = if include_untracked {
        "--untracked-files=all"
    } else {
        "--untracked-files=no"
    };
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["status", "--porcelain=v1", "-z", untracked])
        .output()?;
    if !output.status.success() {
        return Ok(None);
    }

    parse_git_status(&output.stdout, &mut changes);
    Ok(Some(
        changes
            .into_iter()
            .map(|(rel, deleted)| ChangedPath { rel, deleted })
            .collect(),
    ))
}

fn is_git_root(root: &Path) -> Result<bool> {
    let top = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["rev-parse", "--show-toplevel"])
        .output();
    let Ok(top) = top else {
        return Ok(false);
    };
    if !top.status.success() {
        return Ok(false);
    }
    let git_root = PathBuf::from(String::from_utf8_lossy(&top.stdout).trim().to_string());
    let canonical_git_root = fs::canonicalize(&git_root).unwrap_or(git_root);
    let canonical_root = fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    Ok(canonical_git_root == canonical_root)
}

fn parse_git_status(data: &[u8], changes: &mut BTreeMap<String, bool>) {
    let mut parts = data
        .split(|byte| *byte == 0)
        .filter(|part| !part.is_empty());
    while let Some(part) = parts.next() {
        if part.len() < 4 {
            continue;
        }
        let x = part[0];
        let y = part[1];
        let rel = normalize_git_path(&part[3..]);
        let renamed = x == b'R' || y == b'R' || x == b'C' || y == b'C';
        if renamed {
            if let Some(old_path) = parts.next() {
                let old_rel = normalize_git_path(old_path);
                mark_change(changes, old_rel, true);
            }
            mark_change(changes, rel, false);
            continue;
        }

        let deleted = x == b'D' || y == b'D';
        mark_change(changes, rel, deleted);
    }
}

fn parse_git_name_status(data: &[u8], changes: &mut BTreeMap<String, bool>) {
    let mut parts = data
        .split(|byte| *byte == 0)
        .filter(|part| !part.is_empty());
    while let Some(status) = parts.next() {
        let code = status.first().copied().unwrap_or_default();
        if matches!(code, b'R' | b'C') {
            let Some(old_path) = parts.next() else {
                break;
            };
            let Some(new_path) = parts.next() else {
                break;
            };
            mark_change(changes, normalize_git_path(old_path), true);
            mark_change(changes, normalize_git_path(new_path), false);
            continue;
        }
        let Some(path) = parts.next() else {
            break;
        };
        mark_change(changes, normalize_git_path(path), code == b'D');
    }
}

fn mark_change(changes: &mut BTreeMap<String, bool>, rel: String, deleted: bool) {
    if deleted {
        changes.entry(rel).or_insert(true);
    } else {
        changes.insert(rel, false);
    }
}

fn normalize_git_path(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).replace('\\', "/")
}

fn current_git_head(root: &Path) -> Result<Option<String>> {
    if !is_git_root(root)? {
        return Ok(None);
    }
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["rev-parse", "--verify", "HEAD"])
        .output()?;
    if !output.status.success() {
        return Ok(None);
    }
    Ok(Some(
        String::from_utf8_lossy(&output.stdout).trim().to_string(),
    ))
}

fn read_index_state(root: &Path) -> Result<IndexState> {
    let path = state_path(root);
    let Ok(text) = fs::read_to_string(path) else {
        return Ok(IndexState::default());
    };
    let mut state = IndexState::default();
    for line in text.lines() {
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        if key == "git_head" && !value.is_empty() {
            state.git_head = Some(value.to_string());
        }
    }
    Ok(state)
}

fn save_index_state(root: &Path) -> Result<()> {
    fs::create_dir_all(root.join(INDEX_DIR))?;
    let mut text = String::from("version=1\n");
    if let Some(head) = current_git_head(root)? {
        text.push_str("git_head=");
        text.push_str(&head);
        text.push('\n');
    }
    fs::write(state_path(root), text)?;
    Ok(())
}

fn save_delta(root: &Path, index: &BuiltIndex, meta: &DeltaMeta) -> Result<()> {
    let dir = delta_dir(root);
    fs::create_dir_all(&dir)?;
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let stem = format!("delta-{stamp}-{}", std::process::id());
    let bin_path = dir.join(format!("{stem}.bin"));
    let meta_path = dir.join(format!("{stem}.meta"));
    save_index(index, &bin_path)?;
    save_delta_meta(meta, &meta_path)?;
    Ok(())
}

fn save_delta_meta(meta: &DeltaMeta, path: &Path) -> Result<()> {
    let mut text = String::from("version=1\n");
    for tombstone in &meta.tombstones {
        text.push_str("D\t");
        text.push_str(tombstone);
        text.push('\n');
    }
    fs::write(path, text)?;
    Ok(())
}

fn load_deltas(root: &Path) -> Result<Vec<DeltaSegment>> {
    let dir = delta_dir(root);
    let Ok(entries) = fs::read_dir(&dir) else {
        return Ok(Vec::new());
    };
    let mut bins = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().is_some_and(|ext| ext == "bin") {
            bins.push(path);
        }
    }
    bins.sort();
    let mut deltas = Vec::new();
    for bin in bins {
        let meta = load_delta_meta(&bin.with_extension("meta"))?;
        deltas.push(DeltaSegment {
            index: MappedIndex::open(&bin)?,
            meta,
        });
    }
    Ok(deltas)
}

fn delta_files(root: &Path) -> Result<Vec<PathBuf>> {
    let dir = delta_dir(root);
    let Ok(entries) = fs::read_dir(&dir) else {
        return Ok(Vec::new());
    };
    let mut files = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().is_some_and(|ext| ext == "bin") {
            files.push(path);
        }
    }
    files.sort();
    Ok(files)
}

fn load_delta_meta(path: &Path) -> Result<DeltaMeta> {
    let Ok(text) = fs::read_to_string(path) else {
        return Ok(DeltaMeta::default());
    };
    let mut meta = DeltaMeta::default();
    for line in text.lines() {
        if let Some(path) = line.strip_prefix("D\t") {
            meta.tombstones.insert(path.to_string());
        }
    }
    Ok(meta)
}

fn remove_delta_dir(root: &Path) -> Result<()> {
    let dir = delta_dir(root);
    if dir.exists() {
        fs::remove_dir_all(dir)?;
    }
    Ok(())
}

fn retire_delta_dir(root: &Path) -> Result<()> {
    let dir = delta_dir(root);
    if !dir.exists() {
        return Ok(());
    }
    let retired = root.join(INDEX_DIR).join(format!(
        "deltas.old.{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    fs::rename(&dir, &retired)?;
    let _ = fs::remove_dir_all(retired);
    Ok(())
}

fn scan_indexable_files(
    cfg: &ProjectConfig,
    options: &Options,
    scanned: &mut u64,
    skipped: &mut u64,
) -> Result<Vec<CurrentFile>> {
    let entries: Vec<PathBuf> = WalkDir::new(&cfg.root)
        .follow_links(options.follow)
        .into_iter()
        .filter_entry(|entry| should_descend(cfg, options, entry))
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.file_type().is_file())
        .map(|entry| entry.into_path())
        .collect();

    *scanned = entries.len() as u64;
    let mut out = Vec::with_capacity(entries.len());
    for (ordinal, path) in entries.into_iter().enumerate() {
        let Some(rel) = rel_path(&cfg.root, &path) else {
            *skipped += 1;
            continue;
        };
        if (!options.hidden && is_hidden(&rel)) || !is_searchable(cfg, &rel) {
            *skipped += 1;
            continue;
        }
        let Ok(meta) = fs::metadata(&path) else {
            *skipped += 1;
            continue;
        };
        if meta.len() > options.max_filesize {
            *skipped += 1;
            continue;
        }
        out.push(CurrentFile {
            ordinal,
            path,
            rel,
            mtime: mtime_ns(&meta),
            size: meta.len(),
        });
    }
    Ok(out)
}

fn should_descend(cfg: &ProjectConfig, options: &Options, entry: &DirEntry) -> bool {
    let Ok(rel) = entry.path().strip_prefix(&cfg.root) else {
        return true;
    };
    let rel = rel.to_string_lossy().replace('\\', "/");
    if rel.is_empty() {
        return true;
    }
    if !options.hidden && is_hidden(&rel) {
        return false;
    }
    if entry.file_type().is_dir() && cfg.paths_ignore.is_match(&rel) {
        return false;
    }
    true
}

fn is_searchable(cfg: &ProjectConfig, rel: &str) -> bool {
    !cfg.paths_ignore.is_match(rel)
        && !cfg.files_ignore.is_match(rel)
        && cfg.files_include.is_match(rel)
}

fn is_hidden(rel: &str) -> bool {
    rel.split('/')
        .any(|part| part.starts_with('.') && part != ".github")
}

fn rel_path(root: &Path, path: &Path) -> Option<String> {
    path.strip_prefix(root)
        .ok()
        .map(|p| p.to_string_lossy().replace('\\', "/"))
}

fn mtime_ns(meta: &fs::Metadata) -> i64 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_nanos() as i64)
        .unwrap_or(0)
}

fn is_binary(bytes: &[u8]) -> bool {
    bytes
        .get(..bytes.len().min(65536))
        .is_some_and(|prefix| prefix.contains(&0))
}

fn save_index(index: &BuiltIndex, path: &Path) -> Result<()> {
    fs::create_dir_all(path.parent().context("index path has no parent")?)?;
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(INDEX_FILE);
    let tmp_path = path.with_file_name(format!("{file_name}.tmp.{}", std::process::id()));
    let root = index.root.to_string_lossy().as_bytes().to_vec();
    let mut path_blob = Vec::new();
    let mut content_blob = Vec::new();
    let mut file_records = Vec::with_capacity(index.files.len());
    for file in &index.files {
        let path_offset = path_blob.len() as u64;
        path_blob.extend_from_slice(file.path.as_bytes());
        let content_offset = content_blob.len() as u64;
        content_blob.extend_from_slice(&file.content);
        file_records.push(FileRecord {
            path_offset,
            path_size: file.path.len() as u64,
            content_offset,
            content_size: file.content.len() as u64,
            mtime: file.mtime,
            size: file.size,
        });
    }
    let mut posting_records = Vec::with_capacity(index.postings.len());
    let mut posting_data = Vec::new();
    for (&gram, ids) in &index.postings {
        posting_records.push(PostingRecord {
            gram,
            offset: posting_data.len() as u64,
            count: ids.len() as u64,
        });
        posting_data.extend_from_slice(ids);
    }
    let mut cursor = 96_u64;
    let root_offset = cursor;
    cursor += root.len() as u64;
    cursor = align_to(cursor, 8);
    let file_table_offset = cursor;
    cursor += file_records.len() as u64 * 48;
    cursor = align_to(cursor, 8);
    let posting_table_offset = cursor;
    cursor += posting_records.len() as u64 * 24;
    cursor = align_to(cursor, 4);
    let postings_data_offset = cursor;
    cursor += posting_data.len() as u64 * 4;
    let path_blob_offset = cursor;
    cursor += path_blob.len() as u64;
    let content_blob_offset = cursor;

    let mut writer = BufWriter::new(File::create(&tmp_path)?);
    writer.write_all(MAGIC)?;
    write_u32(&mut writer, VERSION)?;
    write_u32(&mut writer, 0)?;
    write_u64(&mut writer, index.config_hash)?;
    write_u64(&mut writer, file_records.len() as u64)?;
    write_u64(&mut writer, posting_records.len() as u64)?;
    write_u64(&mut writer, root_offset)?;
    write_u64(&mut writer, root.len() as u64)?;
    write_u64(&mut writer, file_table_offset)?;
    write_u64(&mut writer, posting_table_offset)?;
    write_u64(&mut writer, postings_data_offset)?;
    write_u64(&mut writer, path_blob_offset)?;
    write_u64(&mut writer, content_blob_offset)?;
    writer.write_all(&root)?;
    write_padding(
        &mut writer,
        root_offset + root.len() as u64,
        file_table_offset,
    )?;
    for rec in &file_records {
        write_u64(&mut writer, rec.path_offset)?;
        write_u64(&mut writer, rec.path_size)?;
        write_u64(&mut writer, rec.content_offset)?;
        write_u64(&mut writer, rec.content_size)?;
        write_i64(&mut writer, rec.mtime)?;
        write_u64(&mut writer, rec.size)?;
    }
    write_padding(
        &mut writer,
        file_table_offset + file_records.len() as u64 * 48,
        posting_table_offset,
    )?;
    for rec in &posting_records {
        write_u32(&mut writer, rec.gram)?;
        write_u32(&mut writer, 0)?;
        write_u64(&mut writer, rec.offset)?;
        write_u64(&mut writer, rec.count)?;
    }
    write_padding(
        &mut writer,
        posting_table_offset + posting_records.len() as u64 * 24,
        postings_data_offset,
    )?;
    for id in &posting_data {
        write_u32(&mut writer, *id)?;
    }
    writer.write_all(&path_blob)?;
    writer.write_all(&content_blob)?;
    writer.flush()?;
    drop(writer);
    if let Err(err) = fs::rename(&tmp_path, path) {
        let _ = fs::remove_file(path);
        fs::rename(&tmp_path, path).with_context(|| {
            format!(
                "failed to replace {} after initial rename error: {err}",
                path.display()
            )
        })?;
    }
    Ok(())
}

fn save_compacted_index(index: &BuiltIndex, path: &Path) -> Result<()> {
    let parent = path.parent().context("index path has no parent")?;
    fs::create_dir_all(parent)?;
    let compact_path = path.with_file_name(format!(
        "{}.compact.{}",
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or(INDEX_FILE),
        std::process::id()
    ));
    save_index(index, &compact_path)?;
    let backup_path = path.with_file_name(format!(
        "{}.old.{}",
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or(INDEX_FILE),
        std::process::id()
    ));
    if path.exists() {
        fs::rename(path, &backup_path)?;
    }
    if let Err(err) = fs::rename(&compact_path, path) {
        if backup_path.exists() {
            let _ = fs::rename(&backup_path, path);
        }
        return Err(err).context("failed to publish compacted index");
    }
    if backup_path.exists() {
        let _ = fs::remove_file(backup_path);
    }
    Ok(())
}

impl MappedIndex {
    fn open(path: &Path) -> Result<Self> {
        let file = File::open(path)?;
        let mmap = unsafe { Mmap::map(&file)? };
        let header = parse_header(&mmap)?;
        let root_bytes = checked_slice(&mmap, header.root_offset, header.root_size)?;
        let root = PathBuf::from(String::from_utf8_lossy(root_bytes).to_string());
        Ok(Self {
            mmap,
            root,
            config_hash: header.config_hash,
            file_count: header.file_count as usize,
            posting_count: header.posting_count as usize,
            file_table_offset: header.file_table_offset as usize,
            posting_table_offset: header.posting_table_offset as usize,
            postings_data_offset: header.postings_data_offset as usize,
            path_blob_offset: header.path_blob_offset as usize,
            content_blob_offset: header.content_blob_offset as usize,
        })
    }

    fn file(&self, id: usize) -> Result<FileView<'_>> {
        if id >= self.file_count {
            bail!("file id out of bounds");
        }
        let rec = self.file_record(id)?;
        Ok(FileView {
            path: checked_slice(
                &self.mmap,
                self.path_blob_offset as u64 + rec.path_offset,
                rec.path_size,
            )?,
            content: checked_slice(
                &self.mmap,
                self.content_blob_offset as u64 + rec.content_offset,
                rec.content_size,
            )?,
        })
    }

    fn file_record(&self, id: usize) -> Result<FileRecord> {
        let off = self.file_table_offset + id * 48;
        Ok(FileRecord {
            path_offset: read_u64_at(&self.mmap, off)?,
            path_size: read_u64_at(&self.mmap, off + 8)?,
            content_offset: read_u64_at(&self.mmap, off + 16)?,
            content_size: read_u64_at(&self.mmap, off + 24)?,
            mtime: read_i64_at(&self.mmap, off + 32)?,
            size: read_u64_at(&self.mmap, off + 40)?,
        })
    }

    fn posting(&self, gram: u32) -> Result<Option<PostingView<'_>>> {
        let mut lo = 0usize;
        let mut hi = self.posting_count;
        while lo < hi {
            let mid = (lo + hi) / 2;
            let rec = self.posting_record(mid)?;
            match rec.gram.cmp(&gram) {
                Ordering::Less => lo = mid + 1,
                Ordering::Greater => hi = mid,
                Ordering::Equal => {
                    let start = self.postings_data_offset as u64 + rec.offset * 4;
                    let bytes = checked_slice(&self.mmap, start, rec.count * 4)?;
                    let data = unsafe {
                        std::slice::from_raw_parts(bytes.as_ptr() as *const u32, rec.count as usize)
                    };
                    return Ok(Some(PostingView { data }));
                }
            }
        }
        Ok(None)
    }

    fn posting_record(&self, id: usize) -> Result<PostingRecord> {
        let off = self.posting_table_offset + id * 24;
        Ok(PostingRecord {
            gram: read_u32_at(&self.mmap, off)?,
            offset: read_u64_at(&self.mmap, off + 8)?,
            count: read_u64_at(&self.mmap, off + 16)?,
        })
    }
}

fn parse_header(data: &[u8]) -> Result<Header> {
    if data.len() < 96 || &data[..8] != MAGIC {
        bail!("invalid index header");
    }
    let version = read_u32_at(data, 8)?;
    if version != VERSION {
        bail!("unsupported index version {version}");
    }
    Ok(Header {
        config_hash: read_u64_at(data, 16)?,
        file_count: read_u64_at(data, 24)?,
        posting_count: read_u64_at(data, 32)?,
        root_offset: read_u64_at(data, 40)?,
        root_size: read_u64_at(data, 48)?,
        file_table_offset: read_u64_at(data, 56)?,
        posting_table_offset: read_u64_at(data, 64)?,
        postings_data_offset: read_u64_at(data, 72)?,
        path_blob_offset: read_u64_at(data, 80)?,
        content_blob_offset: read_u64_at(data, 88)?,
    })
}

fn execute_search_segments(
    base: &MappedIndex,
    deltas: &[DeltaSegment],
    options: &Options,
    searched: &mut u64,
) -> Result<Vec<FileResult>> {
    let exclusions = segment_exclusions(base, deltas)?;
    let mut results = execute_search(base, options, searched, &exclusions[0])?;
    for (idx, delta) in deltas.iter().enumerate() {
        let mut segment_results =
            execute_search(&delta.index, options, searched, &exclusions[idx + 1])?;
        results.append(&mut segment_results);
    }
    Ok(results)
}

fn execute_search(
    index: &MappedIndex,
    options: &Options,
    searched: &mut u64,
    excluded_paths: &HashSet<String>,
) -> Result<Vec<FileResult>> {
    let grams = query_trigrams(options);
    let candidates = intersect_postings(index, &grams)?;
    let path_matcher = build_path_matcher(options)?;
    let filtered: Vec<u32> = candidates
        .into_iter()
        .filter_map(|id| {
            let file = index.file(id as usize).ok()?;
            let path = bytes_to_string(file.path);
            if excluded_paths.contains(&path) {
                return None;
            }
            path_allowed_with_matcher(options, &index.root, &path, path_matcher.as_ref())
                .ok()
                .filter(|ok| *ok)
                .map(|_| id)
        })
        .collect();
    *searched += filtered.len() as u64;

    let matcher = QueryMatcher::new(options)?;
    let mut results: Vec<(usize, FileResult)> = filtered
        .par_iter()
        .enumerate()
        .filter_map(|(ordinal, id)| {
            let file = index.file(*id as usize).ok()?;
            let matches = matcher.search_file(file.content, options);
            if matches.is_empty() {
                return None;
            }
            Some((
                ordinal,
                FileResult {
                    path: bytes_to_string(file.path),
                    matches,
                },
            ))
        })
        .collect();
    results.sort_by_key(|(ordinal, _)| *ordinal);
    Ok(results.into_iter().map(|(_, result)| result).collect())
}

enum QueryMatcher {
    Fixed {
        needle: Vec<u8>,
        finder: memmem::Finder<'static>,
        ac: Option<AhoCorasick>,
        whole_word: bool,
        ignore_case: bool,
    },
    OrderedLiterals {
        literals: Vec<Vec<u8>>,
        ignore_case: bool,
    },
    Regex(Regex),
}

impl QueryMatcher {
    fn new(options: &Options) -> Result<Self> {
        if options.fixed || regex_meta_free(&options.pattern) {
            let needle = if options.ignore_case {
                lower_bytes(options.pattern.as_bytes())
            } else {
                options.pattern.as_bytes().to_vec()
            };
            let finder = memmem::Finder::new(&needle).into_owned();
            let ac = options.ignore_case.then(|| {
                AhoCorasickBuilder::new()
                    .ascii_case_insensitive(true)
                    .build([options.pattern.as_bytes()])
                    .expect("literal automaton")
            });
            return Ok(Self::Fixed {
                needle,
                finder,
                ac,
                whole_word: options.whole_word,
                ignore_case: options.ignore_case,
            });
        }
        if let Some(literals) = ordered_dotstar_literals(options) {
            return Ok(Self::OrderedLiterals {
                literals,
                ignore_case: options.ignore_case,
            });
        }
        let pattern = if options.whole_word {
            format!(r"\b(?:{})\b", options.pattern)
        } else {
            options.pattern.clone()
        };
        let regex = RegexBuilder::new(&pattern)
            .case_insensitive(options.ignore_case)
            .multi_line(true)
            .build()?;
        Ok(Self::Regex(regex))
    }

    fn search_file(&self, content: &[u8], options: &Options) -> Vec<MatchLine> {
        let mut matches = Vec::new();
        for_each_line(content, |line_no, line| {
            match self {
                QueryMatcher::Fixed {
                    needle,
                    finder,
                    ac,
                    whole_word,
                    ignore_case,
                } => {
                    if needle.is_empty() {
                        return true;
                    }
                    let found = if *ignore_case {
                        ac.as_ref()
                            .and_then(|ac| ac.find(line).map(|m| (m.start(), m.end())))
                    } else {
                        finder.find(line).map(|start| (start, start + needle.len()))
                    };
                    if let Some((start, end)) = found {
                        if !whole_word || word_boundary(line, start, end - start) {
                            let matched = line[start..end].to_vec();
                            matches.push(MatchLine {
                                line_no,
                                column: start + 1,
                                line: line.to_vec(),
                                matched,
                            });
                        }
                    }
                }
                QueryMatcher::Regex(regex) => {
                    if let Some(m) = regex.find(line) {
                        matches.push(MatchLine {
                            line_no,
                            column: m.start() + 1,
                            line: line.to_vec(),
                            matched: line[m.start()..m.end()].to_vec(),
                        });
                    }
                }
                QueryMatcher::OrderedLiterals {
                    literals,
                    ignore_case,
                } => {
                    let lowered;
                    let haystack = if *ignore_case {
                        lowered = lower_bytes(line);
                        lowered.as_slice()
                    } else {
                        line
                    };
                    let mut pos = 0usize;
                    let mut first = None;
                    let mut last_end = 0usize;
                    let mut ok = true;
                    for literal in literals {
                        if let Some(found) = memmem::find(&haystack[pos..], literal) {
                            let start = pos + found;
                            if first.is_none() {
                                first = Some(start);
                            }
                            last_end = start + literal.len();
                            pos = last_end;
                        } else {
                            ok = false;
                            break;
                        }
                    }
                    if ok {
                        let start = first.unwrap_or(0);
                        matches.push(MatchLine {
                            line_no,
                            column: start + 1,
                            line: line.to_vec(),
                            matched: line[start..last_end.min(line.len())].to_vec(),
                        });
                    }
                }
            }
            options.max_count.is_none_or(|max| matches.len() < max)
        });
        matches
    }
}

fn ordered_dotstar_literals(options: &Options) -> Option<Vec<Vec<u8>>> {
    if options.fixed || options.whole_word || options.pattern.is_empty() {
        return None;
    }
    let bytes = options.pattern.as_bytes();
    let mut literals = Vec::new();
    let mut current = Vec::new();
    let mut escaped = false;
    let mut saw_dotstar = false;
    let mut i = 0usize;
    while i < bytes.len() {
        let b = bytes[i];
        if escaped {
            if b.is_ascii_alphanumeric() {
                return None;
            }
            current.push(b);
            escaped = false;
            i += 1;
            continue;
        }
        if b == b'\\' {
            escaped = true;
            i += 1;
            continue;
        }
        if b == b'.' && i + 1 < bytes.len() && bytes[i + 1] == b'*' {
            if !current.is_empty() {
                literals.push(if options.ignore_case {
                    lower_bytes(&current)
                } else {
                    current.clone()
                });
                current.clear();
            }
            saw_dotstar = true;
            i += 2;
            continue;
        }
        if b"^$|()[]{}+?".contains(&b) {
            return None;
        }
        current.push(b);
        i += 1;
    }
    if escaped {
        return None;
    }
    if !current.is_empty() {
        literals.push(if options.ignore_case {
            lower_bytes(&current)
        } else {
            current
        });
    }
    (saw_dotstar && !literals.is_empty()).then_some(literals)
}

fn query_trigrams(options: &Options) -> Vec<u32> {
    let literals = if options.fixed || regex_meta_free(&options.pattern) {
        vec![options.pattern.clone()]
    } else {
        required_regex_literals(&options.pattern)
    };
    let mut grams = Vec::new();
    for literal in literals {
        grams.extend(literal_trigrams(literal.as_bytes()));
    }
    grams.sort_unstable();
    grams.dedup();
    grams
}

fn required_regex_literals(pattern: &str) -> Vec<String> {
    if has_top_level_alternation(pattern) {
        return Vec::new();
    }
    let meta: HashSet<char> = [
        '^', '$', '.', '*', '+', '?', '(', ')', '[', ']', '{', '}', '|',
    ]
    .into_iter()
    .collect();
    let mut literals = Vec::new();
    let mut current = String::new();
    let mut escaped = false;
    let mut in_class = false;
    let mut depth = 0usize;
    for c in pattern.chars() {
        if escaped {
            if depth > 0 {
                escaped = false;
                continue;
            }
            if c.is_ascii_alphanumeric() {
                if current.len() >= 3 {
                    literals.push(current.clone());
                }
                current.clear();
            } else {
                current.push(c);
            }
            escaped = false;
            continue;
        }
        if c == '\\' {
            escaped = true;
            continue;
        }
        if in_class {
            if c == ']' {
                in_class = false;
            }
            continue;
        }
        if c == '[' {
            if current.len() >= 3 {
                literals.push(current.clone());
            }
            current.clear();
            in_class = true;
            continue;
        }
        if c == '(' {
            if current.len() >= 3 {
                literals.push(current.clone());
            }
            current.clear();
            depth += 1;
            continue;
        }
        if c == ')' && depth > 0 {
            depth -= 1;
            continue;
        }
        if depth > 0 {
            continue;
        }
        if meta.contains(&c) {
            if current.len() >= 3 {
                literals.push(current.clone());
            }
            current.clear();
        } else {
            current.push(c);
        }
    }
    if current.len() >= 3 {
        literals.push(current);
    }
    literals.sort_by(|a, b| b.len().cmp(&a.len()).then_with(|| a.cmp(b)));
    literals.truncate(4);
    literals
}

fn intersect_postings(index: &MappedIndex, grams: &[u32]) -> Result<Vec<u32>> {
    if grams.is_empty() {
        return Ok((0..index.file_count as u32).collect());
    }
    let mut postings = Vec::new();
    for &gram in grams {
        let Some(posting) = index.posting(gram)? else {
            return Ok(Vec::new());
        };
        postings.push(posting.data);
    }
    postings.sort_by_key(|p| p.len());
    let mut result = postings[0].to_vec();
    for posting in postings.into_iter().skip(1) {
        let mut out = Vec::with_capacity(result.len().min(posting.len()));
        let mut i = 0;
        let mut j = 0;
        while i < result.len() && j < posting.len() {
            match result[i].cmp(&posting[j]) {
                Ordering::Less => i += 1,
                Ordering::Greater => j += 1,
                Ordering::Equal => {
                    out.push(result[i]);
                    i += 1;
                    j += 1;
                }
            }
        }
        result = out;
        if result.is_empty() {
            break;
        }
    }
    Ok(result)
}

fn print_results(results: &[FileResult], options: &Options) {
    let show_path = should_show_path(options);
    for result in results {
        if options.files_with_matches {
            println!("{}", result.path);
            continue;
        }
        if options.count {
            if show_path {
                print!("{}:", result.path);
            }
            println!("{}", result.matches.len());
            continue;
        }
        for m in &result.matches {
            if options.json {
                println!(
                    "{{\"type\":\"match\",\"data\":{{\"path\":{{\"text\":\"{}\"}},\"lines\":{{\"text\":\"{}\"}},\"line_number\":{},\"absolute_offset\":0,\"submatches\":[{{\"match\":{{\"text\":\"{}\"}},\"start\":{},\"end\":{}}}]}}}}",
                    json_escape(&result.path),
                    json_escape_bytes(&m.line),
                    m.line_no,
                    json_escape_bytes(&m.matched),
                    m.column - 1,
                    m.column - 1 + m.matched.len()
                );
                continue;
            }
            if options.vimgrep {
                println!(
                    "{}:{}:{}:{}",
                    result.path,
                    m.line_no,
                    m.column,
                    bytes_to_string(&m.line)
                );
                continue;
            }
            if show_path {
                print!("{}:", result.path);
            }
            if options.line_number {
                print!("{}:", m.line_no);
            }
            if options.column {
                print!("{}:", m.column);
            }
            if options.only_matching {
                println!("{}", bytes_to_string(&m.matched));
            } else {
                println!("{}", bytes_to_string(&m.line));
            }
        }
    }
}

fn build_path_matcher(options: &Options) -> Result<Option<(MatcherSet, MatcherSet)>> {
    let positives: Vec<String> = options
        .globs
        .iter()
        .filter(|g| !g.starts_with('!'))
        .cloned()
        .collect();
    let negatives: Vec<String> = options
        .globs
        .iter()
        .filter_map(|g| g.strip_prefix('!').map(ToOwned::to_owned))
        .collect();
    if positives.is_empty() && negatives.is_empty() {
        return Ok(None);
    }
    Ok(Some((
        MatcherSet::new(&positives)?,
        MatcherSet::new(&negatives)?,
    )))
}

fn path_allowed(options: &Options, root: &Path, rel: &str) -> Result<bool> {
    let matcher = build_path_matcher(options)?;
    path_allowed_with_matcher(options, root, rel, matcher.as_ref())
}

fn path_allowed_with_matcher(
    options: &Options,
    root: &Path,
    rel: &str,
    matcher: Option<&(MatcherSet, MatcherSet)>,
) -> Result<bool> {
    if !options.paths.is_empty() {
        let mut ok = false;
        for raw in &options.paths {
            let p = PathBuf::from(raw);
            let abs = if p.is_absolute() { p } else { root.join(p) };
            let abs = fs::canonicalize(&abs).unwrap_or(abs);
            if let Ok(candidate) = abs.strip_prefix(root) {
                let r = candidate.to_string_lossy().replace('\\', "/");
                if r.is_empty() || rel == r || rel.starts_with(&(r + "/")) {
                    ok = true;
                    break;
                }
            }
        }
        if !ok {
            return Ok(false);
        }
    }
    if let Some((positive, negative)) = matcher {
        if positive.set.is_some() && !positive.is_match(rel) {
            return Ok(false);
        }
        if negative.is_match(rel) {
            return Ok(false);
        }
    }
    Ok(true)
}

fn should_show_path(options: &Options) -> bool {
    if let Some(value) = options.with_filename {
        return value;
    }
    if options.paths.len() == 1 && Path::new(&options.paths[0]).is_file() {
        return false;
    }
    true
}

fn file_trigrams(bytes: &[u8]) -> Vec<u32> {
    if bytes.len() < 3 {
        return Vec::new();
    }
    let mut grams = Vec::with_capacity(bytes.len() - 2);
    for window in bytes.windows(3) {
        grams.push(
            ((window[0].to_ascii_lowercase() as u32) << 16)
                | ((window[1].to_ascii_lowercase() as u32) << 8)
                | (window[2].to_ascii_lowercase() as u32),
        );
    }
    grams.sort_unstable();
    grams.dedup();
    grams
}

fn literal_trigrams(bytes: &[u8]) -> Vec<u32> {
    file_trigrams(bytes)
}

fn for_each_line(mut content: &[u8], mut f: impl FnMut(usize, &[u8]) -> bool) {
    let mut line_no = 1;
    while !content.is_empty() {
        let end = memchr(b'\n', content).unwrap_or(content.len());
        let mut line = &content[..end];
        if line.ends_with(b"\r") {
            line = &line[..line.len() - 1];
        }
        if !f(line_no, line) {
            break;
        }
        line_no += 1;
        if end == content.len() {
            break;
        }
        content = &content[end + 1..];
    }
}

fn word_boundary(line: &[u8], start: usize, len: usize) -> bool {
    let before = start == 0 || !is_word(line[start - 1]);
    let after = start + len >= line.len() || !is_word(line[start + len]);
    before && after
}

fn is_word(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
}

fn regex_meta_free(pattern: &str) -> bool {
    !pattern.bytes().any(|b| b"\\.^$|()[]{}*+?".contains(&b))
}

fn has_top_level_alternation(pattern: &str) -> bool {
    let mut escaped = false;
    let mut in_class = false;
    let mut depth = 0usize;
    for c in pattern.chars() {
        if escaped {
            escaped = false;
            continue;
        }
        if c == '\\' {
            escaped = true;
            continue;
        }
        if in_class {
            if c == ']' {
                in_class = false;
            }
            continue;
        }
        match c {
            '[' => in_class = true,
            '(' => depth += 1,
            ')' if depth > 0 => depth -= 1,
            '|' if depth == 0 => return true,
            _ => {}
        }
    }
    false
}

fn lower_bytes(bytes: &[u8]) -> Vec<u8> {
    bytes.iter().map(|b| b.to_ascii_lowercase()).collect()
}

fn bytes_to_string(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

fn json_escape(text: &str) -> String {
    json_escape_bytes(text.as_bytes())
}

fn json_escape_bytes(bytes: &[u8]) -> String {
    bytes
        .iter()
        .flat_map(|&b| match b {
            b'\\' => Cow::Borrowed("\\\\").chars().collect::<Vec<_>>(),
            b'"' => Cow::Borrowed("\\\"").chars().collect::<Vec<_>>(),
            b'\n' => Cow::Borrowed("\\n").chars().collect::<Vec<_>>(),
            b'\r' => Cow::Borrowed("\\r").chars().collect::<Vec<_>>(),
            b'\t' => Cow::Borrowed("\\t").chars().collect::<Vec<_>>(),
            _ => String::from_utf8_lossy(&[b]).chars().collect::<Vec<_>>(),
        })
        .collect()
}

fn parse_size(text: &str) -> Result<u64> {
    let lower = text.trim().to_ascii_lowercase();
    let (number, mult) = if let Some(prefix) = lower.strip_suffix("kb") {
        (prefix, 1024.0)
    } else if let Some(prefix) = lower.strip_suffix('k') {
        (prefix, 1024.0)
    } else if let Some(prefix) = lower.strip_suffix("mb") {
        (prefix, 1024.0 * 1024.0)
    } else if let Some(prefix) = lower.strip_suffix('m') {
        (prefix, 1024.0 * 1024.0)
    } else if let Some(prefix) = lower.strip_suffix("gb") {
        (prefix, 1024.0 * 1024.0 * 1024.0)
    } else if let Some(prefix) = lower.strip_suffix('g') {
        (prefix, 1024.0 * 1024.0 * 1024.0)
    } else {
        (lower.as_str(), 1.0)
    };
    Ok((number.parse::<f64>()? * mult) as u64)
}

fn index_path(root: &Path) -> PathBuf {
    root.join(INDEX_DIR).join(INDEX_FILE)
}

fn delta_dir(root: &Path) -> PathBuf {
    root.join(INDEX_DIR).join(DELTA_DIR)
}

fn lock_path(root: &Path) -> PathBuf {
    root.join(INDEX_DIR).join(LOCK_FILE)
}

fn state_path(root: &Path) -> PathBuf {
    root.join(INDEX_DIR).join(STATE_FILE)
}

fn watch_registry_dir() -> PathBuf {
    env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".indexsearch")
        .join(WATCH_DIR)
}

fn watch_record_path(id: &str) -> PathBuf {
    watch_registry_dir().join(format!("{id}.watch"))
}

fn default_install_dir() -> PathBuf {
    env::var_os("HOME")
        .or_else(|| env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".local")
        .join("bin")
}

fn executable_name(stem: &str) -> String {
    if cfg!(windows) {
        format!("{stem}.exe")
    } else {
        stem.to_string()
    }
}

fn install_executable(src: &Path, dst: &Path) -> Result<()> {
    if fs::canonicalize(src).ok() == fs::canonicalize(dst).ok() {
        return Ok(());
    }
    let tmp = dst.with_extension(format!("tmp.{}", std::process::id()));
    fs::copy(src, &tmp)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&tmp, fs::Permissions::from_mode(0o755))?;
    }
    if dst.exists() {
        let _ = fs::remove_file(dst);
    }
    fs::rename(tmp, dst)?;
    Ok(())
}

fn install_alias(exe_path: &Path, alias_path: &Path) -> Result<()> {
    if alias_path.exists() {
        let _ = fs::remove_file(alias_path);
    }
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(
            exe_path
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("indexsearch"),
            alias_path,
        )?;
    }
    #[cfg(windows)]
    {
        let target = exe_path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("indexsearch.exe");
        fs::write(alias_path, format!("@echo off\r\n\"%~dp0{target}\" %*\r\n"))?;
    }
    Ok(())
}

fn path_contains(dir: &Path) -> bool {
    let Ok(path) = env::var("PATH") else {
        return false;
    };
    env::split_paths(&path).any(|p| p == dir)
}

fn watch_log_path(root: &Path) -> PathBuf {
    root.join(INDEX_DIR).join(WATCH_LOG_FILE)
}

fn watch_id(root: &Path) -> String {
    let canonical = fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    format!("{:016x}", fnv1a(canonical.to_string_lossy().as_bytes()))
}

fn path_is_ancestor(parent: &Path, child: &Path) -> bool {
    child == parent || child.starts_with(parent)
}

fn append_watch_log(root: &Path, message: &str) -> Result<()> {
    fs::create_dir_all(root.join(INDEX_DIR))?;
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(watch_log_path(root))?;
    writeln!(file, "{} {}", log_timestamp(), message)?;
    Ok(())
}

fn log_timestamp() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    secs.to_string()
}

fn acquire_shared_lock(root: &Path) -> Result<IndexLock> {
    fs::create_dir_all(root.join(INDEX_DIR))?;
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(lock_path(root))?;
    file.lock_shared()?;
    Ok(IndexLock::Shared(file))
}

fn acquire_exclusive_lock(root: &Path) -> Result<IndexLock> {
    fs::create_dir_all(root.join(INDEX_DIR))?;
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(lock_path(root))?;
    file.lock_exclusive()?;
    Ok(IndexLock::Exclusive(file))
}

fn write_watch_record(record: &WatchRecord) -> Result<()> {
    fs::create_dir_all(watch_registry_dir())?;
    let text = format!(
        "id={}\npid={}\nroot={}\n",
        record.id,
        record.pid,
        record.root.display()
    );
    fs::write(watch_record_path(&record.id), text)?;
    Ok(())
}

fn read_watch_record(path: &Path) -> Result<WatchRecord> {
    let text = fs::read_to_string(path)?;
    let mut id = String::new();
    let mut pid = 0;
    let mut root = PathBuf::new();
    for line in text.lines() {
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        match key {
            "id" => id = value.to_string(),
            "pid" => pid = value.parse().unwrap_or(0),
            "root" => root = PathBuf::from(value),
            _ => {}
        }
    }
    if id.is_empty() || pid == 0 || root.as_os_str().is_empty() {
        bail!("invalid watch record {}", path.display());
    }
    Ok(WatchRecord { id, root, pid })
}

fn process_alive(pid: u32) -> bool {
    #[cfg(unix)]
    {
        Command::new("kill")
            .arg("-0")
            .arg(pid.to_string())
            .status()
            .is_ok_and(|status| status.success())
    }
    #[cfg(windows)]
    {
        Command::new("tasklist")
            .args(["/FI", &format!("PID eq {pid}")])
            .output()
            .is_ok_and(|output| String::from_utf8_lossy(&output.stdout).contains(&pid.to_string()))
    }
}

fn stop_process(pid: u32) {
    #[cfg(unix)]
    {
        let _ = Command::new("kill").arg(pid.to_string()).status();
    }
    #[cfg(windows)]
    {
        let _ = Command::new("taskkill")
            .args(["/PID", &pid.to_string(), "/T", "/F"])
            .status();
    }
}

fn print_timings(timings: &Timings) {
    println!(
        "timing: git={:.3}s scan={:.3}s process={:.3}s write={:.3}s",
        timings.git, timings.scan, timings.process, timings.write
    );
}

fn timing_summary(timings: &Timings) -> String {
    format!(
        "git={:.3}s scan={:.3}s process={:.3}s write={:.3}s",
        timings.git, timings.scan, timings.process, timings.write
    )
}

fn has_uppercase(text: &str) -> bool {
    text.chars().any(|c| c.is_uppercase())
}

fn fnv1a(bytes: &[u8]) -> u64 {
    let mut hash = 1469598103934665603u64;
    for &byte in bytes {
        hash ^= byte as u64;
        hash = hash.wrapping_mul(1099511628211);
    }
    hash
}

fn align_to(value: u64, alignment: u64) -> u64 {
    (value + alignment - 1) & !(alignment - 1)
}

fn checked_slice(data: &[u8], offset: u64, size: u64) -> Result<&[u8]> {
    let offset = offset as usize;
    let size = size as usize;
    data.get(offset..offset + size)
        .ok_or_else(|| anyhow!("index offset out of range"))
}

fn read_u32_at(data: &[u8], offset: usize) -> Result<u32> {
    let bytes: [u8; 4] = data
        .get(offset..offset + 4)
        .ok_or_else(|| anyhow!("index offset out of range"))?
        .try_into()?;
    Ok(u32::from_le_bytes(bytes))
}

fn read_u64_at(data: &[u8], offset: usize) -> Result<u64> {
    let bytes: [u8; 8] = data
        .get(offset..offset + 8)
        .ok_or_else(|| anyhow!("index offset out of range"))?
        .try_into()?;
    Ok(u64::from_le_bytes(bytes))
}

fn read_i64_at(data: &[u8], offset: usize) -> Result<i64> {
    Ok(read_u64_at(data, offset)? as i64)
}

fn write_u32(writer: &mut impl Write, value: u32) -> Result<()> {
    writer.write_all(&value.to_le_bytes())?;
    Ok(())
}

fn write_u64(writer: &mut impl Write, value: u64) -> Result<()> {
    writer.write_all(&value.to_le_bytes())?;
    Ok(())
}

fn write_i64(writer: &mut impl Write, value: i64) -> Result<()> {
    writer.write_all(&value.to_le_bytes())?;
    Ok(())
}

fn write_padding(writer: &mut impl Write, from: u64, to: u64) -> Result<()> {
    static ZEROS: [u8; 8] = [0; 8];
    let mut current = from;
    while current < to {
        let n = (to - current).min(ZEROS.len() as u64);
        writer.write_all(&ZEROS[..n as usize])?;
        current += n;
    }
    Ok(())
}
