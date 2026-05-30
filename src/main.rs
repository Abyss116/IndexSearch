use std::borrow::Cow;
use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::env;
use std::ffi::OsString;
use std::fs::{self, File, OpenOptions};
use std::io::{BufWriter, IsTerminal, Read, Write};
#[cfg(not(unix))]
use std::net::TcpListener;
use std::net::TcpStream;
#[cfg(unix)]
use std::os::fd::{AsRawFd, FromRawFd, RawFd};
#[cfg(unix)]
use std::os::unix::net::{UnixListener, UnixStream};
#[cfg(windows)]
use std::os::windows::ffi::{OsStrExt, OsStringExt};
#[cfg(windows)]
use std::os::windows::fs::MetadataExt;
#[cfg(windows)]
use std::os::windows::io::{FromRawHandle, RawHandle};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering as AtomicOrdering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use aho_corasick::{AhoCorasick, AhoCorasickBuilder, MatchKind};
use anyhow::{Context, Result, anyhow, bail};
use fs2::FileExt;
use globset::{GlobBuilder, GlobSet, GlobSetBuilder};
use memchr::{memchr, memmem, memrchr};
use memmap2::Mmap;
use notify::{
    Config as NotifyConfig, Event, RecommendedWatcher, RecursiveMode, Watcher,
    event::{EventKind, MetadataKind, ModifyKind},
};
use rayon::prelude::*;
use rayon::{ThreadPool, ThreadPoolBuilder};
use regex::bytes::{Regex, RegexBuilder};
use rustc_hash::{FxHashMap as HashMap, FxHashSet as HashSet};
use walkdir::{DirEntry, WalkDir};

#[cfg(unix)]
type StdoutFd = RawFd;
#[cfg(windows)]
type StdoutFd = RawHandle;
#[cfg(not(any(unix, windows)))]
type StdoutFd = i32;

const INDEX_DIR: &str = ".indexsearch";
const PROJECT_CONFIG_FILE: &str = "is-project-config.txt";
const PROJECT_CONFIG_REL: &str = ".indexsearch/is-project-config.txt";
const LEGACY_PROJECT_FILE: &str = "index-search-project.txt";
const INDEX_FILE: &str = "index.bin";
const DELTA_DIR: &str = "deltas";
const PROJECTS_DIR: &str = "projects";
const LOCK_FILE: &str = "index.lock";
const STATE_FILE: &str = "state.txt";
const PROJECT_LOG_FILE: &str = "project.log";
const SEARCH_DAEMON_FILE: &str = "search-daemon.txt";
const LOCAL_GIT_EXCLUDE_SECTION: &str = "# Local ignore IndexSearch and IndexGraph";
const SEARCH_DAEMON_SERVICE_NAME: &str = "is-daemon";
const SEARCH_DAEMON_PROTOCOL: u32 = 1;
const SEARCH_DAEMON_CAP_SEARCH: &str = "search";
const SEARCH_DAEMON_CAP_UPDATE: &str = "update";
#[cfg(unix)]
const SEARCH_DAEMON_CAP_DIRECT_STDOUT: &str = "direct_stdout";
const SEARCH_DAEMON_REQUEST_MAGIC: &[u8; 8] = b"ISDREQ1\n";
const SEARCH_DAEMON_RESPONSE_MAGIC: &[u8; 8] = b"ISDRES1\n";
const SEARCH_DAEMON_STDOUT_FRAME: u8 = 1;
const SEARCH_DAEMON_STDERR_FRAME: u8 = 2;
const SEARCH_DAEMON_DONE_FRAME: u8 = 3;
const SEARCH_DAEMON_CONTROL_ARG: &str = "--__indexsearch-daemon-control";
const SEARCH_DAEMON_CONTROL_UPDATE: &str = "update";
const SEARCH_DAEMON_SKIP_STARTUP_SYNC_ARG: &str = "--__indexsearch-daemon-skip-startup-sync";
const SEARCH_DAEMON_CONNECT_TIMEOUT: Duration = Duration::from_millis(20);
#[cfg(windows)]
const SEARCH_DAEMON_START_TIMEOUT: Duration = Duration::from_secs(2);
#[cfg(not(windows))]
const SEARCH_DAEMON_START_TIMEOUT: Duration = Duration::from_millis(250);
#[cfg(windows)]
const SEARCH_DAEMON_STREAM_BUFFER_SIZE: usize = 256 * 1024;
#[cfg(not(windows))]
const SEARCH_DAEMON_STREAM_BUFFER_SIZE: usize = 64 * 1024;
const STREAMING_PIPELINE_MIN_CANDIDATES: usize = 50_000;
const POSTING_BUILD_CHUNK_FILES: usize = 4096;
const POSTING_MERGE_MAX_SHARDS: usize = 64;
const INDEX_PROCESS_PROGRESS_GRANULARITY: u64 = 8 * 1024 * 1024;
const DEFAULT_PROJECT_CONFIG: &str = "[IndexSearch.paths.ignore]\n.git/\n.hg/\n.svn/\n.indexsearch/\n\n\
[IndexSearch.files.ignore]\n*.png\n*.jpg\n*.jpeg\n*.gif\n*.pdf\n*.zip\n*.gz\n*.dll\n*.exe\n*.pdb\n*.o\n*.obj\n\n\
[IndexSearch.files.include]\n*\n";
const MAGIC: &[u8; 8] = b"ISIDXR02";
const VERSION: u32 = 4;
const DEFAULT_MAX_FILE_SIZE: u64 = 20 * 1024 * 1024;
const ENABLE_CHUNK_EXTENSION: bool = false;
const CHUNK_SIZE: usize = 32 * 1024;
const CHUNK_OVERLAP: usize = 64;
const CHUNK_BLOOM_BYTES: usize = 256;
const CHUNK_FOOTER_MAGIC: &[u8; 8] = b"ISCHNK01";
const CHUNK_FOOTER_SIZE: usize = 56;
const AGENT_BLOCK_START: &str = "<!-- indexsearch-agent:start -->";
const AGENT_BLOCK_END: &str = "<!-- indexsearch-agent:end -->";
const EMBEDDED_CODEX_SKILL: &str = include_str!("../skills/indexsearch/SKILL.md");
const EMBEDDED_UE_SKILL_CONFIG: &str =
    include_str!("../skills/indexsearch/assets/unreal-engine-is-project-config.txt");
const EMBEDDED_AGENTS_RULE: &str = include_str!("../agent-rules/AGENTS.md");
const EMBEDDED_CLAUDE_RULE: &str = include_str!("../agent-rules/CLAUDE.md");
const EMBEDDED_CURSOR_RULE: &str = include_str!("../agent-rules/cursor/indexsearch.mdc");
const WORD_FRAGMENT_TAG: u32 = 0x2000_0000;
const WORD_FRAGMENT_MIN_LEN: usize = 6;
const WORD_FRAGMENT_MAX_LEN: usize = 6;
const WORD_FRAGMENT_MIN_FILES: u32 = 32;
const WORD_FRAGMENT_MAX_FILES: u32 = 8192;
const SPECIAL_QUALIFIED_CALL: u32 = 0x8000_0001;
const QUALIFIED_CLASS_FRAGMENT_TAG: u32 = 0xC000_0000;
const QUALIFIED_CLASS_FRAGMENT_MAX_LEN: usize = 4;
const PREFIX_POSTING_TAG: u32 = 0x4000_0000;
const PREFIX_MIN_LEN: usize = 5;
const PREFIX_MAX_LEN: usize = 6;
const TRIGRAM_SPACE: usize = 1 << 24;
const TRIGRAM_WORD_BITS: usize = 64;
const TRIGRAM_BITSET_WORDS: usize = TRIGRAM_SPACE / TRIGRAM_WORD_BITS;

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
        Self::new_with_case(patterns, false)
    }

    fn new_with_case(patterns: &[String], case_insensitive: bool) -> Result<Self> {
        if patterns.is_empty() {
            return Ok(Self { set: None });
        }
        let mut builder = GlobSetBuilder::new();
        for pat in patterns {
            add_glob_pattern(&mut builder, pat, case_insensitive)?;
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
    before_context: usize,
    after_context: usize,
    with_filename: Option<bool>,
    heading: Option<bool>,
    files_with_matches: bool,
    files_without_match: bool,
    count: bool,
    count_matches: bool,
    files: bool,
    vimgrep: bool,
    stats: bool,
    quiet: bool,
    only_matching: bool,
    invert_match: bool,
    line_regexp: bool,
    include_zero: bool,
    trim: bool,
    json: bool,
    color: ColorChoice,
    auto_index: bool,
    auto_update: bool,
    profile: bool,
    sort_path: bool,
    git_update: bool,
    force_scan: bool,
    hidden: bool,
    follow: bool,
    max_count: Option<usize>,
    max_filesize: u64,
    pattern: String,
    globs: Vec<String>,
    ignore_files: Vec<String>,
    glob_case_insensitive: bool,
    ignore_file_case_insensitive: bool,
    max_depth: Option<usize>,
    type_includes: Vec<String>,
    type_excludes: Vec<String>,
    compatibility_notes: Vec<SearchCompatibilityNote>,
    paths: Vec<String>,
    cwd: PathBuf,
}

#[derive(Clone)]
struct SearchCompatibilityNote {
    flag: String,
    detail: &'static str,
}

#[derive(Clone, Copy, Default, PartialEq, Eq)]
enum ColorChoice {
    #[default]
    Auto,
    Always,
    Never,
}

impl ColorChoice {
    fn enabled(self) -> bool {
        match self {
            Self::Always => true,
            Self::Never => false,
            Self::Auto => stdout_supports_color(),
        }
    }
}

#[derive(Clone)]
struct FileEntry {
    path: String,
    mtime: i64,
    size: u64,
    content: Vec<u8>,
    compressed_content: Vec<u8>,
}

struct BuiltIndex {
    root: PathBuf,
    config_hash: u64,
    files: Vec<FileEntry>,
    postings: HashMap<u32, Vec<u32>>,
}

#[cfg(windows)]
fn drop_built_index_async(index: BuiltIndex) {
    let _ = std::thread::Builder::new()
        .name("indexsearch-drop-index".to_string())
        .spawn(move || drop(index));
}

#[cfg(not(windows))]
fn drop_built_index_async(index: BuiltIndex) {
    drop(index);
}

#[derive(Default)]
struct Timings {
    git: f64,
    scan: f64,
    process: f64,
    write: f64,
    open_index: f64,
    current_meta: f64,
    change_diff: f64,
    file_read: f64,
    tokenize: f64,
    tokenize_scan_keys: f64,
    tokenize_qualified_calls: f64,
    tokenize_sort_extras: f64,
    tokenize_sort_fragments: f64,
    compress: f64,
    sort: f64,
    select_fragments: f64,
    postings: f64,
    postings_build_chunks: f64,
    postings_merge: f64,
    postings_merge_shards: u64,
    write_prepare_files: f64,
    write_prepare_postings: f64,
    write_prepare_chunks: f64,
    write_header_tables: f64,
    write_postings_paths: f64,
    write_content: f64,
    write_flush_publish: f64,
    write_state: f64,
    write_delta_meta: f64,
    index_cpu_threads: u64,
    index_io_threads: u64,
    indexed_files: u64,
    indexed_bytes: u64,
    gram_keys: u64,
    extra_keys: u64,
    fragment_keys: u64,
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

struct ProjectRecord {
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

struct ReadIndexFile {
    ordinal: usize,
    rel: String,
    mtime: i64,
    bytes: Vec<u8>,
}

struct BuiltIndexFile {
    ordinal: usize,
    file: FileEntry,
    gram_arena: usize,
    gram_start: usize,
    gram_len: usize,
    fragment_arena: usize,
    fragment_start: usize,
    fragment_len: usize,
}

#[derive(Default)]
struct BuiltIndexFiles {
    files: Vec<BuiltIndexFile>,
    gram_arenas: Vec<Vec<u32>>,
    fragment_arenas: Vec<Vec<u32>>,
}

impl BuiltIndexFiles {
    fn grams(&self, file: &BuiltIndexFile) -> &[u32] {
        &self.gram_arenas[file.gram_arena][file.gram_start..file.gram_start + file.gram_len]
    }

    fn fragments(&self, file: &BuiltIndexFile) -> &[u32] {
        &self.fragment_arenas[file.fragment_arena]
            [file.fragment_start..file.fragment_start + file.fragment_len]
    }
}

#[derive(Default)]
struct IndexWorkerOutput {
    files: Vec<BuiltIndexFile>,
    grams: Vec<u32>,
    fragments: Vec<u32>,
}

struct IndexKeyScratch {
    trigram: TrigramScratch,
    grams: Vec<u32>,
    extras: Vec<u32>,
    fragments: Vec<u32>,
}

impl IndexKeyScratch {
    fn new() -> Self {
        Self {
            trigram: TrigramScratch::new(),
            grams: Vec::new(),
            extras: Vec::new(),
            fragments: Vec::new(),
        }
    }
}

#[derive(Default)]
struct IndexBuildStats {
    cpu_threads: AtomicU64,
    io_threads: AtomicU64,
    skipped_reads: AtomicU64,
    indexed_files: AtomicU64,
    indexed_bytes: AtomicU64,
    gram_keys: AtomicU64,
    extra_keys: AtomicU64,
    fragment_keys: AtomicU64,
    read_ns: AtomicU64,
    tokenize_ns: AtomicU64,
    tokenize_scan_ns: AtomicU64,
    tokenize_qualified_ns: AtomicU64,
    tokenize_sort_extras_ns: AtomicU64,
    tokenize_sort_fragments_ns: AtomicU64,
    compress_ns: AtomicU64,
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

struct TrigramScratch {
    bits: Vec<u64>,
    touched_words: Vec<usize>,
}

impl TrigramScratch {
    fn new() -> Self {
        Self {
            bits: vec![0; TRIGRAM_BITSET_WORDS],
            touched_words: Vec::with_capacity(4096),
        }
    }
}

struct PathFilter {
    prefixes: Vec<String>,
    matcher: Option<(MatcherSet, MatcherSet)>,
    max_depth: Option<usize>,
}

struct FileView<'a> {
    content: Cow<'a, [u8]>,
}

#[derive(Clone)]
struct PostingView<'a> {
    data: Cow<'a, [u32]>,
}

struct MappedIndex {
    mmap: Mmap,
    index_size: u64,
    index_mtime: i64,
    version: u32,
    root: PathBuf,
    config_hash: u64,
    file_count: usize,
    posting_count: usize,
    file_table_offset: usize,
    posting_table_offset: usize,
    postings_data_offset: usize,
    path_blob_offset: usize,
    content_blob_offset: usize,
    chunk_info: Option<ChunkInfo>,
}

#[derive(Clone, Copy)]
struct Header {
    version: u32,
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

#[derive(Clone, Copy)]
struct ChunkInfo {
    chunk_count: usize,
    chunk_table_offset: usize,
    chunk_posting_count: usize,
    chunk_posting_table_offset: usize,
    chunk_posting_data_offset: usize,
    chunk_blob_offset: usize,
}

#[allow(dead_code)]
#[derive(Clone, Copy)]
struct ChunkRecord {
    file_id: u32,
    start: u64,
    size: u32,
    line_no: u32,
}

#[derive(Clone)]
struct MatchLine {
    line_no: usize,
    column: usize,
    line: Vec<u8>,
    matched: Vec<u8>,
}

struct RenderedFileResult {
    path: String,
    output: Vec<u8>,
    match_count: usize,
}

struct SearchOutput {
    code: i32,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

#[derive(Default)]
struct RenderStats {
    matched_files: usize,
    match_count: usize,
}

struct SearchDaemonRun {
    code: i32,
    log_stats: String,
}

#[derive(Default)]
struct SearchProfile {
    events: Vec<(&'static str, f64)>,
}

impl SearchProfile {
    fn record(&mut self, name: &'static str, elapsed: Duration) {
        self.events.push((name, elapsed.as_secs_f64() * 1000.0));
    }
}

fn elapsed_ns(start: Instant) -> u64 {
    start.elapsed().as_nanos().min(u64::MAX as u128) as u64
}

fn ns_to_secs(ns: u64) -> f64 {
    ns as f64 / 1_000_000_000.0
}

#[derive(Clone)]
struct QualifiedCallSpec {
    class_prefix: Option<Vec<u8>>,
    class_min_extra: usize,
}

#[derive(Clone)]
struct SearchDaemonRecord {
    service_name: String,
    protocol: u32,
    capabilities: Vec<String>,
    pid: u32,
    port: u16,
    socket_path: Option<PathBuf>,
    token: String,
    root: PathBuf,
    exe_path: PathBuf,
    exe_size: u64,
    exe_mtime: i64,
    index_size: u64,
    index_mtime: i64,
}

impl SearchDaemonRecord {
    fn current_capabilities() -> Vec<String> {
        let mut capabilities = Vec::with_capacity(3);
        capabilities.push(SEARCH_DAEMON_CAP_SEARCH.to_string());
        capabilities.push(SEARCH_DAEMON_CAP_UPDATE.to_string());
        #[cfg(unix)]
        capabilities.push(SEARCH_DAEMON_CAP_DIRECT_STDOUT.to_string());
        capabilities
    }

    fn legacy_capabilities() -> Vec<String> {
        [SEARCH_DAEMON_CAP_SEARCH, SEARCH_DAEMON_CAP_UPDATE]
            .iter()
            .map(|capability| capability.to_string())
            .collect()
    }

    fn has_capability(&self, capability: &str) -> bool {
        self.capabilities.iter().any(|item| item == capability)
    }

    fn supports_search(&self) -> bool {
        self.has_capability(SEARCH_DAEMON_CAP_SEARCH)
    }

    fn supports_update(&self) -> bool {
        self.has_capability(SEARCH_DAEMON_CAP_UPDATE)
    }

    fn capabilities_text(&self) -> String {
        self.capabilities.join(",")
    }
}

fn main() {
    if let Err(err) = run() {
        eprintln!("istool: {err:#}");
        std::process::exit(2);
    }
}

fn run() -> Result<()> {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() {
        print_help();
        std::process::exit(2);
    }
    normalize_leading_command_flags(&mut args);
    if args[0] == "--" {
        args.remove(0);
        std::process::exit(command_search(&args)?);
    }
    match args[0].as_str() {
        "index" => {
            args.remove(0);
            if maybe_print_command_help("index", &args) {
                std::process::exit(0);
            }
            std::process::exit(command_index(&args)?);
        }
        "update" => {
            args.remove(0);
            if maybe_print_command_help("update", &args) {
                std::process::exit(0);
            }
            std::process::exit(command_update(&args)?);
        }
        "compact" => {
            args.remove(0);
            if maybe_print_command_help("compact", &args) {
                std::process::exit(0);
            }
            std::process::exit(command_compact(&args)?);
        }
        "clean" => {
            args.remove(0);
            if maybe_print_command_help("clean", &args) {
                std::process::exit(0);
            }
            std::process::exit(command_clean(&args)?);
        }
        "search-daemon" => {
            args.remove(0);
            if maybe_print_command_help("search-daemon", &args) {
                std::process::exit(0);
            }
            std::process::exit(command_search_daemon(&args)?);
        }
        "projects" => {
            args.remove(0);
            if maybe_print_command_help("projects", &args) {
                std::process::exit(0);
            }
            std::process::exit(command_list_projects(&args)?);
        }
        "stop" => {
            args.remove(0);
            if maybe_print_command_help("stop", &args) {
                std::process::exit(0);
            }
            std::process::exit(command_stop_projects(&args)?);
        }
        "log" => {
            args.remove(0);
            if maybe_print_command_help("log", &args) {
                std::process::exit(0);
            }
            std::process::exit(command_project_log(&args)?);
        }
        "install" => {
            args.remove(0);
            if maybe_print_command_help("install", &args) {
                std::process::exit(0);
            }
            std::process::exit(command_install(&args)?);
        }
        "install-skills" => {
            args.remove(0);
            if maybe_print_command_help("install-skills", &args) {
                std::process::exit(0);
            }
            std::process::exit(command_install_skills(&args)?);
        }
        "status" => {
            args.remove(0);
            if maybe_print_command_help("status", &args) {
                std::process::exit(0);
            }
            std::process::exit(command_status(&args)?);
        }
        "search" => {
            args.remove(0);
            if maybe_print_command_help("search", &args) {
                std::process::exit(0);
            }
            std::process::exit(command_search(&args)?);
        }
        "completions" => {
            args.remove(0);
            if maybe_print_command_help("completions", &args) {
                std::process::exit(0);
            }
            std::process::exit(command_completions(&args)?);
        }
        "-h" | "--help" => {
            print_help();
            Ok(())
        }
        "-V" | "--version" => {
            println!("{} {}", cli_binary_name(), env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        "version" => {
            args.remove(0);
            if maybe_print_command_help("version", &args) {
                std::process::exit(0);
            }
            println!("{} {}", cli_binary_name(), env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        value => bail_unknown_command(value),
    }
}

fn cli_binary_name() -> &'static str {
    if env::current_exe()
        .ok()
        .and_then(|path| {
            path.file_stem()
                .and_then(|stem| stem.to_str())
                .map(str::to_string)
        })
        .is_some_and(|stem| stem == "is-daemon" || stem.starts_with("is-daemon-"))
    {
        "is-daemon"
    } else {
        "istool"
    }
}

struct CommandSpec {
    name: &'static str,
    usage: &'static str,
    description: &'static str,
}

const ISTOOL_COMMANDS: &[CommandSpec] = &[
    CommandSpec {
        name: "index",
        usage: "index [PATH]",
        description: "rebuild the base index from scratch",
    },
    CommandSpec {
        name: "update",
        usage: "update [PATH]",
        description: "incrementally update or rebuild the index",
    },
    CommandSpec {
        name: "compact",
        usage: "compact [PATH]",
        description: "fold delta indexes into the base index",
    },
    CommandSpec {
        name: "clean",
        usage: "clean [OPTIONS] [PATH]",
        description: "stop project services and remove index state",
    },
    CommandSpec {
        name: "search-daemon",
        usage: "search-daemon [OPTIONS] PATH",
        description: "run the project service backend",
    },
    CommandSpec {
        name: "projects",
        usage: "projects",
        description: "list active project services",
    },
    CommandSpec {
        name: "stop",
        usage: "stop [--all] [ID|PATH]",
        description: "stop project services",
    },
    CommandSpec {
        name: "log",
        usage: "log [PATH]",
        description: "print project service activity",
    },
    CommandSpec {
        name: "install",
        usage: "install [--dir PATH]",
        description: "install the daemon backend and user-facing commands into a bin dir",
    },
    CommandSpec {
        name: "install-skills",
        usage: "install-skills [OPTIONS]",
        description: "install bundled agent skills and rules",
    },
    CommandSpec {
        name: "status",
        usage: "status [PATH]",
        description: "print index status",
    },
    CommandSpec {
        name: "search",
        usage: "search [OPTIONS] PATTERN [PATH ...]",
        description: "explicit search mode",
    },
    CommandSpec {
        name: "completions",
        usage: "completions [SHELL]",
        description: "print shell completion script",
    },
    CommandSpec {
        name: "version",
        usage: "version",
        description: "print version information",
    },
];

fn command_spec(name: &str) -> Option<&'static CommandSpec> {
    ISTOOL_COMMANDS.iter().find(|command| command.name == name)
}

fn bail_unknown_command(value: &str) -> Result<()> {
    let tool_search = if value.starts_with('-') {
        format!("istool search -- {value}")
    } else {
        format!("istool search {value}")
    };
    let short_search = if value.starts_with('-') {
        format!("is -- {value}")
    } else {
        format!("is {value}")
    };
    bail!(
        "unknown command or misplaced option `{value}`; `istool` requires a subcommand; use `{tool_search}` or `{short_search}` to search"
    )
}

#[derive(Default)]
struct WatchState {
    pending: Mutex<HashSet<String>>,
    index_io: Mutex<()>,
    restart_required: AtomicBool,
}

struct WatchFlushOutcome {
    events: usize,
    changed: bool,
}

fn normalize_leading_command_flags(args: &mut Vec<String>) {
    let mut i = 0;
    while args
        .get(i)
        .is_some_and(|arg| matches!(arg.as_str(), "--profile" | "--instrument"))
    {
        i += 1;
    }
    if i > 0 && args.get(i).is_some_and(|arg| is_command_name(arg)) {
        let command = args.remove(i);
        args.insert(0, command);
    }
}

fn is_command_name(arg: &str) -> bool {
    command_spec(arg).is_some()
}

fn print_help() {
    let style = HelpStyle::new();
    help_section(&style, "Usage");
    println!(
        "  {} {} {}",
        style.cmd("istool"),
        style.meta("<COMMAND>"),
        style.opt("[ARGS]")
    );
    println!();

    help_section(&style, "Commands");
    for command in ISTOOL_COMMANDS {
        help_command(&style, command.usage, command.description);
    }
    println!();

    help_section(&style, "Search Frontends");
    help_command(
        &style,
        "is PATTERN [PATH ...]",
        "rg-like shorthand for istool search",
    );
    help_command(
        &style,
        "indexsearch PATTERN [PATH ...]",
        "same search frontend with a longer name",
    );
    println!();

    help_section(&style, "Common Search Options");
    help_option(&style, "-i, --ignore-case", "case insensitive search");
    help_option(&style, "-s, --case-sensitive", "case sensitive search");
    help_option(&style, "-S, --smart-case", "smart case search");
    help_option(&style, "-F, --fixed-strings", "treat pattern as a literal");
    help_option(&style, "-w, --word-regexp", "require word boundaries");
    help_option(&style, "-e, --regexp PATTERN", "search pattern");
    help_option(&style, "-g, --glob GLOB", "include or exclude path glob");
    help_option(&style, "-n, --line-number", "show line numbers");
    help_option(&style, "-N, --no-line-number", "suppress line numbers");
    help_option(&style, "--column", "show columns");
    help_option(
        &style,
        "-A, --after-context NUM",
        "show NUM lines after each match",
    );
    help_option(
        &style,
        "-B, --before-context NUM",
        "show NUM lines before each match",
    );
    help_option(
        &style,
        "-C, --context NUM",
        "show NUM lines before and after each match",
    );
    help_option(&style, "-H, --with-filename", "always show file names");
    help_option(&style, "-I, --no-filename", "never show file names");
    help_option(&style, "--heading", "group matches under file headings");
    help_option(
        &style,
        "--no-heading",
        "print file name on each matching line",
    );
    help_option(
        &style,
        "-l, --files-with-matches",
        "print matching file paths",
    );
    help_option(
        &style,
        "--files-without-match",
        "print non-matching file paths",
    );
    help_option(&style, "-c, --count", "print match counts per file");
    help_option(
        &style,
        "--count-matches",
        "print match occurrence counts per file",
    );
    help_option(&style, "-o, --only-matching", "print only matching text");
    help_option(&style, "-v, --invert-match", "print non-matching lines");
    help_option(&style, "-x, --line-regexp", "match whole lines only");
    help_option(&style, "-q, --quiet", "suppress normal output");
    help_option(&style, "--files", "print indexed searchable files");
    help_option(&style, "--json", "print JSON Lines matches");
    help_option(&style, "--vimgrep", "print path:line:column:line");
    help_option(&style, "--sort path", "sort matches by path");
    help_option(
        &style,
        "--color WHEN",
        "colorize matches: auto, always, never",
    );
    help_option(&style, "-m, --max-count NUM", "max matching lines per file");
    help_option(&style, "--stats", "print search stats");
    help_option(
        &style,
        "--profile, --instrument",
        "print internal timing breakdown to stderr",
    );
    println!();

    help_section(&style, "Indexing and Freshness Options");
    help_option(
        &style,
        "--max-filesize SIZE",
        "skip larger files while indexing",
    );
    help_option(&style, "--hidden", "include hidden paths while indexing");
    help_option(&style, "-L, --follow", "follow symlinks while indexing");
    help_option(
        &style,
        "--git",
        "explicit Git changed-path update, including local and untracked files",
    );
    help_option(
        &style,
        "--force-scan",
        "bypass a running project service and reconcile by scanning the filesystem",
    );
    help_option(
        &style,
        "--no-auto-index",
        "search fails instead of building a missing/stale index",
    );
    help_option(
        &style,
        "--auto-update",
        "search reconciles the index before running",
    );
    help_option(
        &style,
        "--profile, --instrument",
        "print internal timing breakdown to stderr",
    );
    println!();

    println!(
        "Run {} for command-specific options.",
        style.cmd("istool <COMMAND> --help")
    );
    println!(
        "Run {} for the full search option list.",
        style.cmd("istool search --help")
    );
    println!(
        "`istool` always expects a subcommand; use {} or {} for searches.",
        style.cmd("istool search PATTERN [PATH ...]"),
        style.cmd("is PATTERN [PATH ...]")
    );
}

fn maybe_print_command_help(command: &str, args: &[String]) -> bool {
    if args.iter().any(|arg| arg == "-h" || arg == "--help") {
        print_command_help(command);
        true
    } else {
        false
    }
}

fn print_command_help(command: &str) {
    match command {
        "index" => print_index_help(),
        "update" => print_update_help(),
        "compact" => print_compact_help(),
        "clean" => print_clean_help(),
        "search-daemon" => print_search_daemon_help(),
        "projects" => print_projects_help(),
        "stop" => print_stop_help(),
        "log" => print_project_log_help(),
        "install" => print_install_help(),
        "install-skills" => print_install_skills_help(),
        "status" => print_status_help(),
        "search" => print_search_help(),
        "completions" => print_completions_help(),
        "version" => print_version_help(),
        _ => print_help(),
    }
}

fn print_index_help() {
    let style = HelpStyle::new();
    command_usage(
        &style,
        "index",
        "[OPTIONS] [PATH]",
        "rebuild the base index from scratch",
    );
    help_section(&style, "Options");
    help_option(
        &style,
        "--max-filesize SIZE",
        "skip larger files while indexing",
    );
    help_option(&style, "--hidden", "include hidden paths while indexing");
    help_option(&style, "-L, --follow", "follow symlinks while indexing");
    help_option(
        &style,
        "--profile, --instrument",
        "print internal timing breakdown to stderr",
    );
}

fn print_update_help() {
    let style = HelpStyle::new();
    command_usage(
        &style,
        "update",
        "[OPTIONS] [PATH]",
        "incrementally update or rebuild the index",
    );
    help_section(&style, "Options");
    help_option(
        &style,
        "--max-filesize SIZE",
        "skip larger files while indexing",
    );
    help_option(&style, "--hidden", "include hidden paths while indexing");
    help_option(&style, "-L, --follow", "follow symlinks while indexing");
    help_option(
        &style,
        "--git",
        "explicit Git changed-path update, including local and untracked files",
    );
    help_option(
        &style,
        "--force-scan",
        "bypass a running project service and reconcile by scanning the filesystem",
    );
    help_option(
        &style,
        "--profile, --instrument",
        "print internal timing breakdown to stderr",
    );
}

fn print_compact_help() {
    let style = HelpStyle::new();
    command_usage(
        &style,
        "compact",
        "[PATH]",
        "fold delta indexes into the base index",
    );
    help_section(&style, "Options");
    help_option(
        &style,
        "--profile, --instrument",
        "print internal timing breakdown to stderr",
    );
}

fn print_clean_help() {
    let style = HelpStyle::new();
    command_usage(
        &style,
        "clean",
        "[OPTIONS] [PATH]",
        "stop project services and remove index state found in parent paths",
    );
    help_section(&style, "Options");
    help_option(
        &style,
        "-y, --yes",
        "run without an interactive confirmation",
    );
    help_option(&style, "--full", "remove the entire .indexsearch directory");
    help_option(&style, "--dry-run", "print what would be removed");
}

fn print_projects_help() {
    let style = HelpStyle::new();
    command_usage(&style, "projects", "", "list active project services");
}

fn print_stop_help() {
    let style = HelpStyle::new();
    command_usage(&style, "stop", "[--all] [ID|PATH]", "stop project services");
    help_option(
        &style,
        "--all",
        "stop every registered service and best-effort stale daemon process",
    );
}

fn print_project_log_help() {
    let style = HelpStyle::new();
    command_usage(
        &style,
        "log",
        "[PATH]",
        "print project service activity for a root",
    );
}

fn print_search_daemon_help() {
    let style = HelpStyle::new();
    command_usage(
        &style,
        "search-daemon",
        "[OPTIONS] PATH",
        "run the project service backend",
    );
    help_section(&style, "Options");
    help_option(&style, "--detach", "start the service in the background");
    help_option(
        &style,
        "--idle-seconds NUM",
        "seconds of inactivity before idle maintenance",
    );
    help_option(
        &style,
        "--compact-delta-count NUM",
        "compact after this many delta indexes",
    );
    help_option(
        &style,
        "--compact-delta-bytes SIZE",
        "compact after this many delta bytes",
    );
}

fn print_completions_help() {
    let style = HelpStyle::new();
    command_usage(
        &style,
        "completions",
        "[SHELL]",
        "print shell completion script",
    );
    help_section(&style, "Shells");
    help_option(
        &style,
        "powershell",
        "PowerShell Register-ArgumentCompleter script",
    );
    help_option(&style, "bash", "bash completion script");
    help_option(&style, "zsh", "zsh completion script");
    help_option(&style, "fish", "fish completion script");
}

fn print_version_help() {
    let style = HelpStyle::new();
    command_usage(&style, "version", "", "print version information");
}

fn print_install_help() {
    let style = HelpStyle::new();
    command_usage(
        &style,
        "install",
        "[OPTIONS] [DIR]",
        "install the daemon backend and user-facing commands into a bin dir",
    );
    help_section(&style, "Options");
    help_option(
        &style,
        "--dir PATH",
        "copy into PATH instead of the default user bin directory",
    );
}

fn print_install_skills_help() {
    let style = HelpStyle::new();
    command_usage(
        &style,
        "install-skills",
        "[OPTIONS]",
        "install bundled agent skills and rules",
    );
    help_section(&style, "Options");
    help_option(
        &style,
        "--target TARGET",
        "auto, all, codex, claude, opencode, cursor, agents",
    );
    help_option(&style, "--scope SCOPE", "user or project (default: user)");
    help_option(
        &style,
        "--project PATH",
        "project root for project installs",
    );
    help_option(
        &style,
        "--ue-template",
        "copy the Unreal Engine is-project-config.txt template",
    );
    help_option(&style, "--force", "replace an existing UE template");
    help_option(&style, "--dry-run", "show what would be installed");
}

fn print_status_help() {
    let style = HelpStyle::new();
    command_usage(&style, "status", "[PATH]", "print index status");
}

fn print_search_help() {
    let style = HelpStyle::new();
    if let Ok(frontend_name) = env::var("INDEXSEARCH_FRONTEND_HELP_NAME") {
        help_section(&style, "Usage");
        println!(
            "  {} {}",
            style.cmd(&frontend_name),
            style.meta("[OPTIONS] PATTERN [PATH ...]")
        );
        println!();
        help_section(&style, "Description");
        println!("  search the indexed tree");
        println!();
    } else {
        command_usage(
            &style,
            "search",
            "[OPTIONS] PATTERN [PATH ...]",
            "search the indexed tree",
        );
    }
    help_section(&style, "Options");
    help_option(&style, "-i, --ignore-case", "case insensitive search");
    help_option(&style, "-s, --case-sensitive", "case sensitive search");
    help_option(&style, "-S, --smart-case", "smart case search");
    help_option(&style, "-F, --fixed-strings", "treat pattern as a literal");
    help_option(&style, "-w, --word-regexp", "require word boundaries");
    help_option(&style, "-e, --regexp PATTERN", "search pattern");
    help_option(
        &style,
        "-- PATTERN",
        "treat following arguments as pattern and paths",
    );
    help_option(&style, "-g, --glob GLOB", "include or exclude path glob");
    help_option(
        &style,
        "--iglob GLOB",
        "case insensitive include or exclude path glob",
    );
    help_option(
        &style,
        "--glob-case-insensitive",
        "match subsequent globs case insensitively",
    );
    help_option(
        &style,
        "--glob-case-sensitive",
        "match subsequent globs case sensitively",
    );
    help_option(&style, "-n, --line-number", "show line numbers");
    help_option(&style, "-N, --no-line-number", "suppress line numbers");
    help_option(&style, "--column", "show columns");
    help_option(&style, "--no-column", "suppress columns");
    help_option(
        &style,
        "-A, --after-context NUM",
        "show NUM lines after each match",
    );
    help_option(
        &style,
        "-B, --before-context NUM",
        "show NUM lines before each match",
    );
    help_option(
        &style,
        "-C, --context NUM",
        "show NUM lines before and after each match",
    );
    help_option(&style, "-H, --with-filename", "always show file names");
    help_option(&style, "-I, --no-filename", "never show file names");
    help_option(&style, "--heading", "group matches under file headings");
    help_option(
        &style,
        "--no-heading",
        "print file name on each matching line",
    );
    help_option(
        &style,
        "-l, --files-with-matches",
        "print matching file paths",
    );
    help_option(
        &style,
        "--files-without-match",
        "print non-matching file paths",
    );
    help_option(&style, "-c, --count", "print match counts per file");
    help_option(
        &style,
        "--count-matches",
        "print match occurrence counts per file",
    );
    help_option(&style, "-o, --only-matching", "print only matching text");
    help_option(&style, "-v, --invert-match", "print non-matching lines");
    help_option(&style, "-x, --line-regexp", "match whole lines only");
    help_option(
        &style,
        "--trim",
        "trim leading whitespace from printed lines",
    );
    help_option(&style, "--no-trim", "preserve leading whitespace");
    help_option(&style, "-q, --quiet", "suppress normal output");
    help_option(&style, "--files", "print indexed searchable files");
    help_option(&style, "--json", "print JSON Lines matches");
    help_option(&style, "--vimgrep", "print path:line:column:line");
    help_option(&style, "--sort path", "sort matches by path");
    help_option(&style, "--sort-files", "sort file-oriented output by path");
    help_option(
        &style,
        "--color WHEN",
        "colorize matches: auto, always, never",
    );
    help_option(&style, "-m, --max-count NUM", "max matching lines per file");
    help_option(&style, "--max-depth NUM", "descend at most NUM path levels");
    help_option(
        &style,
        "--max-filesize SIZE",
        "skip larger files while auto-indexing",
    );
    help_option(
        &style,
        "--hidden",
        "include hidden paths while auto-indexing",
    );
    help_option(
        &style,
        "-L, --follow",
        "follow symlinks while auto-indexing",
    );
    help_option(&style, "-t, --type TYPE", "include a built-in file type");
    help_option(
        &style,
        "-T, --type-not TYPE",
        "exclude a built-in file type",
    );
    help_option(&style, "--type-list", "print built-in file types");
    help_option(
        &style,
        "--ignore-file PATH",
        "add ignore patterns from PATH",
    );
    help_option(
        &style,
        "--ignore-file-case-insensitive",
        "match ignore-file patterns case insensitively",
    );
    help_option(
        &style,
        "-u, -uu, --unrestricted",
        "relax ignore filtering within indexed files",
    );
    help_option(
        &style,
        "--no-auto-index",
        "do not build a missing or stale index during search",
    );
    help_option(
        &style,
        "--auto-update",
        "reconcile filesystem changes before search",
    );
    help_option(
        &style,
        "--encoding ENCODING",
        "accept rg encoding option; non-UTF-8 decoding is not supported",
    );
    help_option(
        &style,
        "--engine ENGINE",
        "accept rg regex engine option; default/auto are supported",
    );
    help_option(
        &style,
        "--no-require-git",
        "accepted for editor compatibility",
    );
    help_option(
        &style,
        "--no-ignore-parent",
        "accepted for editor compatibility",
    );
    help_option(
        &style,
        "--no-ignore-global",
        "accepted for editor compatibility",
    );
    help_option(&style, "--no-config", "accepted for editor compatibility");
    help_option(&style, "--crlf", "accepted for editor compatibility");
    help_option(&style, "--stats", "print search stats");
    help_option(
        &style,
        "--profile, --instrument",
        "print internal timing breakdown to stderr",
    );
    if env::var("INDEXSEARCH_FRONTEND_HELP_NAME").is_ok() {
        println!();
        help_section(&style, "Related Commands");
        println!(
            "  Use {} for index, update, service, install, and log commands.",
            style.cmd("istool <COMMAND> [ARGS]")
        );
    }
}

fn command_usage(style: &HelpStyle, command: &str, args: &str, description: &str) {
    help_section(style, "Usage");
    if args.is_empty() {
        println!("  {} {}", style.cmd("istool"), style.cmd(command));
    } else {
        println!(
            "  {} {} {}",
            style.cmd("istool"),
            style.cmd(command),
            style.meta(args)
        );
    }
    println!();
    help_section(style, "Description");
    println!("  {description}");
    println!();
}

fn help_section(style: &HelpStyle, text: &str) {
    println!("{}", style.heading(text));
}

fn help_command(style: &HelpStyle, command: &str, description: &str) {
    help_row(style.cmd(command), command.len(), 42, description);
}

fn help_option(style: &HelpStyle, option: &str, description: &str) {
    help_row(style.opt(option), option.len(), 34, description);
}

fn help_row(label: String, label_len: usize, width: usize, description: &str) {
    let padding = width.saturating_sub(label_len).max(1);
    println!("  {label}{}{}", " ".repeat(padding), description);
}

struct HelpStyle {
    color: bool,
}

impl HelpStyle {
    fn new() -> Self {
        Self {
            color: stdout_supports_color(),
        }
    }

    fn heading(&self, text: &str) -> String {
        self.paint(text, "1;36")
    }

    fn cmd(&self, text: &str) -> String {
        self.paint(text, "1;32")
    }

    fn opt(&self, text: &str) -> String {
        self.paint(text, "1;33")
    }

    fn meta(&self, text: &str) -> String {
        self.paint(text, "2")
    }

    fn paint(&self, text: &str, code: &str) -> String {
        if self.color {
            format!("\x1b[{code}m{text}\x1b[0m")
        } else {
            text.to_string()
        }
    }
}

fn stdout_supports_color() -> bool {
    if env::var_os("NO_COLOR").is_some() {
        return false;
    }
    let force_color = env::var("CLICOLOR_FORCE").is_ok_and(|value| value != "0");
    force_color
        || (std::io::stdout().is_terminal()
            && env::var("TERM").map(|term| term != "dumb").unwrap_or(true)
            && stdout_supports_ansi())
}

#[cfg(not(windows))]
fn stdout_supports_ansi() -> bool {
    true
}

#[cfg(windows)]
fn stdout_supports_ansi() -> bool {
    const STD_OUTPUT_HANDLE: u32 = -11i32 as u32;
    const ENABLE_VIRTUAL_TERMINAL_PROCESSING: u32 = 0x0004;

    unsafe extern "system" {
        fn GetStdHandle(n_std_handle: u32) -> *mut std::ffi::c_void;
        fn GetConsoleMode(h_console_handle: *mut std::ffi::c_void, lp_mode: *mut u32) -> i32;
        fn SetConsoleMode(h_console_handle: *mut std::ffi::c_void, dw_mode: u32) -> i32;
    }

    unsafe {
        let handle = GetStdHandle(STD_OUTPUT_HANDLE);
        if handle.is_null() || handle as isize == -1 {
            return false;
        }
        let mut mode = 0u32;
        if GetConsoleMode(handle, &mut mode) == 0 {
            return false;
        }
        if mode & ENABLE_VIRTUAL_TERMINAL_PROCESSING != 0 {
            return true;
        }
        SetConsoleMode(handle, mode | ENABLE_VIRTUAL_TERMINAL_PROCESSING) != 0
    }
}

fn stdout_supports_search_decoration() -> bool {
    std::io::stdout().is_terminal() && env::var("TERM").map(|term| term != "dumb").unwrap_or(true)
}

struct ProgressLine {
    done: Arc<AtomicBool>,
    label: Arc<Mutex<String>>,
    current: Arc<AtomicU64>,
    total: Arc<AtomicU64>,
    determinate: Arc<AtomicBool>,
    completed: Arc<AtomicBool>,
    output_lock: Arc<Mutex<()>>,
    stage_started: Arc<Mutex<Instant>>,
    started: Instant,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl ProgressLine {
    fn start(label: &str) -> Self {
        if !stderr_supports_progress() {
            return Self {
                done: Arc::new(AtomicBool::new(true)),
                label: Arc::new(Mutex::new(label.to_string())),
                current: Arc::new(AtomicU64::new(0)),
                total: Arc::new(AtomicU64::new(0)),
                determinate: Arc::new(AtomicBool::new(false)),
                completed: Arc::new(AtomicBool::new(true)),
                output_lock: Arc::new(Mutex::new(())),
                stage_started: Arc::new(Mutex::new(Instant::now())),
                started: Instant::now(),
                handle: None,
            };
        }
        let done = Arc::new(AtomicBool::new(false));
        let label = Arc::new(Mutex::new(label.to_string()));
        let current = Arc::new(AtomicU64::new(0));
        let total = Arc::new(AtomicU64::new(0));
        let determinate = Arc::new(AtomicBool::new(false));
        let completed = Arc::new(AtomicBool::new(false));
        let output_lock = Arc::new(Mutex::new(()));
        let stage_started = Arc::new(Mutex::new(Instant::now()));
        let thread_done = Arc::clone(&done);
        let thread_label = Arc::clone(&label);
        let thread_current = Arc::clone(&current);
        let thread_total = Arc::clone(&total);
        let thread_determinate = Arc::clone(&determinate);
        let thread_completed = Arc::clone(&completed);
        let thread_output_lock = Arc::clone(&output_lock);
        let thread_stage_started = Arc::clone(&stage_started);
        let started = Instant::now();
        let handle = std::thread::spawn(move || {
            let frames = ["|", "/", "-", "\\"];
            let width = 28usize;
            let mut tick = 0usize;
            while !thread_done.load(AtomicOrdering::Relaxed) {
                if thread_completed.load(AtomicOrdering::Relaxed) {
                    std::thread::sleep(Duration::from_millis(80));
                    continue;
                }
                let text = thread_label
                    .lock()
                    .map(|label| label.clone())
                    .unwrap_or_else(|_| "Working".to_string());
                let is_determinate = thread_determinate.load(AtomicOrdering::Relaxed);
                let current = thread_current.load(AtomicOrdering::Relaxed);
                let total = thread_total.load(AtomicOrdering::Relaxed);
                let (bar, percent) = if is_determinate && total != 0 {
                    (
                        progress_bar_fraction(current, total, width),
                        progress_percent(current, total),
                    )
                } else {
                    (progress_bar(tick, width), 0.0)
                };
                let elapsed = thread_stage_started
                    .lock()
                    .map(|started| format_progress_duration(started.elapsed()))
                    .unwrap_or_else(|_| format_progress_duration(started.elapsed()));
                if let Ok(_guard) = thread_output_lock.lock() {
                    if is_determinate && total != 0 {
                        eprint!(
                            "\r│ {} {:<18} {} {:>5.1}% {:>10}",
                            color_progress(frames[tick % frames.len()]),
                            text,
                            bar,
                            percent,
                            elapsed
                        );
                    } else {
                        eprint!(
                            "\r│ {} {:<18} {} {:>10}",
                            color_progress(frames[tick % frames.len()]),
                            text,
                            bar,
                            elapsed
                        );
                    }
                    clear_progress_line_suffix();
                    let _ = std::io::stderr().flush();
                }
                tick = tick.wrapping_add(1);
                std::thread::sleep(Duration::from_millis(80));
            }
            if let Ok(_guard) = thread_output_lock.lock() {
                clear_progress_line();
            }
        });
        Self {
            done,
            label,
            current,
            total,
            determinate,
            completed,
            output_lock,
            stage_started,
            started,
            handle: Some(handle),
        }
    }

    fn update(&self, label: &str) -> bool {
        if let Ok(mut text) = self.label.lock() {
            let changed = text.as_str() != label;
            *text = label.to_string();
            changed
        } else {
            true
        }
    }

    fn begin_indeterminate(&self, label: &str) {
        let was_completed = self.completed.load(AtomicOrdering::Relaxed);
        if self.update(label) || was_completed {
            self.reset_stage_timer();
        }
        self.current.store(0, AtomicOrdering::Relaxed);
        self.total.store(0, AtomicOrdering::Relaxed);
        self.determinate.store(false, AtomicOrdering::Relaxed);
        self.completed.store(false, AtomicOrdering::Relaxed);
    }

    fn begin_determinate(&self, label: &str, total: u64) {
        let was_completed = self.completed.load(AtomicOrdering::Relaxed);
        if self.update(label) || was_completed {
            self.reset_stage_timer();
        }
        self.current.store(0, AtomicOrdering::Relaxed);
        self.total.store(total, AtomicOrdering::Relaxed);
        self.determinate.store(total != 0, AtomicOrdering::Relaxed);
        self.completed.store(false, AtomicOrdering::Relaxed);
    }

    fn set_indeterminate(&self, label: &str) {
        self.complete_stage();
        self.begin_indeterminate(label);
    }

    fn set_total(&self, label: &str, total: u64) {
        self.complete_stage();
        self.begin_determinate(label, total);
    }

    fn advance(&self, delta: u64) {
        if delta != 0 {
            self.current.fetch_add(delta, AtomicOrdering::Relaxed);
        }
    }

    fn as_active(&self) -> Option<&Self> {
        self.handle.is_some().then_some(self)
    }

    fn reset_stage_timer(&self) {
        if let Ok(mut started) = self.stage_started.lock() {
            *started = Instant::now();
        }
    }

    fn complete_stage(&self) {
        if self.handle.is_none() || self.completed.swap(true, AtomicOrdering::Relaxed) {
            return;
        }
        let text = self
            .label
            .lock()
            .map(|label| label.clone())
            .unwrap_or_else(|_| "Working".to_string());
        let width = 28usize;
        let elapsed = self
            .stage_started
            .lock()
            .map(|started| format_progress_duration(started.elapsed()))
            .unwrap_or_else(|_| format_progress_duration(self.started.elapsed()));
        if let Ok(_guard) = self.output_lock.lock() {
            clear_progress_line();
            eprintln!(
                "│ {} {:<18} {} {:>5.1}% {:>10}",
                color_success("◆"),
                text,
                progress_bar_fraction(1, 1, width),
                100.0,
                elapsed
            );
        }
    }

    fn finish(mut self, label: &str) {
        self.complete_stage();
        self.stop();
        if stderr_supports_progress() {
            eprintln!(
                "{} {} — done ({})",
                color_success("◆"),
                label,
                format_progress_duration(self.started.elapsed())
            );
        }
    }

    fn stop(&mut self) {
        if self.handle.is_some() {
            self.done.store(true, AtomicOrdering::Relaxed);
            if let Some(handle) = self.handle.take() {
                let _ = handle.join();
            }
        }
    }
}

impl Drop for ProgressLine {
    fn drop(&mut self) {
        self.stop();
    }
}

struct ConsoleFlow {
    enabled: bool,
    started: Instant,
}

impl ConsoleFlow {
    fn start() -> Self {
        Self {
            enabled: stderr_supports_progress(),
            started: Instant::now(),
        }
    }

    fn step_done(&self, label: impl AsRef<str>) {
        if self.enabled {
            eprintln!("{} {} — done", color_success("◆"), label.as_ref());
        }
    }

    fn summary(&self, text: impl AsRef<str>) {
        if self.enabled {
            eprintln!("{} {}", color_success("◆"), text.as_ref());
        }
    }

    fn detail(&self, text: impl AsRef<str>) {
        if self.enabled {
            eprintln!("{} {}", color_info("●"), text.as_ref());
        }
    }

    fn compact_stdout(&self) -> bool {
        self.enabled && stdout_supports_search_decoration()
    }

    fn done(&self) {
        if self.enabled {
            eprintln!(
                "└ Done in {}",
                format_progress_duration(self.started.elapsed())
            );
        }
    }
}

fn progress_bar(tick: usize, width: usize) -> String {
    let span = 6usize.min(width);
    let max_pos = width.saturating_sub(span).max(1);
    let pos = tick % (max_pos + 1);
    let mut out = String::with_capacity(width + 2);
    out.push('[');
    for idx in 0..width {
        let ch = if idx >= pos && idx < pos + span {
            '#'
        } else if idx < pos {
            '='
        } else {
            '.'
        };
        out.push(ch);
    }
    out.push(']');
    out
}

fn progress_bar_fraction(current: u64, total: u64, width: usize) -> String {
    let filled = if total == 0 {
        width
    } else {
        ((current.min(total) as u128 * width as u128) / total as u128) as usize
    };
    let mut out = String::with_capacity(width + 2);
    out.push('[');
    for idx in 0..width {
        out.push(if idx < filled { '#' } else { '.' });
    }
    out.push(']');
    out
}

fn progress_percent(current: u64, total: u64) -> f64 {
    if total == 0 {
        100.0
    } else {
        (current.min(total) as f64 * 100.0) / total as f64
    }
}

fn stderr_supports_progress() -> bool {
    std::io::stderr().is_terminal()
        && env::var("TERM").map(|term| term != "dumb").unwrap_or(true)
        && env::var_os("INDEXSEARCH_NO_PROGRESS").is_none()
}

fn clear_progress_line() {
    if stdout_supports_ansi() {
        eprint!("\r\x1b[K");
    } else {
        eprint!("\r{}\r", " ".repeat(120));
    }
    let _ = std::io::stderr().flush();
}

fn clear_progress_line_suffix() {
    if stdout_supports_ansi() {
        eprint!("\x1b[K");
    } else {
        eprint!("{}", " ".repeat(16));
    }
}

fn color_progress(text: &str) -> String {
    if stderr_supports_color() {
        format!("\x1b[1;33m{text}\x1b[0m")
    } else {
        text.to_string()
    }
}

fn color_success(text: &str) -> String {
    if stderr_supports_color() {
        format!("\x1b[1;32m{text}\x1b[0m")
    } else {
        text.to_string()
    }
}

fn color_info(text: &str) -> String {
    if stderr_supports_color() {
        format!("\x1b[1;34m{text}\x1b[0m")
    } else {
        text.to_string()
    }
}

fn format_elapsed_duration(elapsed: Duration) -> String {
    format_elapsed_secs(elapsed.as_secs_f64())
}

fn format_progress_duration(elapsed: Duration) -> String {
    format!("{:.3}s", elapsed.as_secs_f64())
}

fn format_elapsed_secs(secs: f64) -> String {
    if secs > 5.0 {
        format!("{secs:.3}s")
    } else {
        format!("{:.3}ms", secs * 1000.0)
    }
}

fn format_elapsed_millis(ms: f64) -> String {
    if ms > 5_000.0 {
        format!("{:.3}s", ms / 1000.0)
    } else {
        format!("{ms:.3}ms")
    }
}

fn format_count(value: u64) -> String {
    let text = value.to_string();
    let mut out = String::with_capacity(text.len() + text.len() / 3);
    for (idx, ch) in text.chars().rev().enumerate() {
        if idx != 0 && idx % 3 == 0 {
            out.push(',');
        }
        out.push(ch);
    }
    out.chars().rev().collect()
}

fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let mut value = bytes as f64;
    let mut unit = 0usize;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    format!("{value:.3} {} ({} bytes)", UNITS[unit], format_count(bytes))
}

fn stderr_supports_color() -> bool {
    if env::var_os("NO_COLOR").is_some() {
        return false;
    }
    if env::var("CLICOLOR_FORCE").is_ok_and(|value| value != "0") {
        return true;
    }
    std::io::stderr().is_terminal() && stdout_supports_ansi()
}

fn command_index(args: &[String]) -> Result<i32> {
    let (options, start) = parse_index_args(args)?;
    let cfg = load_or_create_config(&start)?;
    let _lock = acquire_exclusive_lock(&cfg.root)?;
    let timer = Instant::now();
    let flow = ConsoleFlow::start();
    flow.step_done(format!("Resolved project {}", display_path(&cfg.root)));
    let progress = ProgressLine::start("Indexing code");
    let mut timings = Timings::default();
    let mut scanned = 0;
    let mut skipped = 0;
    let index = build_index(
        &cfg,
        &options,
        &mut scanned,
        &mut skipped,
        Some(&mut timings),
        progress.as_active(),
    )?;
    progress.set_indeterminate("Writing index");
    let path = index_path(&cfg.root);
    let write_timer = Instant::now();
    save_index_profiled_with_progress(&index, &path, Some(&mut timings), progress.as_active())?;
    remove_delta_dir(&cfg.root)?;
    let state_timer = Instant::now();
    save_index_state(&cfg.root)?;
    timings.write_state += state_timer.elapsed().as_secs_f64();
    timings.write += write_timer.elapsed().as_secs_f64();
    let index_size = index_storage_size(&cfg.root, &path);
    let elapsed = timer.elapsed().as_secs_f64();
    progress.finish("Indexed code");
    flow.summary(format!(
        "Indexed {} files",
        format_count(index.files.len() as u64)
    ));
    flow.detail(format!(
        "{} scanned, {} skipped",
        format_count(scanned),
        format_count(skipped)
    ));
    flow.detail(format!("index size {}", format_bytes(index_size)));
    flow.done();
    if !flow.compact_stdout() {
        println!(
            "indexed {} files ({} skipped, {} scanned) in {}",
            index.files.len(),
            skipped,
            scanned,
            format_elapsed_secs(elapsed)
        );
        print_timings(&timings);
    }
    if options.profile {
        print_index_profile(&timings);
    }
    println!("root: {}", display_path(&cfg.root));
    println!("index: {}", display_path(&path));
    if !flow.compact_stdout() {
        println!("index_size: {}", format_bytes(index_size));
    }
    drop(_lock);
    refresh_or_start_search_daemon_after_index(&cfg.root)?;
    std::mem::forget(index);
    Ok(0)
}

fn command_update(args: &[String]) -> Result<i32> {
    let (options, start) = parse_index_args(args)?;
    let cfg = load_or_create_config(&start)?;
    if !options.force_scan && !options.git_update {
        if let Some(code) = try_update_via_daemon(&cfg)? {
            return Ok(code);
        }
    }
    let _lock = acquire_exclusive_lock(&cfg.root)?;
    let path = index_path(&cfg.root);
    let timer = Instant::now();
    let flow = ConsoleFlow::start();
    flow.step_done(format!("Resolved project {}", display_path(&cfg.root)));
    let progress = ProgressLine::start("Updating index");
    let mut timings = Timings::default();
    let mut scanned = 0;
    let mut skipped = 0;
    let open_timer = Instant::now();
    let old = MappedIndex::open(&path).ok();
    timings.open_index += open_timer.elapsed().as_secs_f64();
    let index = if let Some(ref old_index) = old {
        if old_index.config_hash == cfg.hash {
            let try_git_update = !options.force_scan && options.git_update;
            if try_git_update {
                let include_untracked = true;
                let git_timer = Instant::now();
                let changes = collect_git_changes(&cfg.root, include_untracked)?;
                timings.git += git_timer.elapsed().as_secs_f64();
                match changes {
                    Some(changes) if changes.is_empty() => {
                        let state_timer = Instant::now();
                        save_index_state(&cfg.root)?;
                        timings.write_state += state_timer.elapsed().as_secs_f64();
                        progress.finish("Index already current");
                        flow.summary(format!(
                            "Checked {} files",
                            format_count(old_index.file_count as u64)
                        ));
                        flow.detail("0 changed by git");
                        flow.done();
                        if !flow.compact_stdout() {
                            println!(
                                "updated {} files (0 changed by git) in {}",
                                old_index.file_count,
                                format_elapsed_duration(timer.elapsed())
                            );
                            print_timings(&timings);
                        }
                        if options.profile {
                            print_index_profile(&timings);
                        }
                        println!("root: {}", display_path(&cfg.root));
                        println!("index: {}", display_path(&path));
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
                            Some(&mut timings),
                        )?;
                        timings.process += process_timer.elapsed().as_secs_f64();
                        let write_timer = Instant::now();
                        save_delta_profiled(&cfg.root, &delta, &meta, Some(&mut timings))?;
                        let state_timer = Instant::now();
                        save_index_state(&cfg.root)?;
                        timings.write_state += state_timer.elapsed().as_secs_f64();
                        timings.write += write_timer.elapsed().as_secs_f64();
                        let elapsed = timer.elapsed().as_secs_f64();
                        let visible_count = stats.reused + stats.updated + stats.added;
                        progress.finish("Updated index");
                        flow.summary(format!(
                            "Updated {} files",
                            format_count(visible_count as u64)
                        ));
                        flow.detail(format!(
                            "{} reused, {} added, {} modified, {} removed",
                            format_count(stats.reused as u64),
                            format_count(stats.added as u64),
                            format_count(stats.updated as u64),
                            format_count(stats.removed as u64)
                        ));
                        flow.done();
                        if !flow.compact_stdout() {
                            println!(
                                "updated {} files ({} reused, {} added, {} modified, {} removed, {} skipped, {} scanned) in {}",
                                visible_count,
                                stats.reused,
                                stats.added,
                                stats.updated,
                                stats.removed,
                                skipped,
                                scanned,
                                format_elapsed_secs(elapsed)
                            );
                            print_timings(&timings);
                        }
                        if options.profile {
                            print_index_profile(&timings);
                        }
                        println!("root: {}", display_path(&cfg.root));
                        println!("index: {}", display_path(&path));
                        println!("delta: {}", display_path(&delta_dir(&cfg.root)));
                        return Ok(0);
                    }
                    None => {
                        return update_from_filesystem_scan(
                            &cfg,
                            &options,
                            &path,
                            &timer,
                            &mut timings,
                            progress,
                            &flow,
                        );
                    }
                }
            } else {
                return update_from_filesystem_scan(
                    &cfg,
                    &options,
                    &path,
                    &timer,
                    &mut timings,
                    progress,
                    &flow,
                );
            }
        } else {
            progress.update("Rebuilding index");
            build_index(
                &cfg,
                &options,
                &mut scanned,
                &mut skipped,
                Some(&mut timings),
                progress.as_active(),
            )?
        }
    } else {
        build_index(
            &cfg,
            &options,
            &mut scanned,
            &mut skipped,
            Some(&mut timings),
            progress.as_active(),
        )?
    };
    drop(old);
    progress.set_indeterminate("Writing index");
    let write_timer = Instant::now();
    save_index_profiled_with_progress(&index, &path, Some(&mut timings), progress.as_active())?;
    remove_delta_dir(&cfg.root)?;
    let state_timer = Instant::now();
    save_index_state(&cfg.root)?;
    timings.write_state += state_timer.elapsed().as_secs_f64();
    timings.write += write_timer.elapsed().as_secs_f64();
    let index_size = index_storage_size(&cfg.root, &path);
    let elapsed = timer.elapsed().as_secs_f64();
    progress.finish("Indexed code");
    flow.summary(format!(
        "Indexed {} files",
        format_count(index.files.len() as u64)
    ));
    flow.detail(format!(
        "{} scanned, {} skipped",
        format_count(scanned),
        format_count(skipped)
    ));
    flow.detail(format!("index size {}", format_bytes(index_size)));
    flow.done();
    if !flow.compact_stdout() {
        println!(
            "indexed {} files ({} skipped, {} scanned) in {}",
            index.files.len(),
            skipped,
            scanned,
            format_elapsed_secs(elapsed)
        );
        print_timings(&timings);
    }
    if options.profile {
        print_index_profile(&timings);
    }
    println!("root: {}", display_path(&cfg.root));
    println!("index: {}", display_path(&path));
    if !flow.compact_stdout() {
        println!("index_size: {}", format_bytes(index_size));
    }
    drop(_lock);
    refresh_or_start_search_daemon_after_index(&cfg.root)?;
    std::mem::forget(index);
    Ok(0)
}

fn update_from_filesystem_scan(
    cfg: &ProjectConfig,
    options: &Options,
    path: &Path,
    timer: &Instant,
    timings: &mut Timings,
    progress: ProgressLine,
    flow: &ConsoleFlow,
) -> Result<i32> {
    let mut scanned = 0;
    let mut skipped = 0;
    let (changes, visible_before) =
        collect_filesystem_changes(cfg, options, &mut scanned, &mut skipped, timings)?;

    if changes.is_empty() {
        let state_timer = Instant::now();
        save_index_state(&cfg.root)?;
        timings.write_state += state_timer.elapsed().as_secs_f64();
        let index_size = index_storage_size(&cfg.root, path);
        let elapsed = timer.elapsed().as_secs_f64();
        progress.finish("Index already current");
        flow.summary(format!(
            "Checked {} files",
            format_count(visible_before as u64)
        ));
        flow.detail(format!(
            "{} scanned, {} skipped",
            format_count(scanned),
            format_count(skipped)
        ));
        flow.detail(format!("index size {}", format_bytes(index_size)));
        flow.done();
        if !flow.compact_stdout() {
            println!(
                "updated {} files ({} reused, 0 added, 0 modified, 0 removed, {} skipped, {} scanned) in {}",
                visible_before,
                visible_before,
                skipped,
                scanned,
                format_elapsed_secs(elapsed)
            );
            print_timings(timings);
        }
        if options.profile {
            print_index_profile(timings);
        }
        println!("root: {}", display_path(&cfg.root));
        println!("index: {}", display_path(path));
        if !flow.compact_stdout() {
            println!("index_size: {}", format_bytes(index_size));
        }
        return Ok(0);
    }

    let process_timer = Instant::now();
    let full_scan_count = scanned;
    let mut changed_scanned = 0;
    let mut changed_skipped = 0;
    let (delta, meta, stats) = build_delta_index(
        cfg,
        options,
        &changes,
        &mut changed_scanned,
        &mut changed_skipped,
        Some(&mut *timings),
    )?;
    timings.process += process_timer.elapsed().as_secs_f64();
    skipped += changed_skipped;
    scanned = full_scan_count;

    if stats.added == 0 && stats.updated == 0 && stats.removed == 0 {
        let state_timer = Instant::now();
        save_index_state(&cfg.root)?;
        timings.write_state += state_timer.elapsed().as_secs_f64();
    } else {
        let write_timer = Instant::now();
        save_delta_profiled(&cfg.root, &delta, &meta, Some(&mut *timings))?;
        let state_timer = Instant::now();
        save_index_state(&cfg.root)?;
        timings.write_state += state_timer.elapsed().as_secs_f64();
        timings.write += write_timer.elapsed().as_secs_f64();
    }

    let elapsed = timer.elapsed().as_secs_f64();
    let index_size = index_storage_size(&cfg.root, path);
    let visible_count = stats.reused + stats.updated + stats.added;
    progress.finish("Updated index");
    flow.summary(format!(
        "Updated {} files",
        format_count(visible_count as u64)
    ));
    flow.detail(format!(
        "{} reused, {} added, {} modified, {} removed",
        format_count(stats.reused as u64),
        format_count(stats.added as u64),
        format_count(stats.updated as u64),
        format_count(stats.removed as u64)
    ));
    flow.detail(format!(
        "{} scanned, {} skipped",
        format_count(scanned),
        format_count(skipped)
    ));
    flow.detail(format!("index size {}", format_bytes(index_size)));
    flow.done();
    if !flow.compact_stdout() {
        println!(
            "updated {} files ({} reused, {} added, {} modified, {} removed, {} skipped, {} scanned) in {}",
            visible_count,
            stats.reused,
            stats.added,
            stats.updated,
            stats.removed,
            skipped,
            scanned,
            format_elapsed_secs(elapsed)
        );
        print_timings(timings);
    }
    if options.profile {
        print_index_profile(timings);
    }
    println!("root: {}", display_path(&cfg.root));
    println!("index: {}", display_path(path));
    if !flow.compact_stdout() {
        println!("index_size: {}", format_bytes(index_size));
    }
    if stats.added != 0 || stats.updated != 0 || stats.removed != 0 {
        println!("delta: {}", display_path(&delta_dir(&cfg.root)));
    }
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
            "--force-scan" => options.force_scan = true,
            "--profile" | "--instrument" => options.profile = true,
            "--max-filesize" => {
                i += 1;
                options.max_filesize =
                    parse_size(args.get(i).context("missing --max-filesize value")?)?;
            }
            value if value.starts_with('-') => bail!("unsupported option: {value}"),
            value => start = PathBuf::from(value),
        }
        i += 1;
    }
    Ok((options, start))
}

fn sync_index_before_service(cfg: &ProjectConfig, flow: &ConsoleFlow) -> Result<()> {
    let _lock = acquire_exclusive_lock(&cfg.root)?;
    let options = Options {
        max_filesize: DEFAULT_MAX_FILE_SIZE,
        ..Options::default()
    };
    let path = index_path(&cfg.root);
    let timer = Instant::now();
    let progress = ProgressLine::start("Preparing index");
    let mut timings = Timings::default();
    let mut scanned = 0;
    let mut skipped = 0;
    let loaded = MappedIndex::open(&path);
    if loaded
        .as_ref()
        .map(|index| index.config_hash != cfg.hash)
        .unwrap_or(true)
    {
        drop(loaded);
        let index = build_index(
            cfg,
            &options,
            &mut scanned,
            &mut skipped,
            Some(&mut timings),
            progress.as_active(),
        )?;
        progress.set_indeterminate("Writing index");
        let write_timer = Instant::now();
        save_index_with_progress(&index, &path, progress.as_active())?;
        remove_delta_dir(&cfg.root)?;
        save_index_state(&cfg.root)?;
        timings.write += write_timer.elapsed().as_secs_f64();
        let index_size = index_storage_size(&cfg.root, &path);
        append_project_log(
            &cfg.root,
            &format!(
                "startup-index files={} skipped={} scanned={} elapsed={} {}",
                index.files.len(),
                skipped,
                scanned,
                format_elapsed_duration(timer.elapsed()),
                timing_summary(&timings)
            ),
        )?;
        progress.finish("Indexed code");
        flow.summary(format!(
            "Indexed {} files",
            format_count(index.files.len() as u64)
        ));
        flow.detail(format!(
            "{} scanned, {} skipped",
            format_count(scanned),
            format_count(skipped)
        ));
        flow.detail(format!("index size {}", format_bytes(index_size)));
        std::mem::forget(index);
        return Ok(());
    }
    drop(loaded);

    progress.update("Checking changes");
    let (changes, visible_before) =
        collect_filesystem_changes(cfg, &options, &mut scanned, &mut skipped, &mut timings)?;
    if changes.is_empty() {
        save_index_state(&cfg.root)?;
        let index_size = index_storage_size(&cfg.root, &path);
        append_project_log(
            &cfg.root,
            &format!(
                "startup-update files={} reused={} added=0 modified=0 removed=0 skipped={} scanned={} elapsed={} {}",
                visible_before,
                visible_before,
                skipped,
                scanned,
                format_elapsed_duration(timer.elapsed()),
                timing_summary(&timings)
            ),
        )?;
        progress.finish("Index already current");
        flow.summary(format!(
            "Checked {} files",
            format_count(visible_before as u64)
        ));
        flow.detail(format!(
            "{} scanned, {} skipped",
            format_count(scanned),
            format_count(skipped)
        ));
        flow.detail(format!("index size {}", format_bytes(index_size)));
        return Ok(());
    }

    progress.update("Updating index");
    let process_timer = Instant::now();
    let mut changed_scanned = 0;
    let mut changed_skipped = 0;
    let (delta, meta, stats) = build_delta_index(
        cfg,
        &options,
        &changes,
        &mut changed_scanned,
        &mut changed_skipped,
        Some(&mut timings),
    )?;
    timings.process += process_timer.elapsed().as_secs_f64();
    skipped += changed_skipped;
    if stats.added == 0 && stats.updated == 0 && stats.removed == 0 {
        save_index_state(&cfg.root)?;
    } else {
        let write_timer = Instant::now();
        save_delta(&cfg.root, &delta, &meta)?;
        save_index_state(&cfg.root)?;
        timings.write += write_timer.elapsed().as_secs_f64();
    }
    let index_size = index_storage_size(&cfg.root, &path);
    append_project_log(
        &cfg.root,
        &format!(
            "startup-update files={} reused={} added={} modified={} removed={} skipped={} scanned={} elapsed={} {}",
            stats.reused + stats.updated + stats.added,
            stats.reused,
            stats.added,
            stats.updated,
            stats.removed,
            skipped,
            scanned,
            format_elapsed_duration(timer.elapsed()),
            timing_summary(&timings)
        ),
    )?;
    progress.finish("Updated index");
    flow.summary(format!(
        "Updated {} files",
        format_count((stats.reused + stats.updated + stats.added) as u64)
    ));
    flow.detail(format!(
        "{} reused, {} added, {} modified, {} removed",
        format_count(stats.reused as u64),
        format_count(stats.added as u64),
        format_count(stats.updated as u64),
        format_count(stats.removed as u64)
    ));
    flow.detail(format!("index size {}", format_bytes(index_size)));
    Ok(())
}

fn command_list_projects(_args: &[String]) -> Result<i32> {
    fs::create_dir_all(project_registry_dir())?;
    let mut records = Vec::new();
    for entry in fs::read_dir(project_registry_dir())?.flatten() {
        let path = entry.path();
        if path.extension().is_some_and(|ext| ext == "project") {
            if let Ok(record) = read_project_record(&path) {
                records.push((path, record));
            }
        }
    }
    records.sort_by(|a, b| a.1.root.cmp(&b.1.root));
    for (path, record) in records {
        let alive = process_alive(record.pid);
        let daemon = read_search_daemon_record(&search_daemon_record_path(&record.root)).ok();
        let service = daemon
            .as_ref()
            .map(|record| record.service_name.as_str())
            .unwrap_or("unknown");
        let protocol = daemon
            .as_ref()
            .map(|record| record.protocol.to_string())
            .unwrap_or_else(|| "unknown".to_string());
        let capabilities = daemon
            .as_ref()
            .map(SearchDaemonRecord::capabilities_text)
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| "unknown".to_string());
        println!(
            "{}\tpid={}\talive={}\tservice={}\tprotocol={}\tcapabilities={}\t{}",
            record.id,
            record.pid,
            alive,
            service,
            protocol,
            capabilities,
            display_path(&record.root)
        );
        if !alive {
            let _ = fs::remove_file(path);
        }
    }
    Ok(0)
}

fn stop_child_project_services(root: &Path) -> Result<()> {
    fs::create_dir_all(project_registry_dir())?;
    let requested_root = fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    for entry in fs::read_dir(project_registry_dir())?.flatten() {
        let path = entry.path();
        if !path.extension().is_some_and(|ext| ext == "project") {
            continue;
        }
        let Ok(record) = read_project_record(&path) else {
            let _ = fs::remove_file(path);
            continue;
        };
        if !process_alive(record.pid) {
            let _ = fs::remove_file(path);
            continue;
        }
        let record_root = fs::canonicalize(&record.root).unwrap_or_else(|_| record.root.clone());
        if record_root != requested_root && path_is_ancestor(&requested_root, &record_root) {
            stop_process(record.pid);
            let _ = fs::remove_file(path);
            let _ = fs::remove_file(search_daemon_record_path(&record.root));
            let _ = append_project_log(
                &record.root,
                &format!(
                    "project-service-stop pid={} superseded_by={}",
                    record.pid,
                    display_path(root)
                ),
            );
        }
    }
    Ok(())
}

fn command_stop_projects(args: &[String]) -> Result<i32> {
    let all = args.iter().any(|arg| arg == "--all");
    let targets: Vec<&String> = args.iter().filter(|arg| arg.as_str() != "--all").collect();
    if all && !targets.is_empty() {
        bail!("stop --all does not accept an id or path");
    }
    if !all && targets.len() != 1 {
        bail!("stop requires an id or path; use --all to stop every project service");
    }
    let target = targets.first().copied();
    let target_roots = target
        .map(|target| stop_target_roots(Path::new(target)))
        .unwrap_or_default();
    let mut matched = Vec::new();
    fs::create_dir_all(project_registry_dir())?;
    for entry in fs::read_dir(project_registry_dir())?.flatten() {
        let path = entry.path();
        if path.extension().is_some_and(|ext| ext == "project") {
            if let Ok(record) = read_project_record(&path) {
                let by_id = target.is_some_and(|target| record.id == *target);
                let by_path = target_roots
                    .iter()
                    .any(|root| same_clean_root(root, &record.root));
                if all || by_id || by_path {
                    matched.push((path, record));
                }
            }
        }
    }
    let mut stopped = HashSet::default();
    if matched.is_empty() {
        let stale_count = if all {
            stop_stale_daemon_processes(&mut stopped)
        } else {
            0
        };
        if !all {
            if let Some(root) = target_roots.first() {
                if stop_search_daemon_for_root(root, &mut stopped) {
                    println!("stopped project service {}", display_path(root));
                    return Ok(0);
                }
            }
        }
        if stale_count != 0 {
            println!("stopped {stale_count} stale daemon processes");
            return Ok(0);
        }
        if let Some(target) = target {
            eprintln!("indexsearch: no project service matched {target}");
            return Ok(1);
        } else {
            eprintln!("indexsearch: no project services registered or running");
            return Ok(0);
        }
    }
    for (path, record) in matched {
        if stopped.insert(record.pid) {
            stop_process(record.pid);
        }
        let _ = fs::remove_file(path);
        let _ = stop_search_daemon_for_root(&record.root, &mut stopped);
        let _ = append_project_log(
            &record.root,
            &format!("project-service-stop pid={}", record.pid),
        );
        println!(
            "stopped {} pid={} {}",
            record.id,
            record.pid,
            display_path(&record.root)
        );
    }
    if all {
        let stale_count = stop_stale_daemon_processes(&mut stopped);
        if stale_count != 0 {
            println!("stopped {stale_count} stale daemon processes");
        }
    }
    Ok(0)
}

fn stop_all_project_services_quiet() -> Result<usize> {
    fs::create_dir_all(project_registry_dir())?;
    let mut stopped = HashSet::default();
    let mut count = 0usize;
    for entry in fs::read_dir(project_registry_dir())?.flatten() {
        let path = entry.path();
        if !path.extension().is_some_and(|ext| ext == "project") {
            continue;
        }
        let Ok(record) = read_project_record(&path) else {
            let _ = fs::remove_file(path);
            continue;
        };
        if process_alive(record.pid)
            && record.pid != std::process::id()
            && stopped.insert(record.pid)
        {
            stop_process(record.pid);
            count += 1;
        }
        let _ = fs::remove_file(path);
        let _ = stop_search_daemon_for_root(&record.root, &mut stopped);
        let _ = append_project_log(
            &record.root,
            &format!("project-service-stop pid={} reason=install", record.pid),
        );
    }
    count += stop_stale_daemon_processes(&mut stopped);
    Ok(count)
}

fn stop_target_roots(target: &Path) -> Vec<PathBuf> {
    let mut roots = Vec::new();
    if let Ok(path) = fs::canonicalize(target) {
        roots.push(path);
    }
    if let Ok(cfg) = load_config(target) {
        if !roots.iter().any(|root| same_clean_root(root, &cfg.root)) {
            roots.push(cfg.root);
        }
    }
    roots
}

fn stop_search_daemon_for_root(root: &Path, stopped: &mut HashSet<u32>) -> bool {
    let path = search_daemon_record_path(root);
    let Ok(record) = read_search_daemon_record(&path) else {
        let _ = fs::remove_file(path);
        return false;
    };
    if stopped.insert(record.pid) {
        stop_process(record.pid);
    }
    let _ = fs::remove_file(path);
    true
}

fn stop_stale_daemon_processes(stopped: &mut HashSet<u32>) -> usize {
    if env::var_os("INDEXSEARCH_SKIP_STALE_DAEMON_KILL").is_some() {
        return 0;
    }
    let mut count = 0usize;
    for pid in discover_daemon_pids() {
        if pid == std::process::id() || !stopped.insert(pid) {
            continue;
        }
        stop_process(pid);
        count += 1;
    }
    count
}

fn discover_daemon_pids() -> Vec<u32> {
    let mut pids = HashSet::default();
    #[cfg(unix)]
    {
        for pattern in ["is-daemon", "search-daemon"] {
            if let Ok(output) = Command::new("pgrep").args(["-f", pattern]).output() {
                for line in String::from_utf8_lossy(&output.stdout).lines() {
                    if let Ok(pid) = line.trim().parse::<u32>() {
                        pids.insert(pid);
                    }
                }
            }
        }
    }
    #[cfg(windows)]
    {
        let script = concat!(
            "Get-CimInstance Win32_Process | ",
            "Where-Object { $_.Name -like 'is-daemon*' -or $_.CommandLine -match 'search-daemon' } | ",
            "ForEach-Object { $_.ProcessId }"
        );
        if let Ok(output) = Command::new("powershell")
            .args(["-NoProfile", "-Command", script])
            .output()
        {
            for line in String::from_utf8_lossy(&output.stdout).lines() {
                if let Ok(pid) = line.trim().parse::<u32>() {
                    pids.insert(pid);
                }
            }
        }
    }
    pids.into_iter().collect()
}

fn command_project_log(args: &[String]) -> Result<i32> {
    let start = args
        .first()
        .map(PathBuf::from)
        .unwrap_or(env::current_dir()?);
    let Some(root) = find_existing_project_root(&start) else {
        eprintln!(
            "istool: not in an IndexSearch project: {}",
            display_path(&start)
        );
        return Ok(1);
    };
    let cfg = load_config(&root)?;
    let path = project_log_path(&cfg.root);
    let lines = fs::read_to_string(&path).unwrap_or_default();
    if lines.is_empty() {
        println!("project log is empty: {}", display_path(&path));
    } else {
        print!("{lines}");
    }
    Ok(0)
}

fn command_completions(args: &[String]) -> Result<i32> {
    if args.len() > 1 {
        bail!("completions accepts at most one shell: powershell, bash, zsh, or fish");
    }
    let shell = args
        .first()
        .map(|value| value.to_ascii_lowercase())
        .unwrap_or_else(default_completion_shell);
    match shell.as_str() {
        "powershell" | "pwsh" | "ps" => print_powershell_completions(),
        "bash" => print_bash_completions(),
        "zsh" => print_zsh_completions(),
        "fish" => print_fish_completions(),
        _ => {
            bail!("unsupported completion shell `{shell}`; expected powershell, bash, zsh, or fish")
        }
    }
    Ok(0)
}

fn default_completion_shell() -> String {
    #[cfg(windows)]
    {
        "powershell".to_string()
    }
    #[cfg(not(windows))]
    {
        let shell = env::var("SHELL")
            .ok()
            .and_then(|value| {
                Path::new(&value)
                    .file_stem()
                    .and_then(|name| name.to_str())
                    .map(|name| name.to_ascii_lowercase())
            })
            .unwrap_or_default();
        if shell.contains("zsh") {
            "zsh".to_string()
        } else if shell.contains("fish") {
            "fish".to_string()
        } else {
            "bash".to_string()
        }
    }
}

fn print_powershell_completions() {
    println!("Register-ArgumentCompleter -Native -CommandName istool -ScriptBlock {{");
    println!("    param($wordToComplete, $commandAst, $cursorPosition)");
    println!();
    println!("    $commands = @({})", powershell_command_array());
    println!("    $commandSet = @{{}}");
    println!("    foreach ($command in $commands) {{");
    println!("        $commandSet[$command] = $true");
    println!("    }}");
    println!();
    println!("    $seenCommand = $false");
    println!("    $currentStart = $cursorPosition - $wordToComplete.Length");
    println!("    foreach ($element in $commandAst.CommandElements | Select-Object -Skip 1) {{");
    println!(
        "        if ($element.Extent.StartOffset -ge $currentStart -and $element.Extent.EndOffset -le $cursorPosition) {{"
    );
    println!("            continue");
    println!("        }}");
    println!("        if ($element.Extent.EndOffset -gt $cursorPosition) {{");
    println!("            continue");
    println!("        }}");
    println!("        if ($commandSet.ContainsKey($element.Extent.Text)) {{");
    println!("            $seenCommand = $true");
    println!("            break");
    println!("        }}");
    println!("    }}");
    println!();
    println!("    if (-not $seenCommand) {{");
    println!(
        "        $commands | Where-Object {{ $_ -like \"$wordToComplete*\" }} | ForEach-Object {{"
    );
    println!(
        "            [System.Management.Automation.CompletionResult]::new($_, $_, 'ParameterValue', $_)"
    );
    println!("        }}");
    println!("    }}");
    println!("}}");
}

fn print_bash_completions() {
    println!("_istool() {{");
    println!("    local cur command word i");
    println!("    COMPREPLY=()");
    println!("    cur=\"${{COMP_WORDS[COMP_CWORD]}}\"");
    println!("    command=\"\"");
    println!("    for ((i = 1; i < COMP_CWORD; i++)); do");
    println!("        word=\"${{COMP_WORDS[i]}}\"");
    println!("        case \"$word\" in");
    println!(
        "            {}) command=\"$word\"; break ;;",
        bash_command_case()
    );
    println!("        esac");
    println!("    done");
    println!("    if [[ -z \"$command\" ]]; then");
    println!(
        "        COMPREPLY=( $(compgen -W \"{}\" -- \"$cur\") )",
        command_names_space_separated()
    );
    println!("    fi");
    println!("}}");
    println!("complete -F _istool istool");
}

fn print_zsh_completions() {
    println!("#compdef istool");
    println!();
    println!("local -a commands");
    println!("commands=(");
    for command in ISTOOL_COMMANDS {
        println!(
            "  {}",
            shell_single_quote(&format!("{}:{}", command.name, command.description))
        );
    }
    println!(")");
    println!();
    println!("if (( CURRENT <= 2 )); then");
    println!("  _describe 'command' commands");
    println!("else");
    println!("  _files");
    println!("fi");
}

fn print_fish_completions() {
    println!("function __fish_istool_needs_command");
    println!(
        "    not __fish_seen_subcommand_from {}",
        command_names_space_separated()
    );
    println!("end");
    println!();
    for command in ISTOOL_COMMANDS {
        println!(
            "complete -c istool -f -n '__fish_istool_needs_command' -a {} -d {}",
            shell_single_quote(command.name),
            shell_single_quote(command.description)
        );
    }
}

fn powershell_command_array() -> String {
    ISTOOL_COMMANDS
        .iter()
        .map(|command| {
            let escaped = command.name.replace('\'', "''");
            format!("'{escaped}'")
        })
        .collect::<Vec<_>>()
        .join(", ")
}

fn command_names_space_separated() -> String {
    ISTOOL_COMMANDS
        .iter()
        .map(|command| command.name)
        .collect::<Vec<_>>()
        .join(" ")
}

fn bash_command_case() -> String {
    ISTOOL_COMMANDS
        .iter()
        .map(|command| command.name)
        .collect::<Vec<_>>()
        .join("|")
}

fn shell_single_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
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
    let daemon_src = sibling_executable(&src, "is-daemon").unwrap_or_else(|| src.clone());
    let tool_src = sibling_executable(&src, "istool").unwrap_or_else(|| src.clone());
    let daemon_path = dir.join(executable_name("is-daemon"));
    let tool_path = dir.join(executable_name("istool"));
    let exe_path = dir.join(executable_name("indexsearch"));
    let alias_path = dir.join(executable_name("is"));
    let stopped = stop_all_project_services_quiet()?;
    if stopped != 0 {
        println!("stopped {stopped} running project service(s) before install");
    }
    #[cfg(windows)]
    let installed_backend_path = {
        let versioned_daemon_path = dir.join(executable_name(&format!(
            "is-daemon-{}",
            env!("CARGO_PKG_VERSION")
        )));
        install_executable(&daemon_src, &versioned_daemon_path)?;
        if let Err(err) = install_executable(&daemon_src, &daemon_path) {
            eprintln!(
                "indexsearch: warning: could not replace {}; installed versioned backend {} instead ({err:#})",
                display_path(&daemon_path),
                display_path(&versioned_daemon_path)
            );
        }
        versioned_daemon_path
    };
    #[cfg(not(windows))]
    let installed_backend_path = {
        install_executable(&daemon_src, &daemon_path)?;
        daemon_path.clone()
    };
    install_executable(&tool_src, &tool_path)?;
    let frontend_src = src
        .parent()
        .map(|parent| parent.join(executable_name("indexsearch")))
        .filter(|path| {
            path.is_file() && fs::canonicalize(path).ok() != fs::canonicalize(&src).ok()
        });
    let short_frontend_src = src
        .parent()
        .map(|parent| parent.join(executable_name("is")))
        .filter(|path| path.is_file());
    #[cfg(windows)]
    {
        let legacy_alias_path = dir.join("is.cmd");
        if legacy_alias_path != alias_path && legacy_alias_path.exists() {
            let _ = fs::remove_file(&legacy_alias_path);
        }
        warn_legacy_cmd_shims(&dir, &legacy_alias_path);
    }
    if let Some(frontend_src) = frontend_src {
        install_executable(&frontend_src, &exe_path)?;
        install_search_alias(&exe_path, &alias_path)?;
    } else if let Some(short_frontend_src) = short_frontend_src {
        install_executable(&short_frontend_src, &exe_path)?;
        install_search_alias(&exe_path, &alias_path)?;
    } else {
        bail!(
            "cannot find `{}` next to {}; build or package the search frontend first",
            executable_name("indexsearch"),
            display_path(&src)
        );
    }
    println!("installed: {}", display_path(&installed_backend_path));
    if installed_backend_path != daemon_path {
        println!("legacy backend: {}", display_path(&daemon_path));
    }
    println!("tool: {}", display_path(&tool_path));
    println!("frontend: {}", display_path(&exe_path));
    println!("frontend: {}", display_path(&alias_path));
    if !path_contains(&dir) {
        println!(
            "note: add {} to PATH to use istool, indexsearch, and is from any shell",
            display_path(&dir)
        );
    }
    Ok(0)
}

fn sibling_executable(src: &Path, stem: &str) -> Option<PathBuf> {
    let path = src.parent()?.join(executable_name(stem));
    path.is_file().then_some(path)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SkillTarget {
    Auto,
    All,
    Codex,
    Claude,
    OpenCode,
    Cursor,
    Agents,
}

struct InstallSkillsOptions {
    targets: Vec<SkillTarget>,
    scope: String,
    project: Option<PathBuf>,
    ue_template: bool,
    force: bool,
    dry_run: bool,
}

fn command_install_skills(args: &[String]) -> Result<i32> {
    let options = parse_install_skills_args(args)?;
    let project = options
        .project
        .as_ref()
        .map(|path| fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf()));
    let targets = resolve_skill_targets(&options, project.as_deref());
    if targets.is_empty() && !options.ue_template {
        println!("no matching local agent environment found");
        return Ok(0);
    }
    for target in targets {
        match target {
            SkillTarget::Codex => install_codex_skill(&options, project.as_deref())?,
            SkillTarget::Claude => install_claude_skill(&options, project.as_deref())?,
            SkillTarget::OpenCode => install_opencode_rule(&options, project.as_deref())?,
            SkillTarget::Cursor => install_cursor_rule(&options, project.as_deref())?,
            SkillTarget::Agents => install_agents_rule(&options, project.as_deref())?,
            SkillTarget::Auto | SkillTarget::All => {}
        }
    }
    if options.ue_template {
        let project = project
            .as_deref()
            .context("--project is required with --ue-template")?;
        install_ue_template(project, options.force, options.dry_run)?;
    }
    Ok(0)
}

fn parse_install_skills_args(args: &[String]) -> Result<InstallSkillsOptions> {
    let mut options = InstallSkillsOptions {
        targets: vec![SkillTarget::Auto],
        scope: "user".to_string(),
        project: None,
        ue_template: false,
        force: false,
        dry_run: false,
    };
    let mut saw_target = false;
    let mut i = 0usize;
    while i < args.len() {
        match args[i].as_str() {
            "--target" => {
                i += 1;
                let value = args.get(i).context("missing --target value")?;
                options.targets = parse_skill_targets(value)?;
                saw_target = true;
            }
            "--scope" => {
                i += 1;
                options.scope = args.get(i).context("missing --scope value")?.clone();
                if options.scope != "user" && options.scope != "project" {
                    bail!("unsupported scope: {}", options.scope);
                }
            }
            "--project" => {
                i += 1;
                options.project = Some(PathBuf::from(
                    args.get(i).context("missing --project value")?,
                ));
            }
            "--ue-template" => options.ue_template = true,
            "--force" => options.force = true,
            "--dry-run" => options.dry_run = true,
            value if value.starts_with("--target=") => {
                options.targets =
                    parse_skill_targets(value.split_once('=').map(|(_, v)| v).unwrap_or(""))?;
                saw_target = true;
            }
            value if value.starts_with("--scope=") => {
                options.scope = value
                    .split_once('=')
                    .map(|(_, v)| v)
                    .unwrap_or("")
                    .to_string();
                if options.scope != "user" && options.scope != "project" {
                    bail!("unsupported scope: {}", options.scope);
                }
            }
            value if value.starts_with("--project=") => {
                options.project = Some(PathBuf::from(
                    value.split_once('=').map(|(_, v)| v).unwrap_or(""),
                ));
            }
            value => bail!("unsupported install-skills option: {value}"),
        }
        i += 1;
    }
    if !saw_target && options.scope == "project" {
        options.targets = vec![SkillTarget::All];
    }
    Ok(options)
}

fn parse_skill_targets(value: &str) -> Result<Vec<SkillTarget>> {
    let mut targets = Vec::new();
    for raw in value.split(',') {
        let target = match raw.trim() {
            "auto" => SkillTarget::Auto,
            "all" => SkillTarget::All,
            "codex" => SkillTarget::Codex,
            "claude" | "claudecode" | "claude-code" => SkillTarget::Claude,
            "opencode" => SkillTarget::OpenCode,
            "cursor" => SkillTarget::Cursor,
            "agents" | "agents.md" => SkillTarget::Agents,
            "" => continue,
            other => bail!("unsupported skill target: {other}"),
        };
        targets.push(target);
    }
    if targets.is_empty() {
        bail!("no skill targets specified");
    }
    Ok(targets)
}

fn resolve_skill_targets(
    options: &InstallSkillsOptions,
    project: Option<&Path>,
) -> Vec<SkillTarget> {
    let mut out = Vec::new();
    let requested = options.targets.clone();
    for target in requested {
        match target {
            SkillTarget::Auto => {
                if options.scope == "project" {
                    out.push(SkillTarget::Agents);
                    if project.is_some_and(|root| root.join(".cursor").exists()) {
                        out.push(SkillTarget::Cursor);
                    }
                    if project.is_some_and(|root| {
                        root.join("CLAUDE.md").exists() || root.join(".claude").exists()
                    }) {
                        out.push(SkillTarget::Claude);
                    }
                } else {
                    if home_dir().join(".codex").exists() {
                        out.push(SkillTarget::Codex);
                    }
                    if home_dir().join(".claude").exists() {
                        out.push(SkillTarget::Claude);
                    }
                    if home_dir().join(".config").join("opencode").exists() {
                        out.push(SkillTarget::OpenCode);
                    }
                }
            }
            SkillTarget::All => {
                if options.scope == "project" {
                    out.extend([
                        SkillTarget::Agents,
                        SkillTarget::Claude,
                        SkillTarget::Cursor,
                    ]);
                } else {
                    out.extend([
                        SkillTarget::Codex,
                        SkillTarget::Claude,
                        SkillTarget::OpenCode,
                    ]);
                }
            }
            other => out.push(other),
        }
    }
    dedupe_skill_targets(out)
}

fn dedupe_skill_targets(targets: Vec<SkillTarget>) -> Vec<SkillTarget> {
    let mut out = Vec::new();
    for target in targets {
        if !out.contains(&target) {
            out.push(target);
        }
    }
    out
}

fn install_codex_skill(options: &InstallSkillsOptions, project: Option<&Path>) -> Result<()> {
    if options.scope == "project" {
        install_agents_rule(options, project)?;
        return Ok(());
    }
    let root = home_dir().join(".codex").join("skills").join("indexsearch");
    write_text_file(
        &root.join("SKILL.md"),
        EMBEDDED_CODEX_SKILL,
        options.dry_run,
    )?;
    write_text_file(
        &root
            .join("assets")
            .join("unreal-engine-is-project-config.txt"),
        EMBEDDED_UE_SKILL_CONFIG,
        options.dry_run,
    )
}

fn install_claude_skill(options: &InstallSkillsOptions, project: Option<&Path>) -> Result<()> {
    if options.scope == "project" {
        let project = project.context("--project is required for project scope")?;
        let root = project.join(".claude").join("skills").join("indexsearch");
        write_text_file(
            &root.join("SKILL.md"),
            EMBEDDED_CODEX_SKILL,
            options.dry_run,
        )?;
        write_text_file(
            &root
                .join("assets")
                .join("unreal-engine-is-project-config.txt"),
            EMBEDDED_UE_SKILL_CONFIG,
            options.dry_run,
        )?;
        write_marked_block(
            &project.join("CLAUDE.md"),
            EMBEDDED_CLAUDE_RULE,
            options.dry_run,
        )
    } else {
        let root = home_dir()
            .join(".claude")
            .join("skills")
            .join("indexsearch");
        write_text_file(
            &root.join("SKILL.md"),
            EMBEDDED_CODEX_SKILL,
            options.dry_run,
        )?;
        write_text_file(
            &root
                .join("assets")
                .join("unreal-engine-is-project-config.txt"),
            EMBEDDED_UE_SKILL_CONFIG,
            options.dry_run,
        )
    }
}

fn install_opencode_rule(options: &InstallSkillsOptions, project: Option<&Path>) -> Result<()> {
    if options.scope == "project" {
        install_agents_rule(options, project)
    } else {
        write_marked_block(
            &home_dir()
                .join(".config")
                .join("opencode")
                .join("AGENTS.md"),
            EMBEDDED_AGENTS_RULE,
            options.dry_run,
        )
    }
}

fn install_cursor_rule(options: &InstallSkillsOptions, project: Option<&Path>) -> Result<()> {
    let project = project.context("--project is required for Cursor rule installs")?;
    write_text_file(
        &project
            .join(".cursor")
            .join("rules")
            .join("indexsearch.mdc"),
        EMBEDDED_CURSOR_RULE,
        options.dry_run,
    )?;
    if options.scope == "project" {
        install_agents_rule(options, Some(project))?;
    }
    Ok(())
}

fn install_agents_rule(options: &InstallSkillsOptions, project: Option<&Path>) -> Result<()> {
    let project = project.context("--project is required for AGENTS.md installs")?;
    write_marked_block(
        &project.join("AGENTS.md"),
        EMBEDDED_AGENTS_RULE,
        options.dry_run,
    )
}

fn install_ue_template(project: &Path, force: bool, dry_run: bool) -> Result<()> {
    let dst = project_config_path(project);
    let legacy = legacy_project_config_path(project);
    if (dst.exists() || legacy.exists()) && !force {
        let existing = if dst.exists() { &dst } else { &legacy };
        println!(
            "kept existing {}; pass --force to replace it",
            display_path(existing)
        );
        return Ok(());
    }
    write_text_file(&dst, EMBEDDED_UE_SKILL_CONFIG, dry_run)
}

fn write_text_file(path: &Path, text: &str, dry_run: bool) -> Result<()> {
    if dry_run {
        println!("would install {}", display_path(path));
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, text)?;
    println!("installed {}", display_path(path));
    Ok(())
}

fn write_marked_block(path: &Path, block: &str, dry_run: bool) -> Result<()> {
    let wrapped = format!(
        "{AGENT_BLOCK_START}\n{}\n{AGENT_BLOCK_END}\n",
        block.trim_end()
    );
    let updated = if path.exists() {
        let existing = fs::read_to_string(path)?;
        if let (Some(start), Some(end)) = (
            existing.find(AGENT_BLOCK_START),
            existing.find(AGENT_BLOCK_END),
        ) {
            if end > start {
                let end = end + AGENT_BLOCK_END.len();
                let mut text = format!(
                    "{}{}{}",
                    &existing[..start],
                    wrapped.trim_end(),
                    &existing[end..]
                );
                if !text.ends_with('\n') {
                    text.push('\n');
                }
                text
            } else {
                append_marked_block(existing, &wrapped)
            }
        } else {
            append_marked_block(existing, &wrapped)
        }
    } else {
        wrapped
    };
    if dry_run {
        println!("would update {}", display_path(path));
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, updated)?;
    println!("updated {}", display_path(path));
    Ok(())
}

fn append_marked_block(existing: String, wrapped: &str) -> String {
    let sep = if existing.is_empty() || existing.ends_with('\n') {
        ""
    } else {
        "\n"
    };
    format!("{existing}{sep}\n{wrapped}")
}

fn start_embedded_watch_thread(
    cfg: ProjectConfig,
    watch_options: WatchOptions,
    search_record: SearchDaemonRecord,
    shutdown: Arc<AtomicBool>,
    watch_state: Arc<WatchState>,
    index_state: Arc<RwLock<MappedIndex>>,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let result = (|| -> Result<()> {
            fs::create_dir_all(project_registry_dir())?;
            write_project_record(&ProjectRecord {
                id: project_id(&cfg.root),
                root: cfg.root.clone(),
                pid: std::process::id(),
            })?;
            append_project_log(
                &cfg.root,
                &format!("project-service-start pid={}", std::process::id()),
            )?;
            run_watch_loop(
                &cfg,
                watch_options,
                Some(&search_record),
                Some(&shutdown),
                watch_state,
                index_state,
            )?;
            append_project_log(
                &cfg.root,
                &format!("project-service-stop pid={}", std::process::id()),
            )?;
            Ok(())
        })();
        if let Err(err) = result {
            let _ = append_project_log(&cfg.root, &format!("project-service-error {err:#}"));
            shutdown.store(true, AtomicOrdering::Relaxed);
        }
    })
}

fn run_watch_loop(
    cfg: &ProjectConfig,
    watch_options: WatchOptions,
    search_record: Option<&SearchDaemonRecord>,
    shutdown: Option<&AtomicBool>,
    watch_state: Arc<WatchState>,
    index_state: Arc<RwLock<MappedIndex>>,
) -> Result<()> {
    let (tx, rx) = mpsc::channel::<notify::Result<Event>>();
    let mut watcher = RecommendedWatcher::new(
        move |res| {
            let _ = tx.send(res);
        },
        NotifyConfig::default(),
    )?;
    watcher.watch(&cfg.root, RecursiveMode::Recursive)?;

    let idle = Duration::from_secs(watch_options.idle_seconds.max(1));
    loop {
        if shutdown.is_some_and(|flag| flag.load(AtomicOrdering::Relaxed)) {
            break;
        }
        match rx.recv_timeout(idle) {
            Ok(Ok(event)) => {
                let mut pending = watch_state.pending.lock().unwrap();
                collect_event_paths(cfg, event, &mut pending, watch_state.as_ref());
            }
            Ok(Err(_)) => {}
            Err(RecvTimeoutError::Timeout) => {
                if watch_state.restart_required.load(AtomicOrdering::Relaxed) {
                    if let Some(flag) = shutdown {
                        let _ = append_project_log(
                            &cfg.root,
                            "project-service-restart-required reason=config",
                        );
                        flag.store(true, AtomicOrdering::Relaxed);
                        break;
                    }
                }
                let outcome = {
                    let _io = watch_state.index_io.lock().unwrap();
                    flush_watch_state(cfg, &watch_state)?
                };
                if outcome.changed {
                    if let Some(record) = search_record {
                        let _ = refresh_search_daemon_record(record);
                    }
                }
                let compacted = {
                    let _io = watch_state.index_io.lock().unwrap();
                    maybe_compact_idle(cfg, watch_options, search_record, &index_state)?
                };
                if !compacted && let Some(record) = search_record {
                    let _ = refresh_search_daemon_record(record);
                }
            }
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }
    Ok(())
}

fn collect_event_paths(
    cfg: &ProjectConfig,
    event: Event,
    pending: &mut HashSet<String>,
    watch_state: &WatchState,
) {
    if !watch_event_can_change_index(&event.kind) {
        return;
    }
    for path in event.paths {
        if let Some(rel) = rel_path(&cfg.root, &path) {
            if is_project_config_rel(&rel) {
                watch_state
                    .restart_required
                    .store(true, AtomicOrdering::Relaxed);
                continue;
            }
            if path.starts_with(cfg.root.join(INDEX_DIR)) || path.is_dir() {
                continue;
            }
            if path.exists() && (is_hidden(&rel) || !is_searchable(cfg, &rel)) {
                continue;
            }
            pending.insert(rel);
        }
    }
}

fn watch_event_can_change_index(kind: &EventKind) -> bool {
    match kind {
        EventKind::Access(_) => false,
        EventKind::Modify(ModifyKind::Metadata(MetadataKind::AccessTime)) => false,
        EventKind::Any
        | EventKind::Create(_)
        | EventKind::Modify(_)
        | EventKind::Remove(_)
        | EventKind::Other => true,
    }
}

fn flush_watch_state(cfg: &ProjectConfig, watch_state: &WatchState) -> Result<WatchFlushOutcome> {
    let mut pending = watch_state.pending.lock().unwrap();
    if pending.is_empty() {
        return Ok(WatchFlushOutcome {
            events: 0,
            changed: false,
        });
    }
    let paths: HashSet<String> = pending.iter().cloned().collect();
    match flush_watch_batch(cfg, &paths) {
        Ok(changed) => {
            pending.clear();
            Ok(WatchFlushOutcome {
                events: paths.len(),
                changed,
            })
        }
        Err(err) => Err(err),
    }
}

fn flush_watch_batch(cfg: &ProjectConfig, paths: &HashSet<String>) -> Result<bool> {
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
        return Ok(false);
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
            None,
        )?;
        let write_timer = Instant::now();
        save_index(&index, &index_path(&cfg.root))?;
        save_index_state(&cfg.root)?;
        timings.write += write_timer.elapsed().as_secs_f64();
        append_project_log(
            &cfg.root,
            &format!(
                "auto-index files={} skipped={} scanned={} events={} elapsed={} {}",
                index.files.len(),
                skipped,
                scanned,
                paths.len(),
                format_elapsed_duration(timer.elapsed()),
                timing_summary(&timings)
            ),
        )?;
        return Ok(true);
    }
    let mut scanned = 0;
    let mut skipped = 0;
    let process_timer = Instant::now();
    let (delta, meta, stats) =
        build_delta_index(cfg, &options, &changes, &mut scanned, &mut skipped, None)?;
    let process_elapsed = process_timer.elapsed().as_secs_f64();
    if stats.added == 0 && stats.updated == 0 && stats.removed == 0 {
        return Ok(false);
    }
    let write_timer = Instant::now();
    save_delta(&cfg.root, &delta, &meta)?;
    save_index_state(&cfg.root)?;
    let write_elapsed = write_timer.elapsed().as_secs_f64();
    append_project_log(
        &cfg.root,
        &format!(
            "auto-update files={} reused={} added={} modified={} removed={} skipped={} scanned={} events={} elapsed={} process={} write={}",
            stats.reused + stats.added + stats.updated,
            stats.reused,
            stats.added,
            stats.updated,
            stats.removed,
            skipped,
            scanned,
            paths.len(),
            format_elapsed_duration(timer.elapsed()),
            format_elapsed_secs(process_elapsed),
            format_elapsed_secs(write_elapsed)
        ),
    )?;
    Ok(true)
}

fn maybe_compact_idle(
    cfg: &ProjectConfig,
    watch_options: WatchOptions,
    search_record: Option<&SearchDaemonRecord>,
    index_state: &Arc<RwLock<MappedIndex>>,
) -> Result<bool> {
    let files = delta_files(&cfg.root)?;
    if files.is_empty() {
        return Ok(false);
    }
    let total_bytes = files
        .iter()
        .filter_map(|path| fs::metadata(path).ok().map(|m| m.len()))
        .sum::<u64>();
    if files.len() < watch_options.compact_delta_count
        && total_bytes < watch_options.compact_delta_bytes
    {
        return Ok(false);
    }
    compact_root(cfg, search_record, index_state)?;
    Ok(true)
}

fn compact_root(
    cfg: &ProjectConfig,
    search_record: Option<&SearchDaemonRecord>,
    index_state: &Arc<RwLock<MappedIndex>>,
) -> Result<()> {
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
    let compact_path = compacted_index_temp_path(&path);
    save_index(&compacted, &compact_path)?;
    let _lock = acquire_exclusive_lock(&cfg.root)?;
    let mut index_guard = index_state.write().unwrap();
    publish_compacted_index(&compact_path, &path)?;
    *index_guard = MappedIndex::open(&path)?;
    retire_delta_dir(&cfg.root)?;
    save_index_state(&cfg.root)?;
    if let Some(record) = search_record {
        let _ = refresh_search_daemon_record(record);
    }
    let write_elapsed = write_timer.elapsed().as_secs_f64();
    append_project_log(
        &cfg.root,
        &format!(
            "auto-compact files={} deltas={} elapsed={} process={} write={}",
            compacted.files.len(),
            delta_count,
            format_elapsed_duration(timer.elapsed()),
            format_elapsed_secs(process_elapsed),
            format_elapsed_secs(write_elapsed)
        ),
    )?;
    append_project_log(&cfg.root, "project-service-reload reason=compact")?;
    drop_built_index_async(compacted);
    Ok(())
}

fn command_compact(args: &[String]) -> Result<i32> {
    let (options, start) = parse_index_args(args)?;
    let cfg = load_config(&start)?;
    let _lock = acquire_exclusive_lock(&cfg.root)?;
    let timer = Instant::now();
    let flow = ConsoleFlow::start();
    flow.step_done(format!("Resolved project {}", display_path(&cfg.root)));
    let progress = ProgressLine::start("Compacting index");
    let mut timings = Timings::default();
    let path = index_path(&cfg.root);
    let base = MappedIndex::open(&path)?;
    if base.config_hash != cfg.hash {
        eprintln!(
            "indexsearch: index not found or stale: {}",
            display_path(&path)
        );
        return Ok(2);
    }
    let deltas = load_deltas(&cfg.root)?;
    if deltas.is_empty() {
        let index_size = index_storage_size(&cfg.root, &path);
        progress.finish("No deltas to compact");
        flow.summary("Compacted 0 delta indexes");
        flow.detail(format!("index size {}", format_bytes(index_size)));
        flow.done();
        println!(
            "compacted 0 delta indexes in {}",
            format_elapsed_duration(timer.elapsed())
        );
        print_timings(&timings);
        if options.profile {
            print_index_profile(&timings);
        }
        println!("root: {}", display_path(&cfg.root));
        println!("index: {}", display_path(&path));
        println!("index_size: {}", format_bytes(index_size));
        return Ok(0);
    }
    let process_timer = Instant::now();
    let delta_count = deltas.len();
    let compacted = compact_segments(&cfg, &base, &deltas, &options)?;
    timings.process += process_timer.elapsed().as_secs_f64();
    drop(deltas);
    drop(base);
    let write_timer = Instant::now();
    save_compacted_index(&compacted, &path)?;
    retire_delta_dir(&cfg.root)?;
    save_index_state(&cfg.root)?;
    timings.write += write_timer.elapsed().as_secs_f64();
    let index_size = index_storage_size(&cfg.root, &path);
    progress.finish("Compacted index");
    flow.summary(format!(
        "Compacted {} files into base index",
        format_count(compacted.files.len() as u64)
    ));
    flow.detail(format!(
        "{} delta indexes merged",
        format_count(delta_count as u64)
    ));
    flow.detail(format!("index size {}", format_bytes(index_size)));
    flow.done();
    println!(
        "compacted {} files into base index in {}",
        compacted.files.len(),
        format_elapsed_duration(timer.elapsed())
    );
    print_timings(&timings);
    if options.profile {
        print_index_profile(&timings);
    }
    println!("root: {}", display_path(&cfg.root));
    println!("index: {}", display_path(&path));
    println!("index_size: {}", format_bytes(index_size));
    drop(_lock);
    refresh_search_daemon_after_base_index_replaced(&cfg.root, false)?;
    std::mem::forget(compacted);
    Ok(0)
}

struct CleanOptions {
    start: PathBuf,
    yes: bool,
    dry_run: bool,
    full: bool,
}

fn parse_clean_args(args: &[String]) -> Result<CleanOptions> {
    let mut start = env::current_dir()?;
    let mut yes = false;
    let mut dry_run = false;
    let mut full = false;
    for arg in args {
        match arg.as_str() {
            "-y" | "--yes" => yes = true,
            "--dry-run" => dry_run = true,
            "--full" => full = true,
            value => start = PathBuf::from(value),
        }
    }
    Ok(CleanOptions {
        start,
        yes,
        dry_run,
        full,
    })
}

fn command_clean(args: &[String]) -> Result<i32> {
    let options = parse_clean_args(args)?;
    let roots = discover_clean_roots(&options.start);
    if roots.is_empty() {
        println!(
            "cleaned 0 index directories; none found above {}",
            display_path(&options.start)
        );
        return Ok(0);
    }
    if options.dry_run {
        for root in &roots {
            println!("would clean {}", display_path(root));
            let dir = index_state_dir(root);
            if dir.exists() {
                if options.full {
                    println!("would remove {}", display_path(&dir));
                } else {
                    println!(
                        "would remove index state under {} and keep {}",
                        display_path(&dir),
                        PROJECT_CONFIG_FILE
                    );
                }
            }
        }
        return Ok(0);
    }
    if !options.yes && !confirm_clean(&roots)? {
        println!("clean cancelled");
        return Ok(1);
    }

    let mut removed_dirs = 0usize;
    let mut stopped = 0usize;
    for root in roots {
        stopped += stop_services_for_root(&root)?;
        if clean_index_state_dir(&root, options.full)? {
            removed_dirs += 1;
        }
    }
    println!("cleaned {removed_dirs} index directories; stopped {stopped} services");
    Ok(0)
}

fn clean_index_state_dir(root: &Path, full: bool) -> Result<bool> {
    let dir = index_state_dir(root);
    if !dir.exists() {
        return Ok(false);
    }
    if full {
        fs::remove_dir_all(&dir)?;
        println!("removed {}", display_path(&dir));
        return Ok(true);
    }

    let config_path = project_config_path(root);
    for entry in fs::read_dir(&dir)? {
        let entry = entry?;
        let path = entry.path();
        if same_clean_root(&path, &config_path) {
            continue;
        }
        if path.is_dir() {
            fs::remove_dir_all(&path)?;
        } else {
            fs::remove_file(&path)?;
        }
    }
    if fs::read_dir(&dir)?.next().is_none() {
        fs::remove_dir(&dir)?;
        println!("removed {}", display_path(&dir));
    } else {
        println!(
            "cleaned index state under {}; kept {}",
            display_path(&dir),
            PROJECT_CONFIG_FILE
        );
    }
    Ok(true)
}

fn discover_clean_roots(start: &Path) -> Vec<PathBuf> {
    let mut path = fs::canonicalize(start).unwrap_or_else(|_| start.to_path_buf());
    if path.is_file() {
        path = path.parent().unwrap_or(Path::new(".")).to_path_buf();
    }
    let mut roots = Vec::new();
    for ancestor in path.ancestors() {
        if is_cleanable_project_root(ancestor) {
            roots.push(ancestor.to_path_buf());
        }
    }
    roots
}

fn is_cleanable_project_root(root: &Path) -> bool {
    let state_dir = index_state_dir(root);
    if !state_dir.is_dir() {
        return false;
    }
    project_marker_exists(root)
}

fn index_state_dir(root: &Path) -> PathBuf {
    root.join(INDEX_DIR)
}

fn project_config_path(root: &Path) -> PathBuf {
    root.join(INDEX_DIR).join(PROJECT_CONFIG_FILE)
}

fn legacy_project_config_path(root: &Path) -> PathBuf {
    root.join(LEGACY_PROJECT_FILE)
}

fn project_config_exists(root: &Path) -> bool {
    project_config_path(root).is_file() || legacy_project_config_path(root).is_file()
}

fn project_marker_exists(root: &Path) -> bool {
    project_config_exists(root) || index_path(root).is_file()
}

fn is_project_config_rel(rel: &str) -> bool {
    rel == PROJECT_CONFIG_REL || rel == LEGACY_PROJECT_FILE
}

fn confirm_clean(roots: &[PathBuf]) -> Result<bool> {
    if !(std::io::stdin().is_terminal() && std::io::stderr().is_terminal()) {
        eprintln!("indexsearch: clean is destructive; pass --yes to run non-interactively");
        return Ok(false);
    }
    eprintln!(
        "indexsearch: clean will stop services and remove index state; pass --full to remove local config too:"
    );
    for root in roots {
        eprintln!("  {}", display_path(root));
    }
    eprint!("Continue? [y/N] ");
    let _ = std::io::stderr().flush();
    let mut answer = String::new();
    std::io::stdin().read_line(&mut answer)?;
    Ok(matches!(answer.trim(), "y" | "Y" | "yes" | "YES"))
}

fn stop_services_for_root(root: &Path) -> Result<usize> {
    let mut stopped = HashSet::default();
    let root = fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    if let Ok(record) = read_search_daemon_record(&search_daemon_record_path(&root)) {
        if same_clean_root(&record.root, &root) && stopped.insert(record.pid) {
            stop_process(record.pid);
        }
        let _ = fs::remove_file(search_daemon_record_path(&root));
    }
    fs::create_dir_all(project_registry_dir())?;
    for entry in fs::read_dir(project_registry_dir())?.flatten() {
        let path = entry.path();
        if !path.extension().is_some_and(|ext| ext == "project") {
            continue;
        }
        let Ok(record) = read_project_record(&path) else {
            let _ = fs::remove_file(path);
            continue;
        };
        if same_clean_root(&record.root, &root) {
            if stopped.insert(record.pid) {
                stop_process(record.pid);
            }
            let _ = fs::remove_file(path);
        }
    }
    Ok(stopped.len())
}

fn same_clean_root(left: &Path, right: &Path) -> bool {
    fs::canonicalize(left).unwrap_or_else(|_| left.to_path_buf())
        == fs::canonicalize(right).unwrap_or_else(|_| right.to_path_buf())
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
    if stdout_supports_search_decoration() {
        return command_status_pretty(&cfg, &path, loaded);
    }
    println!("root: {}", display_path(&cfg.root));
    println!(
        "config: {}",
        cfg.path
            .as_ref()
            .map(|p| display_path(p))
            .unwrap_or_default()
    );
    println!("index: {}", display_path(&path));
    match loaded {
        Ok(index) => {
            let deltas = load_deltas(&cfg.root).unwrap_or_default();
            let index_size = index_storage_size(&cfg.root, &path);
            let visible_files = current_visible_paths(&cfg.root)
                .map(|paths| paths.len())
                .unwrap_or(index.file_count);
            println!("exists: true");
            println!("index_size: {}", format_bytes(index_size));
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

fn command_status_pretty(
    cfg: &ProjectConfig,
    path: &Path,
    loaded: Result<MappedIndex>,
) -> Result<i32> {
    println!("{:<9} {}", "root", display_path(&cfg.root));
    println!(
        "{:<9} {}",
        "config",
        cfg.path
            .as_ref()
            .map(|p| display_path(p))
            .unwrap_or_default()
    );
    println!("{:<9} {}", "index", display_path(path));
    match loaded {
        Ok(index) => {
            let deltas = load_deltas(&cfg.root).unwrap_or_default();
            let index_size = index_storage_size(&cfg.root, path);
            let visible_files = current_visible_paths(&cfg.root)
                .map(|paths| paths.len())
                .unwrap_or(index.file_count);
            println!("{:<9} ready", "state");
            println!(
                "{:<9} {} visible, {} base",
                "files",
                format_count(visible_files as u64),
                format_count(index.file_count as u64)
            );
            println!(
                "{:<9} {}",
                "trigrams",
                format_count(index.posting_count as u64)
            );
            println!("{:<9} {}", "deltas", format_count(deltas.len() as u64));
            println!("{:<9} {}", "size", format_bytes(index_size));
            println!(
                "{:<9} {}",
                "fresh",
                if index.config_hash == cfg.hash {
                    "yes"
                } else {
                    "no"
                }
            );
            Ok(0)
        }
        Err(_) => {
            println!("{:<9} missing", "state");
            Ok(1)
        }
    }
}

fn index_storage_size(root: &Path, index_path: &Path) -> u64 {
    let base_size = fs::metadata(index_path)
        .ok()
        .map(|meta| meta.len())
        .unwrap_or(0);
    let delta_size = delta_files(root)
        .unwrap_or_default()
        .into_iter()
        .filter_map(|path| fs::metadata(path).ok().map(|meta| meta.len()))
        .sum::<u64>();
    base_size + delta_size
}

fn command_search(args: &[String]) -> Result<i32> {
    let total_timer = Instant::now();
    let parse_timer = Instant::now();
    let options = parse_search_args(args)?;
    let mut profile = SearchProfile::default();
    if options.profile {
        profile.record("client_parse_args", parse_timer.elapsed());
    }
    let start_timer = Instant::now();
    let start = options
        .paths
        .first()
        .map(PathBuf::from)
        .unwrap_or(std::env::current_dir()?);
    if options.profile {
        profile.record("client_resolve_start_path", start_timer.elapsed());
    }
    if !options.auto_update
        && let Some(code) = try_search_multiple_roots(args, &options, &mut profile, total_timer)?
    {
        return Ok(code);
    }
    if !options.auto_update {
        let find_timer = Instant::now();
        if let Some(root) = find_existing_index_root(&start) {
            if options.profile {
                profile.record("client_find_index_root", find_timer.elapsed());
            }
            return try_search_daemon(
                &root,
                &daemon_search_args(args),
                if options.profile {
                    Some(&mut profile)
                } else {
                    None
                },
                total_timer,
            );
        } else if options.profile {
            profile.record("client_find_index_root", find_timer.elapsed());
        }
    }
    if options.auto_index && find_existing_project_root(&start).is_none() {
        if !confirm_create_project_for_search(&start)? {
            return Ok(1);
        }
    }
    let config_timer = Instant::now();
    let cfg = if options.auto_index {
        load_or_create_config(&start)?
    } else {
        load_config(&start)?
    };
    if options.profile {
        profile.record("client_load_config", config_timer.elapsed());
    }
    if options.auto_update {
        let update_timer = Instant::now();
        refresh_index_for_search(&cfg, &options)?;
        if options.profile {
            profile.record("client_auto_update", update_timer.elapsed());
        }
    }
    log_search_compat_notes(&cfg.root, &options);
    let lock_timer = Instant::now();
    let _lock = acquire_shared_lock(&cfg.root)?;
    if options.profile {
        profile.record("client_acquire_shared_lock", lock_timer.elapsed());
    }
    let path = index_path(&cfg.root);
    let open_timer = Instant::now();
    let mut index = MappedIndex::open(&path);
    if options.profile {
        profile.record("client_open_index_mmap", open_timer.elapsed());
    }
    if index
        .as_ref()
        .map(|i| i.config_hash != cfg.hash)
        .unwrap_or(true)
    {
        if !options.auto_index {
            eprintln!(
                "indexsearch: index not found or stale: {}",
                display_path(&path)
            );
            return Ok(2);
        }
        drop(index);
        let flow = ConsoleFlow::start();
        flow.step_done(format!("Resolved project {}", display_path(&cfg.root)));
        let progress = ProgressLine::start("Indexing code");
        let mut scanned = 0;
        let mut skipped = 0;
        let built = build_index(
            &cfg,
            &options,
            &mut scanned,
            &mut skipped,
            None,
            progress.as_active(),
        )?;
        progress.set_indeterminate("Writing index");
        save_index_with_progress(&built, &path, progress.as_active())?;
        remove_delta_dir(&cfg.root)?;
        save_index_state(&cfg.root)?;
        let index_size = index_storage_size(&cfg.root, &path);
        progress.finish("Indexed code");
        flow.summary(format!(
            "Indexed {} files",
            format_count(built.files.len() as u64)
        ));
        flow.detail(format!(
            "{} scanned, {} skipped",
            format_count(scanned),
            format_count(skipped)
        ));
        flow.detail(format!("index size {}", format_bytes(index_size)));
        flow.done();
        std::mem::forget(built);
        index = MappedIndex::open(&path);
    }
    let index = index?;
    run_search_with_index(index, &options, Some(profile), total_timer)
}

struct BackendSearchPathArg {
    arg_index: Option<usize>,
    abs: PathBuf,
}

struct BackendSearchGroup {
    root: PathBuf,
    args: Vec<String>,
}

fn try_search_multiple_roots(
    args: &[String],
    _options: &Options,
    _profile: &mut SearchProfile,
    total_timer: Instant,
) -> Result<Option<i32>> {
    let Some(path_args) = search_path_args_from_cli(args) else {
        return Ok(None);
    };
    if path_args.len() < 2 {
        return Ok(None);
    }
    let mut groups: Vec<(PathBuf, Vec<Option<usize>>)> = Vec::new();
    for path in &path_args {
        let Some(root) = find_existing_index_root(&path.abs) else {
            bail!(
                "no IndexSearch project found above {}; run `istool index` in that project first",
                display_path(&path.abs)
            );
        };
        if let Some((_, indexes)) = groups
            .iter_mut()
            .find(|(group_root, _)| same_clean_root(group_root, &root))
        {
            indexes.push(path.arg_index);
        } else {
            groups.push((root, vec![path.arg_index]));
        }
    }
    if groups.len() < 2 {
        return Ok(None);
    }

    let mut final_code = 1;
    for group in groups
        .into_iter()
        .map(|(root, indexes)| BackendSearchGroup {
            root,
            args: search_args_for_backend_group(args, &indexes),
        })
    {
        let daemon_args = daemon_search_args(&group.args);
        let code = try_search_daemon(&group.root, &daemon_args, None, total_timer)?;
        final_code = combine_search_exit_codes(final_code, code);
    }
    Ok(Some(final_code))
}

fn combine_search_exit_codes(current: i32, next: i32) -> i32 {
    if current > 1 || next > 1 {
        current.max(next)
    } else if current == 0 || next == 0 {
        0
    } else {
        1
    }
}

fn search_path_args_from_cli(args: &[String]) -> Option<Vec<BackendSearchPathArg>> {
    let mut after_double_dash = false;
    let mut saw_pattern = false;
    let mut files_mode = false;
    let mut paths = Vec::new();
    let mut i = 0usize;
    while i < args.len() {
        let arg = &args[i];
        if after_double_dash {
            if saw_pattern {
                paths.push((i, arg.clone()));
            } else {
                saw_pattern = true;
            }
            i += 1;
            continue;
        }
        if arg == "--" {
            after_double_dash = true;
            i += 1;
            continue;
        }
        if arg == "--files" {
            files_mode = true;
            i += 1;
            continue;
        }
        if arg == "-e" || arg == "--regexp" {
            saw_pattern = true;
            i += 2;
            continue;
        }
        if search_option_takes_value(arg) {
            i += 2;
            continue;
        }
        if arg.starts_with("--") {
            i += 1;
            continue;
        }
        if search_short_option_with_attached_value(arg) || (arg.starts_with('-') && arg.len() > 1) {
            i += 1;
            continue;
        }
        if files_mode || saw_pattern {
            paths.push((i, arg.clone()));
        } else {
            saw_pattern = true;
        }
        i += 1;
    }
    let cwd = env::current_dir().ok()?;
    if paths.is_empty() {
        return Some(vec![BackendSearchPathArg {
            arg_index: None,
            abs: cwd,
        }]);
    }
    Some(
        paths
            .into_iter()
            .map(|(arg_index, raw)| {
                let path = PathBuf::from(&raw);
                let abs = if path.is_absolute() {
                    path
                } else {
                    cwd.join(path)
                };
                BackendSearchPathArg {
                    arg_index: Some(arg_index),
                    abs,
                }
            })
            .collect(),
    )
}

fn search_args_for_backend_group(
    args: &[String],
    keep_path_indexes: &[Option<usize>],
) -> Vec<String> {
    let explicit_indexes: Vec<usize> = keep_path_indexes.iter().filter_map(|idx| *idx).collect();
    if explicit_indexes.is_empty() {
        return args.to_vec();
    }
    let all_path_indexes = search_path_args_from_cli(args)
        .unwrap_or_default()
        .into_iter()
        .filter_map(|path| path.arg_index)
        .collect::<Vec<_>>();
    args.iter()
        .enumerate()
        .filter_map(|(idx, arg)| {
            if all_path_indexes.contains(&idx) && !explicit_indexes.contains(&idx) {
                None
            } else {
                Some(arg.clone())
            }
        })
        .collect()
}

fn search_option_takes_value(arg: &str) -> bool {
    matches!(
        arg,
        "-g" | "--glob"
            | "-A"
            | "--after-context"
            | "-B"
            | "--before-context"
            | "-C"
            | "--context"
            | "-m"
            | "--max-count"
            | "--max-filesize"
            | "--color"
            | "--colors"
            | "-j"
            | "--threads"
            | "--sort"
            | "--sortr"
            | "--engine"
            | "--encoding"
            | "-t"
            | "--type"
            | "-T"
            | "--type-not"
            | "--max-depth"
            | "--ignore-file"
            | "--pre"
            | "--pre-glob"
            | "--replace"
            | "--path-separator"
            | "--field-context-separator"
            | "--field-match-separator"
            | "--context-separator"
            | "--dfa-size-limit"
            | "--hyperlink-format"
            | "--hostname-bin"
            | "--max-columns"
            | "--regex-size-limit"
            | "--type-add"
            | "--type-clear"
    )
}

fn search_short_option_with_attached_value(arg: &str) -> bool {
    (arg.starts_with("-A")
        || arg.starts_with("-B")
        || arg.starts_with("-C")
        || arg.starts_with("-g")
        || arg.starts_with("-m")
        || arg.starts_with("-t")
        || arg.starts_with("-T")
        || arg.starts_with("-j"))
        && arg.len() > 2
}

fn daemon_search_args(args: &[String]) -> Vec<String> {
    let resolved_color = if stdout_supports_color() {
        "always"
    } else {
        "never"
    };
    let decorated_output = stdout_supports_search_decoration();
    let mut out = Vec::with_capacity(args.len() + 3);
    let mut saw_color = false;
    let mut saw_heading = false;
    let mut saw_line_number = false;
    let mut i = 0usize;
    while i < args.len() {
        let arg = &args[i];
        if arg == "--color" {
            saw_color = true;
            out.push(arg.clone());
            if let Some(value) = args.get(i + 1) {
                out.push(if value == "auto" {
                    resolved_color.to_string()
                } else {
                    value.clone()
                });
                i += 2;
                continue;
            }
        } else if let Some(value) = arg.strip_prefix("--color=") {
            saw_color = true;
            out.push(if value == "auto" {
                format!("--color={resolved_color}")
            } else {
                arg.clone()
            });
            i += 1;
            continue;
        } else if arg == "--heading" || arg == "--no-heading" {
            saw_heading = true;
        } else if matches!(
            arg.as_str(),
            "-n" | "--line-number" | "-N" | "--no-line-number"
        ) {
            saw_line_number = true;
        }
        out.push(arg.clone());
        i += 1;
    }
    let mut defaults = Vec::with_capacity(3);
    if !saw_color {
        defaults.push(format!("--color={resolved_color}"));
    }
    if !saw_heading {
        defaults.push(
            if decorated_output {
                "--heading"
            } else {
                "--no-heading"
            }
            .to_string(),
        );
    }
    if !saw_line_number {
        defaults.push(
            if decorated_output {
                "--line-number"
            } else {
                "--no-line-number"
            }
            .to_string(),
        );
    }
    insert_before_double_dash(&mut out, defaults);
    out
}

fn insert_before_double_dash(args: &mut Vec<String>, defaults: Vec<String>) {
    if defaults.is_empty() {
        return;
    }
    if let Some(pos) = args.iter().position(|arg| arg == "--") {
        args.splice(pos..pos, defaults);
    } else {
        args.extend(defaults);
    }
}

fn run_search_with_index(
    index: MappedIndex,
    options: &Options,
    profile: Option<SearchProfile>,
    total_timer: Instant,
) -> Result<i32> {
    let output = search_output_for_options(&index, options)?;
    let write_timer = Instant::now();
    let stdout = std::io::stdout();
    let mut out = BufWriter::new(stdout.lock());
    out.write_all(&output.stdout)?;
    out.flush()?;
    let stderr = std::io::stderr();
    let mut err = BufWriter::new(stderr.lock());
    err.write_all(&output.stderr)?;
    err.flush()?;
    if let Some(mut profile) = profile {
        if options.profile {
            profile.record("client_write_output", write_timer.elapsed());
            profile.record("client_search_command_total", total_timer.elapsed());
            write_profile_events(&profile)?;
        }
    }
    Ok(output.code)
}

fn search_output_for_options(index: &MappedIndex, options: &Options) -> Result<SearchOutput> {
    if quiet_search_fast_path(options) {
        return search_quiet_with_index_output(index, options);
    }
    search_with_index_output(index, options)
}

fn quiet_search_fast_path(options: &Options) -> bool {
    options.quiet && !options.stats && !options.files
}

fn files_with_matches_can_stop(options: &Options) -> bool {
    options.files_with_matches && !options.count && !options.stats
}

#[cold]
#[inline(never)]
fn search_quiet_with_index_output(index: &MappedIndex, options: &Options) -> Result<SearchOutput> {
    let mut profile = SearchProfile::default();
    let delta_timer = if options.profile {
        Some(Instant::now())
    } else {
        None
    };
    let deltas = load_deltas(&index.root)?;
    if let Some(delta_timer) = delta_timer {
        profile.record("search_load_deltas", delta_timer.elapsed());
    }
    let execute_timer = if options.profile {
        Some(Instant::now())
    } else {
        None
    };
    let found = execute_search_any_segments(index, &deltas, options)?;
    let mut stderr = Vec::new();
    if let Some(execute_timer) = execute_timer {
        profile.record("search_execute_any_segments", execute_timer.elapsed());
        append_profile_events(&mut stderr, &profile)?;
    }
    Ok(SearchOutput {
        code: if found { 0 } else { 1 },
        stdout: Vec::new(),
        stderr,
    })
}

fn search_with_index_output(index: &MappedIndex, options: &Options) -> Result<SearchOutput> {
    let mut profile = SearchProfile::default();
    let delta_timer = if options.profile {
        Some(Instant::now())
    } else {
        None
    };
    let deltas = load_deltas(&index.root)?;
    if let Some(delta_timer) = delta_timer {
        profile.record("search_load_deltas", delta_timer.elapsed());
    }
    if options.files {
        let render_timer = if options.profile {
            Some(Instant::now())
        } else {
            None
        };
        let stdout = if options.quiet {
            Vec::new()
        } else {
            render_visible_files(index, &deltas, options)?
        };
        let mut stderr = Vec::new();
        if let Some(render_timer) = render_timer {
            profile.record("search_render_files", render_timer.elapsed());
            append_profile_events(&mut stderr, &profile)?;
        }
        return Ok(SearchOutput {
            code: 0,
            stdout,
            stderr,
        });
    }
    let timer = Instant::now();
    let mut searched = 0;
    let execute_timer = if options.profile {
        Some(Instant::now())
    } else {
        None
    };
    let results = execute_search_rendered_segments(index, &deltas, options, &mut searched)?;
    if let Some(execute_timer) = execute_timer {
        profile.record(
            "search_execute_and_render_segments",
            execute_timer.elapsed(),
        );
    }
    let output_timer = if options.profile {
        Some(Instant::now())
    } else {
        None
    };
    let mut stdout = Vec::new();
    if !options.quiet {
        write_rendered_results(&mut stdout, &results, options)?;
    }
    if let Some(output_timer) = output_timer {
        profile.record("search_collect_stdout", output_timer.elapsed());
    }
    let mut stderr = Vec::new();
    if options.stats {
        let match_count: usize = results.iter().map(|r| r.match_count).sum();
        writeln!(stderr, "{match_count} matches")?;
        writeln!(stderr, "{} matched files", results.len())?;
        writeln!(stderr, "{searched} candidate files")?;
        writeln!(stderr, "{}", format_elapsed_duration(timer.elapsed()))?;
    }
    if options.profile {
        append_profile_events(&mut stderr, &profile)?;
    }
    Ok(SearchOutput {
        code: if results.is_empty() { 1 } else { 0 },
        stdout,
        stderr,
    })
}

fn find_existing_index_root(start: &Path) -> Option<PathBuf> {
    let path = if start.is_absolute() {
        start.to_path_buf()
    } else {
        std::env::current_dir().ok()?.join(start)
    };
    path.ancestors()
        .find(|ancestor| index_path(ancestor).is_file())
        .map(Path::to_path_buf)
}

fn find_existing_project_root(start: &Path) -> Option<PathBuf> {
    let path = if start.is_absolute() {
        start.to_path_buf()
    } else {
        std::env::current_dir().ok()?.join(start)
    };
    path.ancestors()
        .find(|ancestor| project_marker_exists(ancestor))
        .map(Path::to_path_buf)
}

fn confirm_create_project_for_search(start: &Path) -> Result<bool> {
    let root = discover_root_for_create(start)?;
    if agent_auto_project_mode() {
        let _ = load_or_create_config(&root)?;
        return Ok(true);
    }
    if !(std::io::stdin().is_terminal() && std::io::stderr().is_terminal()) {
        eprintln!(
            "istool: no IndexSearch project found above {}; run `istool index .` at the project root",
            display_path(start)
        );
        return Ok(false);
    }
    eprint!(
        "istool: no IndexSearch project found above {}. Create one at {}? [Y/n] ",
        display_path(start),
        display_path(&root)
    );
    let _ = std::io::stderr().flush();
    let mut answer = String::new();
    if std::io::stdin().read_line(&mut answer).is_err() {
        return Ok(false);
    }
    match answer.trim() {
        "" | "y" | "Y" | "yes" | "YES" => {
            let _ = load_or_create_config(&root)?;
            Ok(true)
        }
        "n" | "N" | "no" | "NO" => Ok(false),
        _ => Ok(false),
    }
}

fn agent_auto_project_mode() -> bool {
    if env::var_os("INDEXSEARCH_NO_AGENT_AUTO_PROJECT").is_some() {
        return false;
    }
    env::var_os("INDEXSEARCH_AGENT_AUTO_PROJECT").is_some()
        || !(std::io::stdin().is_terminal() && std::io::stderr().is_terminal())
}

fn try_search_daemon(
    root: &Path,
    args: &[String],
    mut profile: Option<&mut SearchProfile>,
    total_timer: Instant,
) -> Result<i32> {
    let mut last_error: Option<String> = None;
    if let Some(record) = read_valid_search_daemon_record(root, profile.as_deref_mut())? {
        if record.supports_search() {
            match request_search_daemon(&record, args, profile.as_deref_mut()) {
                Ok(code) => {
                    if let Some(profile) = profile.as_deref_mut() {
                        profile.record("client_search_command_total", total_timer.elapsed());
                        write_profile_events(profile)?;
                    }
                    return Ok(code);
                }
                Err(err) => {
                    last_error = Some(format!("{err:#}"));
                }
            }
        }
        stop_process(record.pid);
        let _ = fs::remove_file(search_daemon_record_path(root));
    }
    let start_timer = Instant::now();
    start_search_daemon(root)?;
    if let Some(profile) = profile.as_deref_mut() {
        profile.record("client_start_daemon", start_timer.elapsed());
    }
    let start = Instant::now();
    while start.elapsed() < SEARCH_DAEMON_START_TIMEOUT {
        if let Some(record) = read_valid_search_daemon_record(root, profile.as_deref_mut())? {
            if !record.supports_search() {
                std::thread::sleep(Duration::from_millis(10));
                continue;
            }
            match request_search_daemon(&record, args, profile.as_deref_mut()) {
                Ok(code) => {
                    if let Some(profile) = profile.as_deref_mut() {
                        profile.record("client_search_command_total", total_timer.elapsed());
                        write_profile_events(profile)?;
                    }
                    return Ok(code);
                }
                Err(err) => {
                    last_error = Some(format!("{err:#}"));
                }
            }
            stop_process(record.pid);
            let _ = fs::remove_file(search_daemon_record_path(root));
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    if let Some(err) = last_error {
        bail!(
            "project service failed for {} after restart: {err}",
            display_path(root)
        );
    }
    bail!(
        "project service did not become ready for {}",
        display_path(root)
    )
}

fn try_update_via_daemon(cfg: &ProjectConfig) -> Result<Option<i32>> {
    let Some(record) = read_valid_search_daemon_record(&cfg.root, None)? else {
        return Ok(None);
    };
    if !record.supports_update() {
        return Ok(None);
    }
    let args = vec![
        SEARCH_DAEMON_CONTROL_ARG.to_string(),
        SEARCH_DAEMON_CONTROL_UPDATE.to_string(),
    ];
    match request_search_daemon(&record, &args, None) {
        Ok(code) => Ok(Some(code)),
        Err(err) => {
            stop_process(record.pid);
            let _ = fs::remove_file(search_daemon_record_path(&cfg.root));
            bail!(
                "project service update failed for {}: {err:#}",
                display_path(&cfg.root)
            )
        }
    }
}

fn write_profile_events(profile: &SearchProfile) -> Result<()> {
    let stderr = std::io::stderr();
    let mut err = BufWriter::new(stderr.lock());
    append_profile_events(&mut err, profile)?;
    err.flush()?;
    Ok(())
}

fn append_profile_events<W: Write>(out: &mut W, profile: &SearchProfile) -> Result<()> {
    for (name, ms) in &profile.events {
        writeln!(out, "profile: {name}={}", format_elapsed_millis(*ms))?;
    }
    Ok(())
}

fn start_search_daemon(root: &Path) -> Result<()> {
    let exe = search_daemon_executable()?;
    let mut command = Command::new(exe);
    command
        .arg("search-daemon")
        .arg("--detach")
        .arg(root)
        .env("INDEXSEARCH_NO_PROGRESS", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let status = command.status()?;
    if !status.success() {
        bail!("failed to start project service for {}", display_path(root));
    }
    Ok(())
}

fn start_search_daemon_from_current_index(root: &Path) -> Result<()> {
    spawn_search_daemon_detached(root, WatchOptions::default())?;
    wait_for_search_daemon_ready(root)
}

fn wait_for_search_daemon_ready(root: &Path) -> Result<()> {
    let start = Instant::now();
    while start.elapsed() < SEARCH_DAEMON_START_TIMEOUT {
        if let Some(record) = read_valid_search_daemon_record(root, None)? {
            if record.supports_search() {
                return Ok(());
            }
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    bail!(
        "project service did not become ready for {}",
        display_path(root)
    )
}

fn refresh_or_start_search_daemon_after_index(root: &Path) -> Result<()> {
    refresh_search_daemon_after_base_index_replaced(root, true)
}

fn refresh_search_daemon_after_base_index_replaced(
    root: &Path,
    start_if_missing: bool,
) -> Result<()> {
    if let Some(record) = read_valid_search_daemon_record(root, None)? {
        if record.supports_update() {
            let args = vec![
                SEARCH_DAEMON_CONTROL_ARG.to_string(),
                SEARCH_DAEMON_CONTROL_UPDATE.to_string(),
            ];
            match request_search_daemon_quiet(&record, &args) {
                Ok(0) => return Ok(()),
                Ok(code) => {
                    let _ = append_project_log(
                        root,
                        &format!("project-service-refresh-failed code={code}"),
                    );
                }
                Err(err) => {
                    let _ = append_project_log(
                        root,
                        &format!(
                            "project-service-refresh-failed error={}",
                            log_quote(&format!("{err:#}"), 512)
                        ),
                    );
                }
            }
        }
        stop_process(record.pid);
        let _ = fs::remove_file(search_daemon_record_path(root));
    }
    if start_if_missing {
        start_search_daemon_from_current_index(root)?;
    }
    Ok(())
}

fn command_search_daemon(args: &[String]) -> Result<i32> {
    let mut detach = false;
    let mut skip_startup_sync = false;
    let mut watch_options = WatchOptions::default();
    let mut root = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--detach" => detach = true,
            SEARCH_DAEMON_SKIP_STARTUP_SYNC_ARG => skip_startup_sync = true,
            "--idle-seconds" => {
                i += 1;
                let value = args.get(i).context("missing --idle-seconds value")?;
                watch_options.idle_seconds = value.parse()?;
            }
            "--compact-delta-count" => {
                i += 1;
                let value = args.get(i).context("missing --compact-delta-count value")?;
                watch_options.compact_delta_count = value.parse()?;
            }
            "--compact-delta-bytes" => {
                i += 1;
                let value = args.get(i).context("missing --compact-delta-bytes value")?;
                watch_options.compact_delta_bytes = parse_size(value)?;
            }
            value => root = Some(PathBuf::from(value)),
        }
        i += 1;
    }
    let root = root.context("search-daemon requires a root")?;
    let cfg = load_or_create_config(&root)?;
    stop_child_project_services(&cfg.root)?;
    if !skip_startup_sync {
        let flow = ConsoleFlow::start();
        flow.step_done(format!("Resolved project {}", display_path(&cfg.root)));
        sync_index_before_service(&cfg, &flow)?;
        flow.done();
    }
    if detach {
        spawn_search_daemon_detached(&cfg.root, watch_options)?;
        return Ok(0);
    }
    run_search_daemon(&cfg.root, watch_options)
}

fn search_daemon_child_args(root: &Path, options: WatchOptions) -> Vec<OsString> {
    vec![
        OsString::from("search-daemon"),
        root.as_os_str().to_os_string(),
        OsString::from(SEARCH_DAEMON_SKIP_STARTUP_SYNC_ARG),
        OsString::from("--idle-seconds"),
        OsString::from(options.idle_seconds.to_string()),
        OsString::from("--compact-delta-count"),
        OsString::from(options.compact_delta_count.to_string()),
        OsString::from("--compact-delta-bytes"),
        OsString::from(options.compact_delta_bytes.to_string()),
    ]
}

fn spawn_search_daemon_detached(root: &Path, options: WatchOptions) -> Result<()> {
    let exe = search_daemon_executable()?;
    let args = search_daemon_child_args(root, options);
    #[cfg(windows)]
    {
        spawn_detached_no_inherit(&exe, &args)?;
    }
    #[cfg(not(windows))]
    {
        let mut command = Command::new(exe);
        command
            .args(&args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        detach_background(&mut command);
        command.spawn()?;
    }
    Ok(())
}

fn search_daemon_executable() -> Result<PathBuf> {
    let exe = env::current_exe()?;
    if exe
        .file_stem()
        .and_then(|stem| stem.to_str())
        .is_some_and(|stem| stem == "is-daemon" || stem.starts_with("is-daemon-"))
    {
        return Ok(exe);
    }
    if let Some(dir) = exe.parent() {
        let versioned = dir.join(executable_name(&format!(
            "is-daemon-{}",
            env!("CARGO_PKG_VERSION")
        )));
        if versioned.is_file() {
            return Ok(versioned);
        }
        let daemon = dir.join(executable_name("is-daemon"));
        if daemon.is_file() {
            return Ok(daemon);
        }
    }
    Ok(exe)
}

#[cfg(unix)]
fn detach_background(command: &mut Command) {
    use std::os::unix::process::CommandExt;
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
}

#[cfg(not(any(unix, windows)))]
fn detach_background(_command: &mut Command) {}

enum SearchDaemonListener {
    #[cfg(not(unix))]
    Tcp(TcpListener),
    #[cfg(unix)]
    Unix {
        listener: UnixListener,
        path: PathBuf,
    },
}

impl SearchDaemonListener {
    fn bind(root: &Path) -> Result<Self> {
        #[cfg(unix)]
        {
            let path = search_daemon_socket_path(root);
            let _ = fs::remove_file(&path);
            let listener = UnixListener::bind(&path)?;
            return Ok(Self::Unix { listener, path });
        }
        #[cfg(not(unix))]
        {
            let _ = root;
            Ok(Self::Tcp(TcpListener::bind(("127.0.0.1", 0))?))
        }
    }

    fn port(&self) -> u16 {
        match self {
            #[cfg(not(unix))]
            Self::Tcp(listener) => listener.local_addr().map(|addr| addr.port()).unwrap_or(0),
            #[cfg(unix)]
            Self::Unix { .. } => 0,
        }
    }

    fn socket_path(&self) -> Option<PathBuf> {
        match self {
            #[cfg(not(unix))]
            Self::Tcp(_) => None,
            #[cfg(unix)]
            Self::Unix { path, .. } => Some(path.clone()),
        }
    }

    fn set_nonblocking(&self, nonblocking: bool) -> Result<()> {
        match self {
            #[cfg(not(unix))]
            Self::Tcp(listener) => listener.set_nonblocking(nonblocking)?,
            #[cfg(unix)]
            Self::Unix { listener, .. } => listener.set_nonblocking(nonblocking)?,
        }
        Ok(())
    }

    fn accept(&self) -> Result<SearchDaemonStream> {
        match self {
            #[cfg(not(unix))]
            Self::Tcp(listener) => {
                let (stream, _) = listener.accept()?;
                stream.set_nonblocking(false)?;
                Ok(SearchDaemonStream::Tcp(stream))
            }
            #[cfg(unix)]
            Self::Unix { listener, .. } => {
                let (stream, _) = listener.accept()?;
                stream.set_nonblocking(false)?;
                Ok(SearchDaemonStream::Unix(stream))
            }
        }
    }
}

enum SearchDaemonStream {
    Tcp(TcpStream),
    #[cfg(unix)]
    Unix(UnixStream),
}

impl SearchDaemonStream {
    fn connect(record: &SearchDaemonRecord) -> Result<Self> {
        #[cfg(unix)]
        {
            if let Some(path) = record.socket_path.as_ref() {
                return Ok(Self::Unix(UnixStream::connect(path)?));
            }
        }
        let addr = std::net::SocketAddr::from(([127, 0, 0, 1], record.port));
        let stream = TcpStream::connect_timeout(&addr, SEARCH_DAEMON_CONNECT_TIMEOUT)?;
        stream.set_nodelay(true)?;
        Ok(Self::Tcp(stream))
    }

    #[cfg(unix)]
    fn send_stdout_fd(&mut self, _record: &SearchDaemonRecord) -> std::io::Result<()> {
        match self {
            Self::Unix(stream) => send_fd(stream, std::io::stdout().as_raw_fd()),
            Self::Tcp(_) => Ok(()),
        }
    }

    #[cfg(windows)]
    fn send_stdout_fd(&mut self, _record: &SearchDaemonRecord) -> std::io::Result<()> {
        match self {
            Self::Tcp(stream) => stream.write_all(&0u64.to_le_bytes()),
        }
    }

    #[cfg(not(any(unix, windows)))]
    fn send_stdout_fd(&mut self, _record: &SearchDaemonRecord) -> std::io::Result<()> {
        Ok(())
    }

    #[cfg(unix)]
    fn receive_stdout_fd(&mut self) -> std::io::Result<Option<RawFd>> {
        match self {
            Self::Unix(stream) => receive_fd(stream).map(Some),
            Self::Tcp(_) => Ok(None),
        }
    }

    #[cfg(windows)]
    fn receive_stdout_fd(&mut self) -> std::io::Result<Option<StdoutFd>> {
        match self {
            Self::Tcp(stream) => {
                let mut bytes = [0u8; 8];
                stream.read_exact(&mut bytes)?;
                let raw = u64::from_le_bytes(bytes);
                Ok((raw != 0).then_some(raw as usize as RawHandle))
            }
        }
    }

    #[cfg(not(any(unix, windows)))]
    fn receive_stdout_fd(&mut self) -> std::io::Result<Option<StdoutFd>> {
        Ok(None)
    }
}

impl Read for SearchDaemonStream {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        match self {
            Self::Tcp(stream) => stream.read(buf),
            #[cfg(unix)]
            Self::Unix(stream) => stream.read(buf),
        }
    }
}

impl Write for SearchDaemonStream {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        match self {
            Self::Tcp(stream) => stream.write(buf),
            #[cfg(unix)]
            Self::Unix(stream) => stream.write(buf),
        }
    }

    fn flush(&mut self) -> std::io::Result<()> {
        match self {
            Self::Tcp(stream) => stream.flush(),
            #[cfg(unix)]
            Self::Unix(stream) => stream.flush(),
        }
    }
}

#[cfg(unix)]
fn send_fd(stream: &UnixStream, fd: RawFd) -> std::io::Result<()> {
    use std::mem;
    use std::ptr;

    let mut byte = [0u8; 1];
    let mut iov = libc::iovec {
        iov_base: byte.as_mut_ptr().cast(),
        iov_len: byte.len(),
    };
    let fd_size = mem::size_of::<RawFd>();
    let mut control = vec![0u8; unsafe { libc::CMSG_SPACE(fd_size as _) } as usize];
    let mut msg: libc::msghdr = unsafe { mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = control.as_mut_ptr().cast();
    msg.msg_controllen = control.len() as _;

    unsafe {
        let cmsg = libc::CMSG_FIRSTHDR(&msg);
        if cmsg.is_null() {
            return Err(std::io::Error::other("missing fd control header"));
        }
        (*cmsg).cmsg_level = libc::SOL_SOCKET;
        (*cmsg).cmsg_type = libc::SCM_RIGHTS;
        (*cmsg).cmsg_len = libc::CMSG_LEN(fd_size as _) as _;
        ptr::copy_nonoverlapping(
            (&fd as *const RawFd).cast::<u8>(),
            libc::CMSG_DATA(cmsg).cast::<u8>(),
            fd_size,
        );
        msg.msg_controllen = (*cmsg).cmsg_len as _;
        if libc::sendmsg(stream.as_raw_fd(), &msg, 0) == -1 {
            return Err(std::io::Error::last_os_error());
        }
    }
    Ok(())
}

#[cfg(unix)]
fn receive_fd(stream: &UnixStream) -> std::io::Result<RawFd> {
    use std::mem;
    use std::ptr;

    let mut byte = [0u8; 1];
    let mut iov = libc::iovec {
        iov_base: byte.as_mut_ptr().cast(),
        iov_len: byte.len(),
    };
    let fd_size = mem::size_of::<RawFd>();
    let mut control = vec![0u8; unsafe { libc::CMSG_SPACE(fd_size as _) } as usize];
    let mut msg: libc::msghdr = unsafe { mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = control.as_mut_ptr().cast();
    msg.msg_controllen = control.len() as _;

    unsafe {
        let received = libc::recvmsg(stream.as_raw_fd(), &mut msg, 0);
        if received == -1 {
            return Err(std::io::Error::last_os_error());
        }
        let cmsg = libc::CMSG_FIRSTHDR(&msg);
        if cmsg.is_null()
            || (*cmsg).cmsg_level != libc::SOL_SOCKET
            || (*cmsg).cmsg_type != libc::SCM_RIGHTS
        {
            return Err(std::io::Error::other("missing stdout fd"));
        }
        let mut fd: RawFd = -1;
        ptr::copy_nonoverlapping(
            libc::CMSG_DATA(cmsg).cast::<u8>(),
            (&mut fd as *mut RawFd).cast::<u8>(),
            fd_size,
        );
        if fd < 0 {
            return Err(std::io::Error::other("invalid stdout fd"));
        }
        Ok(fd)
    }
}

fn run_search_daemon(root: &Path, watch_options: WatchOptions) -> Result<i32> {
    let panic_root = root.to_path_buf();
    std::panic::set_hook(Box::new(move |info| {
        let _ = append_project_log(
            &panic_root,
            &format!(
                "project-service-panic {}",
                clean_log_text(&info.to_string(), 512)
            ),
        );
    }));
    let index_meta = fs::metadata(index_path(root))?;
    let exe = env::current_exe()?;
    let exe_meta = fs::metadata(&exe)?;
    let listener = SearchDaemonListener::bind(root)?;
    let port = listener.port();
    let socket_path = listener.socket_path();
    let token = search_daemon_token(root);
    let record = SearchDaemonRecord {
        service_name: SEARCH_DAEMON_SERVICE_NAME.to_string(),
        protocol: SEARCH_DAEMON_PROTOCOL,
        capabilities: SearchDaemonRecord::current_capabilities(),
        pid: std::process::id(),
        port,
        socket_path,
        token,
        root: root.to_path_buf(),
        exe_path: exe,
        exe_size: exe_meta.len(),
        exe_mtime: mtime_ns(&exe_meta),
        index_size: index_meta.len(),
        index_mtime: mtime_ns(&index_meta),
    };
    write_search_daemon_record(&record)?;
    append_project_log(
        root,
        &format!(
            "project-service-listen pid={} service={} protocol={} capabilities={}",
            record.pid,
            record.service_name,
            record.protocol,
            record.capabilities_text()
        ),
    )?;
    let project = ProjectRecord {
        id: project_id(&record.root),
        root: record.root.clone(),
        pid: record.pid,
    };
    write_project_record(&project)?;
    let index = {
        let _lock = acquire_shared_lock(root)?;
        MappedIndex::open(&index_path(record.root.as_path()))?
    };
    let index_state = Arc::new(RwLock::new(index));
    let shutdown = Arc::new(AtomicBool::new(false));
    let watch_state = Arc::new(WatchState::default());
    let cfg = load_config(root)?;
    let watch_thread = start_embedded_watch_thread(
        cfg,
        watch_options,
        record.clone(),
        Arc::clone(&shutdown),
        Arc::clone(&watch_state),
        Arc::clone(&index_state),
    );
    listener.set_nonblocking(true)?;
    while !shutdown.load(AtomicOrdering::Relaxed)
        && !watch_state.restart_required.load(AtomicOrdering::Relaxed)
    {
        match listener.accept() {
            Ok(mut stream) => {
                let _ = handle_search_daemon_client(
                    &mut stream,
                    &record,
                    Arc::clone(&index_state),
                    watch_state.as_ref(),
                );
            }
            Err(err) => {
                if err
                    .downcast_ref::<std::io::Error>()
                    .is_some_and(|io_err| io_err.kind() == std::io::ErrorKind::WouldBlock)
                {
                    std::thread::sleep(Duration::from_millis(1));
                    continue;
                }
                return Err(err);
            }
        }
    }
    let restart = watch_state.restart_required.load(AtomicOrdering::Relaxed);
    append_project_log(
        root,
        &format!(
            "project-service-main-exit pid={} restart={} shutdown={}",
            record.pid,
            restart,
            shutdown.load(AtomicOrdering::Relaxed)
        ),
    )?;
    shutdown.store(true, AtomicOrdering::Relaxed);
    drop(listener);
    let _ = watch_thread.join();
    let _ = fs::remove_file(search_daemon_record_path(root));
    let _ = fs::remove_file(project_record_path(&project_id(root)));
    if restart {
        let cfg = load_or_create_config(root)?;
        let flow = ConsoleFlow::start();
        flow.step_done(format!("Resolved project {}", display_path(&cfg.root)));
        sync_index_before_service(&cfg, &flow)?;
        flow.done();
        return run_search_daemon(root, watch_options);
    }
    Ok(0)
}

fn handle_search_daemon_client(
    stream: &mut SearchDaemonStream,
    record: &SearchDaemonRecord,
    index_state: Arc<RwLock<MappedIndex>>,
    watch_state: &WatchState,
) -> Result<()> {
    let mut profile = SearchProfile::default();
    let read_timer = Instant::now();
    let cwd = read_search_daemon_request(stream, &record.token)?;
    let mut stdout_fd = stream.receive_stdout_fd().ok().flatten();
    let wants_profile = cwd.args.iter().any(|arg| {
        matches!(
            arg.as_str(),
            "--profile" | "--instrument" | "--profile-search"
        )
    });
    if wants_profile {
        profile.record("daemon_read_request", read_timer.elapsed());
    }
    let current_dir = env::current_dir().ok();
    let cwd_timer = if wants_profile {
        Some(Instant::now())
    } else {
        None
    };
    if let Some(cwd) = cwd.cwd.as_ref() {
        let _ = env::set_current_dir(cwd);
    }
    if let Some(cwd_timer) = cwd_timer {
        profile.record("daemon_set_cwd", cwd_timer.elapsed());
    }
    if is_daemon_update_request(&cwd.args) {
        close_stdout_fd(stdout_fd.take());
        let _io = watch_state.index_io.lock().unwrap();
        let result = handle_search_daemon_update_client(stream, record, &index_state, watch_state);
        if let Some(dir) = current_dir {
            let _ = env::set_current_dir(dir);
        }
        return result;
    }
    let request_timer = Instant::now();
    let request_cwd = cwd
        .cwd
        .as_ref()
        .map(|path| display_path(path))
        .unwrap_or_default();
    let _ = append_project_log(
        &record.root,
        &format!(
            "search-request cwd={} args={}",
            log_quote(&request_cwd, 240),
            format_search_log_args(&cwd.args)
        ),
    );
    let parse_timer = Instant::now();
    let result = parse_search_args(&cwd.args).and_then(|options| {
        if options.auto_update {
            bail!("daemon does not run auto-update searches");
        }
        log_search_compat_notes(&record.root, &options);
        if options.profile {
            profile.record("daemon_parse_args", parse_timer.elapsed());
        }
        let sync_timer = if options.profile {
            Some(Instant::now())
        } else {
            None
        };
        sync_daemon_index_before_search(record, &index_state, watch_state)?;
        if let Some(sync_timer) = sync_timer {
            profile.record("daemon_sync_before_search", sync_timer.elapsed());
        }
        let index = index_state.read().unwrap();
        let search_timer = Instant::now();
        let run = search_daemon_output_to_stream(
            &index,
            &options,
            stream,
            stdout_fd.take(),
            &mut profile,
        )?;
        let code = run.code;
        let _ = append_project_log(
            &record.root,
            &format!(
                "search-result code={} elapsed={} {}",
                code,
                format_elapsed_duration(request_timer.elapsed()),
                run.log_stats
            ),
        );
        if options.profile {
            profile.record("daemon_search_with_index_output", search_timer.elapsed());
            let mut stderr = Vec::new();
            append_profile_events(&mut stderr, &profile)?;
            write_search_daemon_chunk(stream, SEARCH_DAEMON_STDERR_FRAME, &stderr)?;
        }
        write_search_daemon_done(stream, code)?;
        Ok(())
    });
    if let Err(err) = result {
        close_stdout_fd(stdout_fd.take());
        let message = format!("{err:#}");
        let _ = append_project_log(
            &record.root,
            &format!(
                "search-error elapsed={} error={}",
                format_elapsed_duration(request_timer.elapsed()),
                log_quote(&message, 512)
            ),
        );
        write_search_daemon_error(stream, &format!("indexsearch: {message}\n"))?;
    };
    if let Some(dir) = current_dir {
        let _ = env::set_current_dir(dir);
    }
    Ok(())
}

fn is_daemon_update_request(args: &[String]) -> bool {
    args.len() == 2
        && args[0] == SEARCH_DAEMON_CONTROL_ARG
        && args[1] == SEARCH_DAEMON_CONTROL_UPDATE
}

fn sync_daemon_index_before_search(
    _record: &SearchDaemonRecord,
    _index_state: &Arc<RwLock<MappedIndex>>,
    watch_state: &WatchState,
) -> Result<()> {
    if watch_state.restart_required.load(AtomicOrdering::Relaxed) {
        bail!("project configuration changed; project service is restarting");
    }
    Ok(())
}

fn reload_daemon_index_if_changed(
    record: &SearchDaemonRecord,
    index_state: &Arc<RwLock<MappedIndex>>,
    watch_state: &WatchState,
) -> Result<bool> {
    if watch_state.restart_required.load(AtomicOrdering::Relaxed) {
        bail!("project configuration changed; project service is restarting");
    }
    let _lock = acquire_shared_lock(&record.root)?;
    let path = index_path(&record.root);
    let meta = match fs::metadata(&path) {
        Ok(meta) => meta,
        Err(err) => {
            watch_state
                .restart_required
                .store(true, AtomicOrdering::Relaxed);
            bail!("project index unavailable; project service is restarting: {err}");
        }
    };
    let mapped_mtime = mtime_ns(&meta);
    let current_hash = {
        let index = index_state.read().unwrap();
        if index.index_size == meta.len() && index.index_mtime == mapped_mtime {
            return Ok(false);
        }
        index.config_hash
    };
    let new_index = MappedIndex::open(&path)?;
    let cfg = load_config(&record.root)?;
    if new_index.config_hash != cfg.hash || new_index.config_hash != current_hash {
        watch_state
            .restart_required
            .store(true, AtomicOrdering::Relaxed);
        bail!("project configuration changed; project service is restarting");
    }
    let file_count = new_index.file_count;
    let index_size = new_index.index_size;
    {
        let mut index = index_state.write().unwrap();
        if index.index_size == new_index.index_size && index.index_mtime == new_index.index_mtime {
            return Ok(false);
        }
        *index = new_index;
    }
    let _ = refresh_search_daemon_record(record);
    append_project_log(
        &record.root,
        &format!(
            "project-service-reload reason=index-replaced files={} index_size={}",
            file_count, index_size
        ),
    )?;
    Ok(true)
}

fn handle_search_daemon_update_client(
    stream: &mut SearchDaemonStream,
    record: &SearchDaemonRecord,
    index_state: &Arc<RwLock<MappedIndex>>,
    watch_state: &WatchState,
) -> Result<()> {
    let reload_result = reload_daemon_index_if_changed(record, index_state, watch_state);
    write_search_daemon_response_begin(stream)?;
    if let Err(err) = reload_result {
        let message = format!("indexsearch: {err:#}\n");
        write_search_daemon_chunk(stream, SEARCH_DAEMON_STDERR_FRAME, message.as_bytes())?;
        return write_search_daemon_done(stream, 2);
    }
    let cfg = load_config(&record.root)?;
    let config_matches = {
        let index = index_state.read().unwrap();
        cfg.hash == index.config_hash
    };
    if !config_matches {
        watch_state
            .restart_required
            .store(true, AtomicOrdering::Relaxed);
        write_search_daemon_chunk(
            stream,
            SEARCH_DAEMON_STDERR_FRAME,
            b"indexsearch: project configuration changed; project service is restarting\n",
        )?;
        return write_search_daemon_done(stream, 2);
    }
    let timer = Instant::now();
    let outcome = flush_watch_state(&cfg, watch_state)?;
    if outcome.changed {
        let _ = refresh_search_daemon_record(record);
    }
    let elapsed = timer.elapsed().as_secs_f64();
    let message = if outcome.events == 0 {
        format!(
            "updated index (project service current, 0 pending events) in {}\nroot: {}\nindex: {}\n",
            format_elapsed_secs(elapsed),
            display_path(&record.root),
            display_path(&index_path(&record.root))
        )
    } else if outcome.changed {
        format!(
            "updated index from project service ({} pending events flushed) in {}\nroot: {}\nindex: {}\n",
            outcome.events,
            format_elapsed_secs(elapsed),
            display_path(&record.root),
            display_path(&index_path(&record.root))
        )
    } else {
        format!(
            "updated index (project service current, {} pending events produced no index changes) in {}\nroot: {}\nindex: {}\n",
            outcome.events,
            format_elapsed_secs(elapsed),
            display_path(&record.root),
            display_path(&index_path(&record.root))
        )
    };
    append_project_log(
        &record.root,
        &format!(
            "manual-update events={} changed={} elapsed={}",
            outcome.events,
            outcome.changed,
            format_elapsed_secs(elapsed)
        ),
    )?;
    write_search_daemon_chunk(stream, SEARCH_DAEMON_STDOUT_FRAME, message.as_bytes())?;
    write_search_daemon_done(stream, 0)
}

fn search_daemon_output_to_stream(
    index: &MappedIndex,
    options: &Options,
    stream: &mut SearchDaemonStream,
    stdout_fd: Option<StdoutFd>,
    profile: &mut SearchProfile,
) -> Result<SearchDaemonRun> {
    let run_timer = Instant::now();
    if quiet_search_fast_path(options) {
        close_stdout_fd(stdout_fd);
        let output = search_quiet_with_index_output(index, options)?;
        write_search_daemon_response_begin(stream)?;
        if !output.stderr.is_empty() {
            write_search_daemon_chunk(stream, SEARCH_DAEMON_STDERR_FRAME, &output.stderr)?;
        }
        return Ok(SearchDaemonRun {
            code: output.code,
            log_stats: format!(
                "mode=quiet found={} elapsed={}",
                output.code == 0,
                format_elapsed_duration(run_timer.elapsed())
            ),
        });
    }

    let delta_timer = if options.profile {
        Some(Instant::now())
    } else {
        None
    };
    let deltas = load_deltas(&index.root)?;
    if let Some(delta_timer) = delta_timer {
        profile.record("search_load_deltas", delta_timer.elapsed());
    }
    if options.files {
        close_stdout_fd(stdout_fd);
        let render_timer = if options.profile {
            Some(Instant::now())
        } else {
            None
        };
        let stdout = if options.quiet {
            Vec::new()
        } else {
            render_visible_files(index, &deltas, options)?
        };
        write_search_daemon_response_begin(stream)?;
        if !stdout.is_empty() {
            write_search_daemon_chunk(stream, SEARCH_DAEMON_STDOUT_FRAME, &stdout)?;
        }
        if let Some(render_timer) = render_timer {
            profile.record("search_render_files", render_timer.elapsed());
        }
        return Ok(SearchDaemonRun {
            code: 0,
            log_stats: format!(
                "mode=files bytes={} elapsed={}",
                stdout.len(),
                format_elapsed_duration(run_timer.elapsed())
            ),
        });
    }

    let timer = Instant::now();
    let mut searched = 0;
    let execute_timer = if options.profile {
        Some(Instant::now())
    } else {
        None
    };
    let mut stderr = Vec::new();
    write_search_daemon_response_begin(stream)?;
    let stats = if let Some(stdout_fd) = stdout_fd {
        #[cfg(unix)]
        {
            let stdout = unsafe { File::from_raw_fd(stdout_fd) };
            let mut out = BufWriter::with_capacity(64 * 1024, stdout);
            let stats = execute_search_rendered_segments_to_writer(
                index,
                &deltas,
                options,
                &mut searched,
                &mut out,
            )?;
            out.flush()?;
            stats
        }
        #[cfg(windows)]
        {
            let stdout = unsafe { File::from_raw_handle(stdout_fd) };
            let mut out = BufWriter::with_capacity(SEARCH_DAEMON_STREAM_BUFFER_SIZE, stdout);
            let stats = execute_search_rendered_segments_to_writer(
                index,
                &deltas,
                options,
                &mut searched,
                &mut out,
            )?;
            out.flush()?;
            stats
        }
        #[cfg(not(any(unix, windows)))]
        {
            let _ = stdout_fd;
            unreachable!("stdout fd is only available on Unix")
        }
    } else {
        let stdout_writer = SearchDaemonFrameWriter {
            stream,
            tag: SEARCH_DAEMON_STDOUT_FRAME,
        };
        let mut out = BufWriter::with_capacity(SEARCH_DAEMON_STREAM_BUFFER_SIZE, stdout_writer);
        let stats = execute_search_rendered_segments_to_writer(
            index,
            &deltas,
            options,
            &mut searched,
            &mut out,
        )?;
        out.flush()?;
        stats
    };
    if let Some(execute_timer) = execute_timer {
        profile.record("search_execute_render_and_stream", execute_timer.elapsed());
    }
    if options.stats {
        writeln!(stderr, "{} matches", stats.match_count)?;
        writeln!(stderr, "{} matched files", stats.matched_files)?;
        writeln!(stderr, "{searched} candidate files")?;
        writeln!(stderr, "{}", format_elapsed_duration(timer.elapsed()))?;
    }
    if !stderr.is_empty() {
        write_search_daemon_chunk(stream, SEARCH_DAEMON_STDERR_FRAME, &stderr)?;
    }
    let code = if stats.matched_files == 0 { 1 } else { 0 };
    Ok(SearchDaemonRun {
        code,
        log_stats: format!(
            "mode=search matches={} matched_files={} candidates={} elapsed={}",
            stats.match_count,
            stats.matched_files,
            searched,
            format_elapsed_duration(run_timer.elapsed())
        ),
    })
}

fn close_stdout_fd(stdout_fd: Option<StdoutFd>) {
    #[cfg(unix)]
    {
        if let Some(stdout_fd) = stdout_fd {
            unsafe {
                libc::close(stdout_fd);
            }
        }
    }
    #[cfg(windows)]
    {
        if let Some(stdout_fd) = stdout_fd {
            unsafe {
                windows_sys::Win32::Foundation::CloseHandle(
                    stdout_fd as windows_sys::Win32::Foundation::HANDLE,
                );
            }
        }
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = stdout_fd;
    }
}

struct SearchDaemonFrameWriter<'a> {
    stream: &'a mut SearchDaemonStream,
    tag: u8,
}

impl Write for SearchDaemonFrameWriter<'_> {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if !bytes.is_empty() {
            write_search_daemon_chunk(self.stream, self.tag, bytes)
                .map_err(std::io::Error::other)?;
        }
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.stream.flush()
    }
}

struct SearchDaemonRequest {
    cwd: Option<PathBuf>,
    args: Vec<String>,
}

fn request_search_daemon(
    record: &SearchDaemonRecord,
    args: &[String],
    mut profile: Option<&mut SearchProfile>,
) -> Result<i32> {
    let connect_timer = Instant::now();
    let mut stream = SearchDaemonStream::connect(record)?;
    if let Some(profile) = profile.as_deref_mut() {
        profile.record("client_daemon_connect", connect_timer.elapsed());
    }
    let write_timer = Instant::now();
    write_search_daemon_request(&mut stream, record, args)?;
    stream.send_stdout_fd(record)?;
    if let Some(profile) = profile.as_deref_mut() {
        profile.record("client_daemon_write_request", write_timer.elapsed());
    }
    let read_timer = Instant::now();
    let code = read_search_daemon_response_to_stdio(&mut stream)?;
    if let Some(profile) = profile.as_deref_mut() {
        profile.record("client_daemon_read_response", read_timer.elapsed());
    }
    Ok(code)
}

fn request_search_daemon_quiet(record: &SearchDaemonRecord, args: &[String]) -> Result<i32> {
    let mut stream = SearchDaemonStream::connect(record)?;
    write_search_daemon_request(&mut stream, record, args)?;
    stream.send_stdout_fd(record)?;
    read_search_daemon_response_to_sink(&mut stream)
}

fn write_search_daemon_request(
    stream: &mut SearchDaemonStream,
    record: &SearchDaemonRecord,
    args: &[String],
) -> Result<()> {
    stream.write_all(SEARCH_DAEMON_REQUEST_MAGIC)?;
    write_bytes_frame(stream, record.token.as_bytes())?;
    let cwd = env::current_dir()
        .ok()
        .map(|path| path.to_string_lossy().into_owned())
        .unwrap_or_default();
    write_bytes_frame(stream, cwd.as_bytes())?;
    write_u32(stream, args.len() as u32)?;
    for arg in args {
        write_bytes_frame(stream, arg.as_bytes())?;
    }
    stream.flush()?;
    Ok(())
}

fn read_search_daemon_request(
    stream: &mut SearchDaemonStream,
    expected_token: &str,
) -> Result<SearchDaemonRequest> {
    let mut magic = [0u8; 8];
    stream.read_exact(&mut magic)?;
    if &magic != SEARCH_DAEMON_REQUEST_MAGIC {
        bail!("invalid search daemon request");
    }
    let token = read_string_frame(stream)?;
    if token != expected_token {
        bail!("invalid search daemon token");
    }
    let cwd = read_string_frame(stream)?;
    let argc = read_u32_from_reader(stream)? as usize;
    let mut args = Vec::with_capacity(argc);
    for _ in 0..argc {
        args.push(read_string_frame(stream)?);
    }
    Ok(SearchDaemonRequest {
        cwd: (!cwd.is_empty()).then(|| PathBuf::from(cwd)),
        args,
    })
}

fn write_search_daemon_response_begin(stream: &mut SearchDaemonStream) -> Result<()> {
    stream.write_all(SEARCH_DAEMON_RESPONSE_MAGIC)?;
    Ok(())
}

fn write_search_daemon_chunk(stream: &mut SearchDaemonStream, tag: u8, bytes: &[u8]) -> Result<()> {
    if bytes.is_empty() {
        return Ok(());
    }
    stream.write_all(&[tag])?;
    write_bytes_frame(stream, bytes)?;
    stream.flush()?;
    Ok(())
}

fn write_search_daemon_done(stream: &mut SearchDaemonStream, code: i32) -> Result<()> {
    stream.write_all(&[SEARCH_DAEMON_DONE_FRAME])?;
    write_u32(stream, code as u32)?;
    stream.flush()?;
    Ok(())
}

fn write_search_daemon_error(stream: &mut SearchDaemonStream, message: &str) -> Result<()> {
    write_search_daemon_response_begin(stream)?;
    write_search_daemon_chunk(stream, SEARCH_DAEMON_STDERR_FRAME, message.as_bytes())?;
    write_search_daemon_done(stream, 2)
}

fn read_search_daemon_response_to_stdio(stream: &mut SearchDaemonStream) -> Result<i32> {
    let mut magic = [0u8; 8];
    stream.read_exact(&mut magic)?;
    if &magic != SEARCH_DAEMON_RESPONSE_MAGIC {
        bail!("invalid search daemon response");
    }
    let stdout = std::io::stdout();
    let mut out = BufWriter::new(stdout.lock());
    let stderr = std::io::stderr();
    let mut err = BufWriter::new(stderr.lock());
    loop {
        let mut tag = [0u8; 1];
        stream.read_exact(&mut tag)?;
        match tag[0] {
            SEARCH_DAEMON_STDOUT_FRAME => copy_bytes_frame(stream, &mut out)?,
            SEARCH_DAEMON_STDERR_FRAME => copy_bytes_frame(stream, &mut err)?,
            SEARCH_DAEMON_DONE_FRAME => {
                let code = read_u32_from_reader(stream)? as i32;
                out.flush()?;
                err.flush()?;
                return Ok(code);
            }
            _ => bail!("invalid search daemon response frame"),
        }
    }
}

fn read_search_daemon_response_to_sink(stream: &mut SearchDaemonStream) -> Result<i32> {
    let mut magic = [0u8; 8];
    stream.read_exact(&mut magic)?;
    if &magic != SEARCH_DAEMON_RESPONSE_MAGIC {
        bail!("invalid search daemon response");
    }
    let mut sink = std::io::sink();
    loop {
        let mut tag = [0u8; 1];
        stream.read_exact(&mut tag)?;
        match tag[0] {
            SEARCH_DAEMON_STDOUT_FRAME | SEARCH_DAEMON_STDERR_FRAME => {
                copy_bytes_frame(stream, &mut sink)?
            }
            SEARCH_DAEMON_DONE_FRAME => return Ok(read_u32_from_reader(stream)? as i32),
            _ => bail!("invalid search daemon response frame"),
        }
    }
}

fn refresh_index_for_search(cfg: &ProjectConfig, options: &Options) -> Result<()> {
    let _lock = acquire_exclusive_lock(&cfg.root)?;
    let path = index_path(&cfg.root);
    let existing = MappedIndex::open(&path).ok();
    if existing
        .as_ref()
        .map(|index| index.config_hash != cfg.hash)
        .unwrap_or(true)
    {
        let flow = ConsoleFlow::start();
        flow.step_done(format!("Resolved project {}", display_path(&cfg.root)));
        let progress = ProgressLine::start("Indexing code");
        let mut scanned = 0;
        let mut skipped = 0;
        let built = build_index(
            cfg,
            options,
            &mut scanned,
            &mut skipped,
            None,
            progress.as_active(),
        )?;
        progress.set_indeterminate("Writing index");
        save_index_with_progress(&built, &path, progress.as_active())?;
        remove_delta_dir(&cfg.root)?;
        save_index_state(&cfg.root)?;
        let index_size = index_storage_size(&cfg.root, &path);
        progress.finish("Indexed code");
        flow.summary(format!(
            "Indexed {} files",
            format_count(built.files.len() as u64)
        ));
        flow.detail(format!(
            "{} scanned, {} skipped",
            format_count(scanned),
            format_count(skipped)
        ));
        flow.detail(format!("index size {}", format_bytes(index_size)));
        flow.done();
        std::mem::forget(built);
        return Ok(());
    }

    let mut timings = Timings::default();
    let mut scanned = 0;
    let mut skipped = 0;
    let (changes, _) =
        collect_filesystem_changes(cfg, options, &mut scanned, &mut skipped, &mut timings)?;
    if changes.is_empty() {
        save_index_state(&cfg.root)?;
        return Ok(());
    }

    let flow = ConsoleFlow::start();
    flow.step_done(format!("Resolved project {}", display_path(&cfg.root)));
    let progress = ProgressLine::start("Updating index");
    let (delta, meta, stats) =
        build_delta_index(cfg, options, &changes, &mut scanned, &mut skipped, None)?;
    if stats.added != 0 || stats.updated != 0 || stats.removed != 0 {
        progress.update("Writing update");
        save_delta(&cfg.root, &delta, &meta)?;
    }
    save_index_state(&cfg.root)?;
    progress.finish("Updated index");
    flow.summary(format!(
        "Updated {} files",
        format_count((stats.reused + stats.updated + stats.added) as u64)
    ));
    flow.detail(format!(
        "{} reused, {} added, {} modified, {} removed",
        format_count(stats.reused as u64),
        format_count(stats.added as u64),
        format_count(stats.updated as u64),
        format_count(stats.removed as u64)
    ));
    flow.done();
    Ok(())
}

fn note_search_compat(options: &mut Options, flag: &str, detail: &'static str) {
    if options
        .compatibility_notes
        .iter()
        .any(|note| note.flag == flag && note.detail == detail)
    {
        return;
    }
    options.compatibility_notes.push(SearchCompatibilityNote {
        flag: flag.to_string(),
        detail,
    });
}

fn split_arg_value(arg: &str) -> String {
    arg.split_once('=')
        .map(|(_, value)| value.to_string())
        .unwrap_or_default()
}

fn flag_name(arg: &str) -> &str {
    arg.split_once('=').map(|(flag, _)| flag).unwrap_or(arg)
}

fn parse_unrestricted_short_flag(arg: &str) -> Option<usize> {
    let rest = arg.strip_prefix('-')?;
    if rest.is_empty() || rest.starts_with('-') || !rest.bytes().all(|byte| byte == b'u') {
        return None;
    }
    Some(rest.len())
}

fn apply_unrestricted_flag(options: &mut Options, flag: &str, level: usize) {
    if level >= 2 {
        options.hidden = true;
    }
    note_search_compat(
        options,
        flag,
        "unrestricted rg search can only affect paths already present in the IndexSearch index",
    );
}

fn apply_short_flag_cluster(options: &mut Options, arg: &str) -> bool {
    if !arg.starts_with('-') || arg.starts_with("--") || arg.len() <= 2 {
        return false;
    }
    let rest = &arg[1..];
    if rest.starts_with(['A', 'B', 'C', 'e', 'g', 'j', 'm', 't', 'T']) {
        return false;
    }
    for ch in rest.chars() {
        match ch {
            'i' => options.ignore_case = true,
            's' => options.ignore_case = false,
            'S' => options.smart_case = true,
            'F' => options.fixed = true,
            'w' => options.whole_word = true,
            'n' => options.line_number = true,
            'N' => options.line_number = false,
            'H' => options.with_filename = Some(true),
            'I' => options.with_filename = Some(false),
            'l' => options.files_with_matches = true,
            'c' => options.count = true,
            'o' => options.only_matching = true,
            'q' => options.quiet = true,
            'v' => options.invert_match = true,
            'x' => options.line_regexp = true,
            'L' => options.follow = true,
            _ => return false,
        }
    }
    true
}

fn apply_search_encoding_flag(options: &mut Options, flag: &str, value: &str) {
    if !matches!(
        value.to_ascii_lowercase().as_str(),
        "auto" | "utf-8" | "utf8" | "none"
    ) {
        note_search_compat(
            options,
            &format!("{flag}={value}"),
            "non-UTF-8 decoding is not supported; indexed bytes are searched as stored",
        );
    }
}

fn apply_search_engine_flag(options: &mut Options, flag: &str, value: &str) {
    if !matches!(value.to_ascii_lowercase().as_str(), "auto" | "default") {
        note_search_compat(
            options,
            &format!("{flag}={value}"),
            "unsupported regex engine ignored",
        );
    }
}

fn add_search_type_filter(options: &mut Options, flag: &str, value: &str, include: bool) {
    if search_type_globs(value).is_some() {
        if include {
            options.type_includes.push(value.to_string());
        } else {
            options.type_excludes.push(value.to_string());
        }
    } else {
        note_search_compat(
            options,
            &format!("{flag}={value}"),
            "unknown file type ignored",
        );
    }
}

fn search_type_globs(name: &str) -> Option<&'static [&'static str]> {
    match name.to_ascii_lowercase().as_str() {
        "c" => Some(&["*.c", "*.h"]),
        "cpp" | "c++" | "cc" => Some(&[
            "*.cpp", "*.cc", "*.cxx", "*.c++", "*.hpp", "*.hh", "*.hxx", "*.h", "*.inl", "*.ipp",
        ]),
        "cs" | "csharp" => Some(&["*.cs"]),
        "py" | "python" => Some(&["*.py", "*.pyw"]),
        "rust" | "rs" => Some(&["*.rs"]),
        "js" | "javascript" => Some(&["*.js", "*.jsx", "*.mjs", "*.cjs"]),
        "ts" | "typescript" => Some(&["*.ts", "*.tsx", "*.mts", "*.cts"]),
        "json" => Some(&["*.json"]),
        "xml" => Some(&["*.xml"]),
        "md" | "markdown" => Some(&["*.md", "*.markdown"]),
        "toml" => Some(&["*.toml"]),
        "yaml" | "yml" => Some(&["*.yaml", "*.yml"]),
        "ini" => Some(&["*.ini"]),
        "shader" => Some(&["*.usf", "*.ush", "*.hlsl", "*.metal", "*.glsl"]),
        "hlsl" => Some(&["*.hlsl", "*.usf", "*.ush"]),
        "cmake" => Some(&["CMakeLists.txt", "*.cmake"]),
        "build" => Some(&[
            "*.Build.cs",
            "*.Target.cs",
            "CMakeLists.txt",
            "*.cmake",
            "*.bat",
            "*.sh",
            "*.ps1",
        ]),
        "ue" | "unreal" => Some(&[
            "*.Build.cs",
            "*.Target.cs",
            "*.uplugin",
            "*.uproject",
            "*.usf",
            "*.ush",
        ]),
        _ => None,
    }
}

fn print_search_type_list() {
    for (name, globs) in supported_search_types() {
        println!("{name}: {}", globs.join(", "));
    }
}

fn supported_search_types() -> &'static [(&'static str, &'static [&'static str])] {
    &[
        (
            "build",
            &["*.Build.cs", "*.Target.cs", "CMakeLists.txt", "*.cmake"],
        ),
        ("c", &["*.c", "*.h"]),
        (
            "cpp",
            &["*.cpp", "*.cc", "*.cxx", "*.hpp", "*.hh", "*.h", "*.inl"],
        ),
        ("cs", &["*.cs"]),
        ("hlsl", &["*.hlsl", "*.usf", "*.ush"]),
        ("ini", &["*.ini"]),
        ("js", &["*.js", "*.jsx", "*.mjs", "*.cjs"]),
        ("json", &["*.json"]),
        ("markdown", &["*.md", "*.markdown"]),
        ("py", &["*.py", "*.pyw"]),
        ("rust", &["*.rs"]),
        ("shader", &["*.usf", "*.ush", "*.hlsl", "*.metal", "*.glsl"]),
        ("toml", &["*.toml"]),
        ("ts", &["*.ts", "*.tsx", "*.mts", "*.cts"]),
        (
            "ue",
            &[
                "*.Build.cs",
                "*.Target.cs",
                "*.uplugin",
                "*.uproject",
                "*.usf",
                "*.ush",
            ],
        ),
        ("xml", &["*.xml"]),
        ("yaml", &["*.yaml", "*.yml"]),
    ]
}

fn is_known_unsupported_search_flag_with_value(arg: &str) -> bool {
    matches!(
        arg,
        "--context-separator"
            | "--dfa-size-limit"
            | "--hostname-bin"
            | "--regex-size-limit"
            | "--type-add"
            | "--type-clear"
    )
}

fn is_known_unsupported_search_flag(arg: &str) -> bool {
    matches!(
        arg,
        "--binary"
            | "--block-buffered"
            | "--byte-offset"
            | "--debug"
            | "--line-buffered"
            | "--multiline"
            | "--multiline-dotall"
            | "--no-block-buffered"
            | "--no-crlf"
            | "--no-line-buffered"
            | "--no-mmap"
            | "--auto-hybrid-regex"
            | "--mmap"
            | "--one-file-system"
            | "--no-pcre2-unicode"
            | "--null"
            | "--null-data"
            | "--passthru"
            | "--pcre2"
            | "--pcre2-unicode"
            | "--pcre2-version"
            | "--search-zip"
            | "--text"
            | "-a"
            | "-b"
            | "-P"
            | "-U"
            | "-z"
    )
}

fn log_search_compat_notes(root: &Path, options: &Options) {
    if options.compatibility_notes.is_empty() {
        return;
    }
    let mut details = String::new();
    for (idx, note) in options.compatibility_notes.iter().take(24).enumerate() {
        if idx != 0 {
            details.push(' ');
        }
        details.push_str("flag=");
        details.push_str(&log_quote(&note.flag, 120));
        details.push_str(" detail=");
        details.push_str(&log_quote(note.detail, 180));
    }
    if options.compatibility_notes.len() > 24 {
        details.push_str(" more=");
        details.push_str(&(options.compatibility_notes.len() - 24).to_string());
    }
    let _ = append_project_log(root, &format!("search-compat {details}"));
}

fn parse_search_args(args: &[String]) -> Result<Options> {
    let mut options = Options {
        auto_index: true,
        line_number: stdout_supports_search_decoration(),
        max_filesize: DEFAULT_MAX_FILE_SIZE,
        cwd: env::current_dir().unwrap_or_default(),
        ..Options::default()
    };
    let mut regexps = Vec::new();
    let mut i = 0;
    let mut positional_only = false;
    while i < args.len() {
        let arg = &args[i];
        let mut need_value = |name: &str| -> Result<String> {
            i += 1;
            args.get(i)
                .cloned()
                .ok_or_else(|| anyhow!("missing value for {name}"))
        };
        if positional_only {
            if options.pattern.is_empty() && regexps.is_empty() && !options.files {
                options.pattern = arg.clone();
            } else {
                options.paths.push(arg.clone());
            }
            i += 1;
            continue;
        }
        match arg.as_str() {
            "-h" | "--help" => {
                print_search_help();
                std::process::exit(0);
            }
            "-i" | "--ignore-case" => options.ignore_case = true,
            "-s" | "--case-sensitive" => options.ignore_case = false,
            "-S" | "--smart-case" => options.smart_case = true,
            "-F" | "--fixed-strings" => options.fixed = true,
            "-w" | "--word-regexp" => options.whole_word = true,
            "-e" | "--regexp" => regexps.push(need_value(arg)?),
            "-g" | "--glob" => options.globs.push(need_value(arg)?),
            "--iglob" => {
                options.glob_case_insensitive = true;
                options.globs.push(need_value(arg)?);
            }
            "--glob-case-insensitive" => options.glob_case_insensitive = true,
            "--glob-case-sensitive" => options.glob_case_insensitive = false,
            "-n" | "--line-number" => options.line_number = true,
            "-N" | "--no-line-number" => options.line_number = false,
            "--column" => options.column = true,
            "--no-column" => options.column = false,
            "-A" | "--after-context" => {
                options.after_context = parse_context_count(&need_value(arg)?)?
            }
            "-B" | "--before-context" => {
                options.before_context = parse_context_count(&need_value(arg)?)?
            }
            "-C" | "--context" => {
                let value = parse_context_count(&need_value(arg)?)?;
                options.before_context = value;
                options.after_context = value;
            }
            "-H" | "--with-filename" => options.with_filename = Some(true),
            "-I" | "--no-filename" => options.with_filename = Some(false),
            "--heading" => options.heading = Some(true),
            "--no-heading" => options.heading = Some(false),
            "-l" | "--files-with-matches" => options.files_with_matches = true,
            "--files-without-match" => options.files_without_match = true,
            "-c" | "--count" => options.count = true,
            "--count-matches" => {
                options.count = true;
                options.count_matches = true;
            }
            "-o" | "--only-matching" => options.only_matching = true,
            "-v" | "--invert-match" => options.invert_match = true,
            "-x" | "--line-regexp" => options.line_regexp = true,
            "--trim" => options.trim = true,
            "--no-trim" => options.trim = false,
            "-q" | "--quiet" => options.quiet = true,
            "--files" => options.files = true,
            "--json" => options.json = true,
            "--vimgrep" => options.vimgrep = true,
            "--stats" => options.stats = true,
            "--profile" | "--instrument" | "--profile-search" => options.profile = true,
            "--color" => options.color = parse_color_choice(&need_value(arg)?)?,
            "--sort" | "--sortr" => {
                options.sort_path = parse_sort_choice(&need_value(arg)?);
            }
            "--sort-files" => options.sort_path = true,
            "--hidden" => options.hidden = true,
            "--no-hidden" => options.hidden = false,
            "-L" | "--follow" => options.follow = true,
            "--no-follow" => options.follow = false,
            "--no-auto-index" => options.auto_index = false,
            "--auto-update" => options.auto_update = true,
            "--no-daemon" => {
                bail!("unsupported option: --no-daemon; search always uses the project service")
            }
            "--max-depth" => options.max_depth = Some(need_value(arg)?.parse()?),
            "--include-zero" => options.include_zero = true,
            "-t" | "--type" => {
                let value = need_value(arg)?;
                add_search_type_filter(&mut options, arg, &value, true);
            }
            "-T" | "--type-not" => {
                let value = need_value(arg)?;
                add_search_type_filter(&mut options, arg, &value, false);
            }
            "--type-list" => {
                print_search_type_list();
                std::process::exit(0);
            }
            "--ignore-file" => options.ignore_files.push(need_value(arg)?),
            "--ignore-file-case-insensitive" => options.ignore_file_case_insensitive = true,
            "--encoding" => {
                let value = need_value(arg)?;
                apply_search_encoding_flag(&mut options, arg, &value);
            }
            "--engine" => {
                let value = need_value(arg)?;
                apply_search_engine_flag(&mut options, arg, &value);
            }
            "--no-require-git" | "--no-ignore-parent" | "--no-ignore-global" | "--no-config"
            | "--crlf" => {}
            "--no-ignore" | "--no-ignore-vcs" => note_search_compat(
                &mut options,
                arg,
                "ignore overrides are limited by the existing IndexSearch project index",
            ),
            "--" => positional_only = true,
            "--no-messages" => {}
            "--colors" | "-j" | "--threads" => {
                let _ = need_value(arg)?;
            }
            "--pre"
            | "--pre-glob"
            | "--replace"
            | "--path-separator"
            | "--context-separator"
            | "--field-context-separator"
            | "--field-match-separator" => {
                let _ = need_value(arg)?;
                note_search_compat(&mut options, arg, "unsupported option ignored");
            }
            "-m" | "--max-count" => options.max_count = Some(need_value(arg)?.parse()?),
            "--max-filesize" => options.max_filesize = parse_size(&need_value(arg)?)?,
            _ if parse_unrestricted_short_flag(arg).is_some() => {
                apply_unrestricted_flag(
                    &mut options,
                    arg,
                    parse_unrestricted_short_flag(arg).unwrap_or(1),
                );
            }
            _ if apply_short_flag_cluster(&mut options, arg) => {}
            "--unrestricted" => apply_unrestricted_flag(&mut options, arg, 1),
            _ if arg.starts_with("--color=") => {
                options.color = parse_color_choice(
                    arg.split_once('=')
                        .map(|(_, value)| value)
                        .unwrap_or_default(),
                )?
            }
            _ if arg.starts_with("--glob=") => {
                options.globs.push(split_arg_value(arg));
            }
            _ if arg.starts_with("--iglob=") => {
                options.glob_case_insensitive = true;
                options.globs.push(split_arg_value(arg));
            }
            _ if arg.starts_with("--max-count=") => {
                options.max_count = Some(split_arg_value(arg).parse()?);
            }
            _ if arg.starts_with("--max-filesize=") => {
                options.max_filesize = parse_size(&split_arg_value(arg))?;
            }
            _ if arg.starts_with("--max-depth=") => {
                options.max_depth = Some(split_arg_value(arg).parse()?);
            }
            _ if arg.starts_with("--type=") => {
                let value = split_arg_value(arg);
                add_search_type_filter(&mut options, "--type", &value, true);
            }
            _ if arg.starts_with("--type-not=") => {
                let value = split_arg_value(arg);
                add_search_type_filter(&mut options, "--type-not", &value, false);
            }
            _ if arg.starts_with("--ignore-file=") => {
                options.ignore_files.push(split_arg_value(arg));
            }
            _ if arg.starts_with("--encoding=") => {
                apply_search_encoding_flag(&mut options, "--encoding", &split_arg_value(arg));
            }
            _ if arg.starts_with("--engine=") => {
                apply_search_engine_flag(&mut options, "--engine", &split_arg_value(arg));
            }
            _ if arg.starts_with("--colors=")
                || arg.starts_with("--threads=")
                || arg.starts_with("--path-separator=")
                || arg.starts_with("--context-separator=")
                || arg.starts_with("--field-context-separator=")
                || arg.starts_with("--field-match-separator=") =>
            {
                note_search_compat(&mut options, flag_name(arg), "unsupported option ignored");
            }
            _ if arg.starts_with("--hyperlink-format=") || arg.starts_with("--max-columns=") => {
                note_search_compat(&mut options, flag_name(arg), "unsupported option ignored");
            }
            _ if arg.starts_with("--pre=")
                || arg.starts_with("--pre-glob=")
                || arg.starts_with("--replace=") =>
            {
                note_search_compat(&mut options, flag_name(arg), "unsupported option ignored");
            }
            _ if arg.starts_with("--after-context=") => {
                options.after_context = parse_context_count(
                    arg.split_once('=')
                        .map(|(_, value)| value)
                        .unwrap_or_default(),
                )?
            }
            _ if arg.starts_with("--before-context=") => {
                options.before_context = parse_context_count(
                    arg.split_once('=')
                        .map(|(_, value)| value)
                        .unwrap_or_default(),
                )?
            }
            _ if arg.starts_with("--context=") => {
                let value = parse_context_count(
                    arg.split_once('=')
                        .map(|(_, value)| value)
                        .unwrap_or_default(),
                )?;
                options.before_context = value;
                options.after_context = value;
            }
            _ if arg.starts_with("-A") && arg.len() > 2 => {
                options.after_context = parse_context_count(&arg[2..])?
            }
            _ if arg.starts_with("-B") && arg.len() > 2 => {
                options.before_context = parse_context_count(&arg[2..])?
            }
            _ if arg.starts_with("-C") && arg.len() > 2 => {
                let value = parse_context_count(&arg[2..])?;
                options.before_context = value;
                options.after_context = value;
            }
            _ if arg.starts_with("-g") && arg.len() > 2 => {
                options.globs.push(arg[2..].to_string());
            }
            _ if arg.starts_with("-m") && arg.len() > 2 => {
                options.max_count = Some(arg[2..].parse()?);
            }
            _ if arg.starts_with("-t") && arg.len() > 2 => {
                add_search_type_filter(&mut options, "-t", &arg[2..], true);
            }
            _ if arg.starts_with("-T") && arg.len() > 2 => {
                add_search_type_filter(&mut options, "-T", &arg[2..], false);
            }
            _ if arg.starts_with("--sort=") || arg.starts_with("--sortr=") => {
                options.sort_path = parse_sort_choice(
                    arg.split_once('=')
                        .map(|(_, value)| value)
                        .unwrap_or_default(),
                );
            }
            _ if is_known_unsupported_search_flag_with_value(arg) => {
                let _ = need_value(arg)?;
                note_search_compat(&mut options, arg, "unsupported option ignored");
            }
            _ if is_known_unsupported_search_flag(arg) => {
                note_search_compat(&mut options, arg, "unsupported option ignored");
            }
            _ if arg.starts_with('-') => {
                note_search_compat(&mut options, flag_name(arg), "unsupported option ignored");
            }
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
    if options.color == ColorChoice::Auto {
        options.color = if stdout_supports_color() {
            ColorChoice::Always
        } else {
            ColorChoice::Never
        };
    }
    Ok(options)
}

fn parse_color_choice(value: &str) -> Result<ColorChoice> {
    match value {
        "auto" => Ok(if stdout_supports_color() {
            ColorChoice::Always
        } else {
            ColorChoice::Never
        }),
        "always" | "ansi" => Ok(ColorChoice::Always),
        "never" => Ok(ColorChoice::Never),
        _ => bail!("unsupported color mode: {value}"),
    }
}

fn parse_sort_choice(value: &str) -> bool {
    matches!(value, "path")
}

fn parse_context_count(value: &str) -> Result<usize> {
    value
        .parse()
        .with_context(|| format!("invalid context line count: {value}"))
}

fn load_config(start: &Path) -> Result<ProjectConfig> {
    load_config_inner(start, false)
}

fn load_or_create_config(start: &Path) -> Result<ProjectConfig> {
    load_config_inner(start, true)
}

fn load_config_inner(start: &Path, create_default: bool) -> Result<ProjectConfig> {
    let root = if create_default {
        discover_root_for_create(start)?
    } else {
        discover_root(start)?
    };
    if create_default {
        ensure_local_git_excludes_for_project(&root)?;
    }
    let path = project_config_path(&root);
    let legacy_path = legacy_project_config_path(&root);
    let (config_path, text) = if path.exists() {
        let text = fs::read_to_string(&path)?;
        (Some(path.clone()), text)
    } else if legacy_path.exists() {
        let text = fs::read_to_string(&legacy_path)?;
        (Some(legacy_path.clone()), text)
    } else {
        let default_config = if is_unreal_root(&root) {
            EMBEDDED_UE_SKILL_CONFIG
        } else {
            DEFAULT_PROJECT_CONFIG
        };
        if create_default {
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::write(&path, default_config)?;
            eprintln!(
                "indexsearch: created default config: {}",
                display_path(&path)
            );
            (Some(path.clone()), default_config.to_string())
        } else {
            (None, default_config.to_string())
        }
    };
    let has_config = config_path.is_some();
    let path = config_path.unwrap_or(path);
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
        if project_marker_exists(&path) {
            return Ok(path);
        }
        if !path.pop() {
            break;
        }
    }
    Ok(fallback)
}

fn discover_root_for_create(start: &Path) -> Result<PathBuf> {
    let mut path = fs::canonicalize(start).unwrap_or_else(|_| start.to_path_buf());
    if path.is_file() {
        path = path.parent().unwrap_or(Path::new(".")).to_path_buf();
    }
    let fallback = path.clone();
    let mut current = Some(path.as_path());
    while let Some(candidate) = current {
        if project_marker_exists(candidate) {
            return Ok(candidate.to_path_buf());
        }
        current = candidate.parent();
    }
    let mut current = Some(fallback.as_path());
    while let Some(candidate) = current {
        if is_unreal_root(candidate) {
            return Ok(candidate.to_path_buf());
        }
        current = candidate.parent();
    }
    Ok(fallback)
}

fn is_unreal_root(path: &Path) -> bool {
    if path.join("Engine").join("Source").is_dir()
        && (path.join("Engine").join("Build").is_dir()
            || path.join("Engine").join("Config").is_dir())
    {
        return true;
    }
    fs::read_dir(path)
        .ok()
        .into_iter()
        .flat_map(|entries| entries.flatten())
        .any(|entry| {
            entry
                .path()
                .extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("uproject"))
        })
}

fn ensure_local_git_excludes_for_project(root: &Path) -> Result<()> {
    let Some(target) = git_exclude_target(root)? else {
        return Ok(());
    };
    let existing = fs::read_to_string(&target.exclude_path).unwrap_or_default();
    if git_exclude_has_pattern(&existing, &target.pattern) {
        return Ok(());
    }

    if let Some(parent) = target.exclude_path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut updated = existing;
    if !updated.is_empty() && !updated.ends_with('\n') {
        updated.push('\n');
    }
    if !updated
        .lines()
        .any(|line| line.trim() == LOCAL_GIT_EXCLUDE_SECTION)
    {
        if !updated.is_empty() {
            updated.push('\n');
        }
        updated.push_str(LOCAL_GIT_EXCLUDE_SECTION);
        updated.push('\n');
    }
    updated.push_str(&target.pattern);
    updated.push('\n');
    fs::write(target.exclude_path, updated)?;
    Ok(())
}

struct GitExcludeTarget {
    exclude_path: PathBuf,
    pattern: String,
}

fn git_exclude_target(root: &Path) -> Result<Option<GitExcludeTarget>> {
    let Some(exclude_value) = git_rev_parse(root, &["--git-path", "info/exclude"])? else {
        return Ok(None);
    };
    let path = PathBuf::from(exclude_value);
    let exclude_path = if path.is_absolute() {
        path
    } else {
        root.join(path)
    };
    let prefix = git_rev_parse(root, &["--show-prefix"])?.unwrap_or_default();
    Ok(Some(GitExcludeTarget {
        exclude_path,
        pattern: local_git_exclude_pattern(&prefix),
    }))
}

fn git_rev_parse(root: &Path, args: &[&str]) -> Result<Option<String>> {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .arg("rev-parse")
        .args(args)
        .output();
    let Ok(output) = output else {
        return Ok(None);
    };
    if !output.status.success() {
        return Ok(None);
    }
    let value = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if value.is_empty() {
        return Ok(None);
    }
    Ok(Some(value))
}

fn local_git_exclude_pattern(prefix: &str) -> String {
    let prefix = prefix.trim().replace('\\', "/");
    let prefix = prefix.trim_matches('/');
    if prefix.is_empty() {
        "/.indexsearch/".to_string()
    } else {
        format!("/{prefix}/.indexsearch/")
    }
}

fn git_exclude_has_pattern(text: &str, pattern: &str) -> bool {
    let expected = normalize_git_exclude_pattern(pattern);
    text.lines().any(|line| {
        let trimmed = line.trim();
        !trimmed.is_empty()
            && !trimmed.starts_with('#')
            && normalize_git_exclude_pattern(trimmed) == expected
    })
}

fn normalize_git_exclude_pattern(pattern: &str) -> String {
    let mut pattern = pattern.trim().replace('\\', "/");
    while pattern.len() > 1 && pattern.ends_with('/') {
        pattern.pop();
    }
    pattern
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

fn load_pattern_lines(path: &Path) -> Vec<String> {
    fs::read_to_string(path)
        .ok()
        .map(|text| {
            text.lines()
                .map(|l| l.trim().trim_start_matches('\u{feff}').replace('\\', "/"))
                .filter(|l| !l.is_empty() && !l.starts_with('#') && !l.starts_with('!'))
                .collect()
        })
        .unwrap_or_default()
}

fn add_glob_pattern(builder: &mut GlobSetBuilder, raw: &str, case_insensitive: bool) -> Result<()> {
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
        builder.add(
            GlobBuilder::new(&variant)
                .case_insensitive(case_insensitive)
                .build()?,
        );
    }
    Ok(())
}

fn build_index(
    cfg: &ProjectConfig,
    options: &Options,
    scanned: &mut u64,
    skipped: &mut u64,
    mut timings: Option<&mut Timings>,
    progress: Option<&ProgressLine>,
) -> Result<BuiltIndex> {
    if let Some(progress) = progress {
        progress.begin_indeterminate("Scanning files");
    }
    let scan_timer = Instant::now();
    let entries = scan_indexable_files(cfg, options, scanned, skipped)?;
    if let Some(timings) = timings.as_deref_mut() {
        timings.scan += scan_timer.elapsed().as_secs_f64();
    }
    if let Some(progress) = progress {
        progress.set_total("Processing files", processing_progress_total(&entries));
    }
    let process_timer = Instant::now();
    let index_pool = build_index_thread_pool()?;
    let build_stats = IndexBuildStats::default();
    let mut built_files = build_index_file_entries(entries, options, &build_stats, progress);
    if let Some(timings) = timings.as_deref_mut() {
        timings.index_cpu_threads = build_stats.cpu_threads.load(AtomicOrdering::Relaxed);
        timings.index_io_threads = build_stats.io_threads.load(AtomicOrdering::Relaxed);
        timings.indexed_files += build_stats.indexed_files.load(AtomicOrdering::Relaxed);
        timings.indexed_bytes += build_stats.indexed_bytes.load(AtomicOrdering::Relaxed);
        timings.gram_keys += build_stats.gram_keys.load(AtomicOrdering::Relaxed);
        timings.extra_keys += build_stats.extra_keys.load(AtomicOrdering::Relaxed);
        timings.fragment_keys += build_stats.fragment_keys.load(AtomicOrdering::Relaxed);
        timings.file_read += ns_to_secs(build_stats.read_ns.load(AtomicOrdering::Relaxed));
        timings.tokenize += ns_to_secs(build_stats.tokenize_ns.load(AtomicOrdering::Relaxed));
        timings.tokenize_scan_keys +=
            ns_to_secs(build_stats.tokenize_scan_ns.load(AtomicOrdering::Relaxed));
        timings.tokenize_qualified_calls += ns_to_secs(
            build_stats
                .tokenize_qualified_ns
                .load(AtomicOrdering::Relaxed),
        );
        timings.tokenize_sort_extras += ns_to_secs(
            build_stats
                .tokenize_sort_extras_ns
                .load(AtomicOrdering::Relaxed),
        );
        timings.tokenize_sort_fragments += ns_to_secs(
            build_stats
                .tokenize_sort_fragments_ns
                .load(AtomicOrdering::Relaxed),
        );
        timings.compress += ns_to_secs(build_stats.compress_ns.load(AtomicOrdering::Relaxed));
    }
    let sort_timer = Instant::now();
    if let Some(progress) = progress {
        progress.set_indeterminate("Sorting files");
    }
    built_files.files.sort_by_key(|file| file.ordinal);
    if let Some(timings) = timings.as_deref_mut() {
        timings.sort += sort_timer.elapsed().as_secs_f64();
    }

    if let Some(progress) = progress {
        progress.set_indeterminate("Selecting terms");
    }
    let fragments_timer = Instant::now();
    let selected_fragments = selected_word_fragments(
        built_files
            .files
            .iter()
            .map(|file| built_files.fragments(file)),
    );
    if let Some(timings) = timings.as_deref_mut() {
        timings.select_fragments += fragments_timer.elapsed().as_secs_f64();
    }
    let postings_timer = Instant::now();
    let posting_chunks = posting_build_chunk_count(built_files.files.len());
    if let Some(progress) = progress {
        progress.set_total("Building postings", posting_chunks as u64);
    }
    let posting_build = match &index_pool {
        Some(pool) => {
            pool.install(|| build_postings_parallel(&built_files, &selected_fragments, progress))
        }
        None => build_postings_parallel(&built_files, &selected_fragments, progress),
    };
    if let Some(timings) = timings.as_deref_mut() {
        timings.postings += postings_timer.elapsed().as_secs_f64();
        timings.postings_build_chunks += posting_build.build_chunks;
        timings.postings_merge += posting_build.merge;
        timings.postings_merge_shards = posting_build.shards as u64;
    }
    let postings = posting_build.postings;
    let out_files = built_files
        .files
        .into_iter()
        .map(|file| file.file)
        .collect();
    *skipped += build_stats.skipped_reads.load(AtomicOrdering::Relaxed);
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

fn build_index_file_entries(
    entries: Vec<CurrentFile>,
    options: &Options,
    stats: &IndexBuildStats,
    progress: Option<&ProgressLine>,
) -> BuiltIndexFiles {
    if entries.is_empty() {
        return BuiltIndexFiles::default();
    }
    let cpu_threads = index_cpu_thread_count().min(entries.len()).max(1);
    let io_threads = index_io_thread_count(cpu_threads).min(entries.len()).max(1);
    stats
        .cpu_threads
        .store(cpu_threads as u64, AtomicOrdering::Relaxed);
    stats
        .io_threads
        .store(io_threads as u64, AtomicOrdering::Relaxed);

    let count_profile_keys = options.profile;
    let profile_tokenize_steps = options.profile && index_profile_detail_enabled();
    let channel_capacity = index_pipeline_channel_capacity(cpu_threads, io_threads);
    let worker_file_capacity = entries.len().div_ceil(cpu_threads);
    let (tx, rx) = mpsc::sync_channel::<ReadIndexFile>(channel_capacity);
    let rx = Arc::new(Mutex::new(rx));

    std::thread::scope(|scope| {
        let mut cpu_handles = Vec::with_capacity(cpu_threads);
        for _ in 0..cpu_threads {
            let rx = Arc::clone(&rx);
            cpu_handles.push(scope.spawn(move || {
                let mut scratch = IndexKeyScratch::new();
                let mut output = IndexWorkerOutput {
                    files: Vec::with_capacity(worker_file_capacity),
                    ..Default::default()
                };
                let mut progressed = 0u64;
                loop {
                    let read_file = match rx.lock().expect("index receiver poisoned").recv() {
                        Ok(read_file) => read_file,
                        Err(_) => break,
                    };
                    let progress_units = index_file_progress_units(read_file.bytes.len() as u64);
                    process_index_file(
                        read_file,
                        &mut scratch,
                        &mut output,
                        stats,
                        count_profile_keys,
                        profile_tokenize_steps,
                    );
                    if let Some(progress) = progress {
                        progressed += progress_units;
                        if progressed >= INDEX_PROCESS_PROGRESS_GRANULARITY {
                            progress.advance(progressed);
                            progressed = 0;
                        }
                    }
                }
                if let Some(progress) = progress {
                    progress.advance(progressed);
                }
                output
            }));
        }

        let chunk_size = entries.len().div_ceil(io_threads);
        for chunk in entries.chunks(chunk_size) {
            let tx = tx.clone();
            scope.spawn(move || {
                for entry in chunk {
                    let read_timer = Instant::now();
                    let Ok(bytes) = read_index_file_bytes(&entry.path, entry.size) else {
                        if let Some(progress) = progress {
                            progress.advance(index_file_progress_units(entry.size));
                        }
                        continue;
                    };
                    stats
                        .read_ns
                        .fetch_add(elapsed_ns(read_timer), AtomicOrdering::Relaxed);
                    let read_file = ReadIndexFile {
                        ordinal: entry.ordinal,
                        rel: entry.rel.clone(),
                        mtime: entry.mtime,
                        bytes,
                    };
                    if tx.send(read_file).is_err() {
                        if let Some(progress) = progress {
                            progress.advance(index_file_progress_units(entry.size));
                        }
                        break;
                    }
                }
            });
        }
        drop(tx);

        let mut built = BuiltIndexFiles::default();
        for handle in cpu_handles {
            let mut output = handle.join().expect("index worker panicked");
            let arena = built.gram_arenas.len();
            for file in &mut output.files {
                file.gram_arena = arena;
                file.fragment_arena = arena;
            }
            built.files.extend(output.files);
            built.gram_arenas.push(output.grams);
            built.fragment_arenas.push(output.fragments);
        }
        built
    })
}

fn processing_progress_total(entries: &[CurrentFile]) -> u64 {
    entries
        .iter()
        .map(|entry| index_file_progress_units(entry.size))
        .sum()
}

fn index_file_progress_units(size: u64) -> u64 {
    size.max(1)
}

fn process_index_file(
    read_file: ReadIndexFile,
    scratch: &mut IndexKeyScratch,
    output: &mut IndexWorkerOutput,
    stats: &IndexBuildStats,
    count_profile_keys: bool,
    profile_tokenize_steps: bool,
) {
    let bytes = read_file.bytes;
    if is_binary(&bytes) {
        stats.skipped_reads.fetch_add(1, AtomicOrdering::Relaxed);
        return;
    }
    if count_profile_keys {
        stats.indexed_files.fetch_add(1, AtomicOrdering::Relaxed);
        stats
            .indexed_bytes
            .fetch_add(bytes.len() as u64, AtomicOrdering::Relaxed);
    }

    let tokenize_timer = Instant::now();
    let (gram_len, extras_len, fragment_len) = if profile_tokenize_steps {
        index_grams_and_word_fragments_into_profiled(
            &bytes,
            scratch,
            &stats.tokenize_scan_ns,
            &stats.tokenize_qualified_ns,
            &stats.tokenize_sort_extras_ns,
            &stats.tokenize_sort_fragments_ns,
        )
    } else {
        index_grams_and_word_fragments_into(&bytes, scratch)
    };
    stats
        .tokenize_ns
        .fetch_add(elapsed_ns(tokenize_timer), AtomicOrdering::Relaxed);
    if count_profile_keys {
        if profile_tokenize_steps {
            stats
                .gram_keys
                .fetch_add(gram_len as u64, AtomicOrdering::Relaxed);
            stats
                .extra_keys
                .fetch_add(extras_len as u64, AtomicOrdering::Relaxed);
        }
        stats
            .fragment_keys
            .fetch_add(fragment_len as u64, AtomicOrdering::Relaxed);
    }

    let compress_timer = Instant::now();
    let compressed_content = lz4_flex::compress_prepend_size(&bytes);
    stats
        .compress_ns
        .fetch_add(elapsed_ns(compress_timer), AtomicOrdering::Relaxed);
    let gram_start = output.grams.len();
    output.grams.extend_from_slice(&scratch.grams);
    let fragment_start = output.fragments.len();
    output.fragments.extend_from_slice(&scratch.fragments);
    output.files.push(BuiltIndexFile {
        ordinal: read_file.ordinal,
        file: FileEntry {
            path: read_file.rel,
            mtime: read_file.mtime,
            size: bytes.len() as u64,
            content: ENABLE_CHUNK_EXTENSION.then_some(bytes).unwrap_or_default(),
            compressed_content,
        },
        gram_arena: 0,
        gram_start,
        gram_len: scratch.grams.len(),
        fragment_arena: 0,
        fragment_start,
        fragment_len: scratch.fragments.len(),
    });
}

fn read_index_file_bytes(path: &Path, expected_size: u64) -> std::io::Result<Vec<u8>> {
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_SEQUENTIAL_SCAN;

        let mut file = OpenOptions::new()
            .read(true)
            .custom_flags(FILE_FLAG_SEQUENTIAL_SCAN)
            .open(path)?;
        let capacity = usize::try_from(expected_size).unwrap_or(0);
        let mut bytes = Vec::with_capacity(capacity);
        file.read_to_end(&mut bytes)?;
        Ok(bytes)
    }
    #[cfg(not(windows))]
    {
        let _ = expected_size;
        fs::read(path)
    }
}

fn build_index_thread_pool() -> Result<Option<ThreadPool>> {
    let threads = index_cpu_thread_count();
    ThreadPoolBuilder::new()
        .num_threads(threads)
        .thread_name(|idx| format!("indexsearch-index-{idx}"))
        .build()
        .map(Some)
        .context("failed to build index worker pool")
}

fn index_cpu_thread_count() -> usize {
    thread_count_env("INDEXSEARCH_INDEX_CPU_THREADS")
        .or_else(|| thread_count_env("INDEXSEARCH_INDEX_THREADS"))
        .unwrap_or_else(default_index_cpu_threads)
}

fn index_io_thread_count(cpu_threads: usize) -> usize {
    thread_count_env("INDEXSEARCH_INDEX_IO_THREADS").unwrap_or_else(|| {
        let available = available_threads();
        #[cfg(windows)]
        {
            (available / 16).clamp(2, 4).min(cpu_threads.max(1))
        }
        #[cfg(not(windows))]
        {
            (cpu_threads / 4).clamp(2, 8).min(cpu_threads.max(1))
        }
    })
}

fn index_pipeline_channel_capacity(cpu_threads: usize, io_threads: usize) -> usize {
    ((cpu_threads + io_threads) * 4).clamp(32, 512)
}

fn thread_count_env(name: &str) -> Option<usize> {
    if let Ok(value) = env::var(name) {
        let trimmed = value.trim();
        if trimmed == "0" {
            return None;
        }
        if let Ok(threads) = trimmed.parse::<usize>() {
            return (threads > 0).then_some(threads);
        }
    }
    None
}

fn default_index_cpu_threads() -> usize {
    let available = available_threads();
    #[cfg(windows)]
    {
        available.min(16).max(1)
    }
    #[cfg(not(windows))]
    {
        available.max(1)
    }
}

fn available_threads() -> usize {
    std::thread::available_parallelism()
        .ok()
        .map(|count| count.get())
        .unwrap_or(1)
}

fn index_profile_detail_enabled() -> bool {
    matches!(
        env::var("INDEXSEARCH_INDEX_PROFILE_DETAIL").as_deref(),
        Ok("1") | Ok("true") | Ok("TRUE") | Ok("yes") | Ok("YES")
    )
}

fn read_current_file_entry(
    cfg: &ProjectConfig,
    options: &Options,
    rel: &str,
    mut timings: Option<&mut Timings>,
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
    let read_timer = Instant::now();
    let bytes = read_index_file_bytes(&path, meta.len())?;
    if let Some(timings) = timings.as_deref_mut() {
        timings.file_read += read_timer.elapsed().as_secs_f64();
    }
    if is_binary(&bytes) {
        return Ok(None);
    }
    let tokenize_timer = Instant::now();
    let mut scratch = TrigramScratch::new();
    let grams = index_grams(&bytes, &mut scratch);
    if let Some(timings) = timings.as_deref_mut() {
        timings.tokenize += tokenize_timer.elapsed().as_secs_f64();
    }
    let compress_timer = Instant::now();
    let compressed_content = lz4_flex::compress_prepend_size(&bytes);
    if let Some(timings) = timings.as_deref_mut() {
        timings.compress += compress_timer.elapsed().as_secs_f64();
    }
    Ok(Some((
        FileEntry {
            path: rel,
            mtime: mtime_ns(&meta),
            size: bytes.len() as u64,
            content: ENABLE_CHUNK_EXTENSION.then_some(bytes).unwrap_or_default(),
            compressed_content,
        },
        grams,
    )))
}

struct PostingBuildResult {
    postings: HashMap<u32, Vec<u32>>,
    build_chunks: f64,
    merge: f64,
    shards: usize,
}

struct PostingPartial {
    shards: Vec<HashMap<u32, Vec<u32>>>,
}

impl PostingPartial {
    fn new(shard_count: usize) -> Self {
        Self {
            shards: (0..shard_count).map(|_| HashMap::default()).collect(),
        }
    }

    fn push(&mut self, key: u32, id: u32) {
        let shard = posting_key_shard(key, self.shards.len());
        self.shards[shard]
            .entry(key)
            .or_insert_with(Vec::new)
            .push(id);
    }
}

fn build_postings_parallel(
    files: &BuiltIndexFiles,
    selected_fragments: &HashSet<u32>,
    progress: Option<&ProgressLine>,
) -> PostingBuildResult {
    let chunk_files = posting_build_chunk_files();
    let shard_count = posting_merge_shard_count();
    let build_timer = Instant::now();
    let partials: Vec<PostingPartial> = files
        .files
        .par_chunks(chunk_files)
        .enumerate()
        .map(|(chunk_idx, chunk)| {
            let mut local = PostingPartial::new(shard_count);
            let base_id = chunk_idx * chunk_files;
            for (idx, file) in chunk.iter().enumerate() {
                let id = (base_id + idx) as u32;
                for &gram in files.grams(file) {
                    local.push(gram, id);
                }
                for &fragment in files.fragments(file) {
                    if selected_fragments.contains(&fragment) {
                        local.push(fragment, id);
                    }
                }
            }
            if let Some(progress) = progress {
                progress.advance(1);
            }
            local
        })
        .collect();
    let build_chunks = build_timer.elapsed().as_secs_f64();

    if let Some(progress) = progress {
        progress.set_indeterminate("Merging postings");
    }
    let merge_timer = Instant::now();
    let postings = merge_posting_partials(partials, shard_count);
    PostingBuildResult {
        postings,
        build_chunks,
        merge: merge_timer.elapsed().as_secs_f64(),
        shards: shard_count,
    }
}

fn merge_posting_partials(
    partials: Vec<PostingPartial>,
    shard_count: usize,
) -> HashMap<u32, Vec<u32>> {
    let partial_count = partials.len();
    let mut per_shard: Vec<Vec<HashMap<u32, Vec<u32>>>> = (0..shard_count)
        .map(|_| Vec::with_capacity(partial_count))
        .collect();
    for partial in partials {
        for (idx, shard) in partial.shards.into_iter().enumerate() {
            if !shard.is_empty() {
                per_shard[idx].push(shard);
            }
        }
    }
    let merged_shards: Vec<HashMap<u32, Vec<u32>>> = per_shard
        .into_par_iter()
        .map(merge_posting_maps_exact)
        .collect();
    let total_keys = merged_shards.iter().map(HashMap::len).sum();
    let mut postings = HashMap::default();
    postings.reserve(total_keys);
    for shard in merged_shards {
        postings.extend(shard);
    }
    postings
}

fn merge_posting_maps_exact(mut partials: Vec<HashMap<u32, Vec<u32>>>) -> HashMap<u32, Vec<u32>> {
    if partials.len() <= 1 {
        return partials.pop().unwrap_or_default();
    }
    let mut counts: HashMap<u32, usize> = HashMap::default();
    for partial in &partials {
        for (&key, ids) in partial {
            *counts.entry(key).or_insert(0) += ids.len();
        }
    }
    let mut postings = HashMap::default();
    postings.reserve(counts.len());
    for (key, count) in counts {
        postings.insert(key, Vec::with_capacity(count));
    }
    for partial in partials {
        for (key, mut ids) in partial {
            postings
                .get_mut(&key)
                .expect("posting count was not reserved")
                .append(&mut ids);
        }
    }
    postings
}

fn posting_build_chunk_count(file_count: usize) -> usize {
    if file_count == 0 {
        0
    } else {
        file_count.div_ceil(posting_build_chunk_files())
    }
}

fn posting_build_chunk_files() -> usize {
    thread_count_env("INDEXSEARCH_POSTING_CHUNK_FILES")
        .unwrap_or(POSTING_BUILD_CHUNK_FILES)
        .max(1)
}

fn posting_merge_shard_count() -> usize {
    let requested =
        thread_count_env("INDEXSEARCH_POSTING_MERGE_SHARDS").unwrap_or_else(index_cpu_thread_count);
    requested
        .max(1)
        .next_power_of_two()
        .min(POSTING_MERGE_MAX_SHARDS)
}

fn posting_key_shard(key: u32, shard_count: usize) -> usize {
    if shard_count <= 1 {
        return 0;
    }
    let mixed = (key as usize).wrapping_mul(0x9E37_79B1);
    mixed & (shard_count - 1)
}

fn build_delta_index(
    cfg: &ProjectConfig,
    options: &Options,
    changes: &[ChangedPath],
    scanned: &mut u64,
    skipped: &mut u64,
    mut timings: Option<&mut Timings>,
) -> Result<(BuiltIndex, DeltaMeta, UpdateStats)> {
    *scanned = changes.len() as u64;
    let existing_timer = Instant::now();
    let existing = current_visible_paths(&cfg.root)?;
    if let Some(timings) = timings.as_deref_mut() {
        timings.current_meta += existing_timer.elapsed().as_secs_f64();
    }
    let mut files = Vec::new();
    let mut postings: HashMap<u32, Vec<u32>> = HashMap::default();
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

        match read_current_file_entry(cfg, options, &change.rel, timings.as_deref_mut())? {
            Some((entry, grams)) => {
                if existing.contains(&change.rel) {
                    stats.updated += 1;
                    meta.tombstones.insert(change.rel.clone());
                } else {
                    stats.added += 1;
                }
                let id = files.len() as u32;
                let postings_timer = Instant::now();
                for gram in grams {
                    postings.entry(gram).or_default().push(id);
                }
                if let Some(timings) = timings.as_deref_mut() {
                    timings.postings += postings_timer.elapsed().as_secs_f64();
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

fn collect_filesystem_changes(
    cfg: &ProjectConfig,
    options: &Options,
    scanned: &mut u64,
    skipped: &mut u64,
    timings: &mut Timings,
) -> Result<(Vec<ChangedPath>, usize)> {
    let scan_timer = Instant::now();
    let entries = scan_indexable_files(cfg, options, scanned, skipped)?;
    timings.scan += scan_timer.elapsed().as_secs_f64();

    let current_timer = Instant::now();
    let current = current_visible_file_meta(&cfg.root)?;
    timings.current_meta += current_timer.elapsed().as_secs_f64();
    let visible_before = current.len();
    let mut seen = HashSet::with_capacity_and_hasher(entries.len(), Default::default());
    let mut changes = Vec::new();

    let diff_timer = Instant::now();
    for entry in entries {
        seen.insert(entry.rel.clone());
        match current.get(&entry.rel) {
            Some((mtime, size)) if *mtime == entry.mtime && *size == entry.size => {}
            _ => changes.push(ChangedPath {
                rel: entry.rel,
                deleted: false,
            }),
        }
    }

    for rel in current.keys() {
        if !seen.contains(rel) {
            changes.push(ChangedPath {
                rel: rel.clone(),
                deleted: true,
            });
        }
    }
    timings.change_diff += diff_timer.elapsed().as_secs_f64();

    Ok((changes, visible_before))
}

fn current_visible_file_meta(root: &Path) -> Result<BTreeMap<String, (i64, u64)>> {
    let base = MappedIndex::open(&index_path(root))?;
    let deltas = load_deltas(root)?;
    let mut files = BTreeMap::new();
    for id in 0..base.file_count {
        let rec = base.file_record(id)?;
        files.insert(bytes_to_string(base.file_path(id)?), (rec.mtime, rec.size));
    }
    for delta in &deltas {
        for tombstone in &delta.meta.tombstones {
            files.remove(tombstone);
        }
        for id in 0..delta.index.file_count {
            let rec = delta.index.file_record(id)?;
            files.insert(
                bytes_to_string(delta.index.file_path(id)?),
                (rec.mtime, rec.size),
            );
        }
    }
    Ok(files)
}

fn current_visible_paths(root: &Path) -> Result<HashSet<String>> {
    let base = MappedIndex::open(&index_path(root))?;
    let deltas = load_deltas(root)?;
    let mut paths = HashSet::default();
    for id in 0..base.file_count {
        paths.insert(bytes_to_string(base.file_path(id)?));
    }
    for delta in &deltas {
        for tombstone in &delta.meta.tombstones {
            paths.remove(tombstone);
        }
        for id in 0..delta.index.file_count {
            paths.insert(bytes_to_string(delta.index.file_path(id)?));
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
    let mut id_maps = Vec::with_capacity(deltas.len() + 1);
    id_maps.push(append_compacted_segment_files(
        base,
        &exclusions[0],
        &mut files,
    )?);
    for (idx, delta) in deltas.iter().enumerate() {
        id_maps.push(append_compacted_segment_files(
            &delta.index,
            &exclusions[idx + 1],
            &mut files,
        )?);
    }
    let mut postings: HashMap<u32, Vec<u32>> = HashMap::default();
    merge_segment_postings(base, &id_maps[0], &mut postings)?;
    for (idx, delta) in deltas.iter().enumerate() {
        merge_segment_postings(&delta.index, &id_maps[idx + 1], &mut postings)?;
    }
    Ok(BuiltIndex {
        root: cfg.root.clone(),
        config_hash: cfg.hash,
        files,
        postings,
    })
}

fn append_compacted_segment_files(
    index: &MappedIndex,
    excluded_paths: &HashSet<String>,
    out: &mut Vec<FileEntry>,
) -> Result<Vec<Option<u32>>> {
    let mut id_map = vec![None; index.file_count];
    for id in 0..index.file_count {
        let path = bytes_to_string(index.file_path(id)?);
        if excluded_paths.contains(&path) {
            continue;
        }
        let rec = index.file_record(id)?;
        let compressed_content = index.file_compressed_content(&rec)?.to_vec();
        let content = if ENABLE_CHUNK_EXTENSION {
            index.file(id)?.content.to_vec()
        } else {
            Vec::new()
        };
        id_map[id] = Some(out.len() as u32);
        out.push(FileEntry {
            path,
            mtime: rec.mtime,
            size: rec.size,
            content,
            compressed_content,
        });
    }
    Ok(id_map)
}

fn merge_segment_postings(
    index: &MappedIndex,
    id_map: &[Option<u32>],
    postings: &mut HashMap<u32, Vec<u32>>,
) -> Result<()> {
    for posting_id in 0..index.posting_count {
        let rec = index.posting_record(posting_id)?;
        let data = index.posting_data_for_record(posting_id, &rec)?;
        let out = postings.entry(rec.gram).or_default();
        for &old_id in data.as_ref() {
            if let Some(Some(new_id)) = id_map.get(old_id as usize) {
                out.push(*new_id);
            }
        }
    }
    postings.retain(|_, ids| !ids.is_empty());
    Ok(())
}

fn render_visible_files(
    base: &MappedIndex,
    deltas: &[DeltaSegment],
    options: &Options,
) -> Result<Vec<u8>> {
    let exclusions = segment_exclusions(base, deltas)?;
    let mut out = Vec::new();
    write_segment_files(&mut out, base, &exclusions[0], options)?;
    for (idx, delta) in deltas.iter().enumerate() {
        write_segment_files(&mut out, &delta.index, &exclusions[idx + 1], options)?;
    }
    Ok(out)
}

fn write_segment_files<W: Write>(
    out: &mut W,
    index: &MappedIndex,
    excluded_paths: &HashSet<String>,
    options: &Options,
) -> Result<()> {
    let path_filter = PathFilter::new(options, &index.root)?;
    let display_path = DisplayPathMapper::new(&index.root, options);
    for id in 0..index.file_count {
        let path = bytes_to_string(index.file_path(id)?);
        if path_filter.allows(&path, excluded_paths) {
            writeln!(out, "{}", display_path.display(&path))?;
        }
    }
    Ok(())
}

fn segment_exclusions(base: &MappedIndex, deltas: &[DeltaSegment]) -> Result<Vec<HashSet<String>>> {
    let mut exclusions = vec![HashSet::default(); deltas.len() + 1];
    let mut shadowed = HashSet::default();
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
        shadowed.insert(bytes_to_string(delta.index.file_path(id)?));
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
    if !git_metadata_present(root) {
        return Ok(false);
    }
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

fn git_metadata_present(root: &Path) -> bool {
    root.join(".git").exists()
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
    if !git_metadata_present(root) {
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
    save_delta_profiled(root, index, meta, None)
}

fn save_delta_profiled(
    root: &Path,
    index: &BuiltIndex,
    meta: &DeltaMeta,
    mut timings: Option<&mut Timings>,
) -> Result<()> {
    let dir = delta_dir(root);
    fs::create_dir_all(&dir)?;
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let stem = format!("delta-{stamp}-{}", std::process::id());
    let bin_path = dir.join(format!("{stem}.bin"));
    let meta_path = dir.join(format!("{stem}.meta"));
    save_index_profiled(index, &bin_path, timings.as_deref_mut())?;
    let meta_timer = Instant::now();
    save_delta_meta(meta, &meta_path)?;
    if let Some(timings) = timings {
        timings.write_delta_meta += meta_timer.elapsed().as_secs_f64();
    }
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
    #[cfg(windows)]
    {
        if !options.follow {
            return scan_indexable_files_windows(cfg, options, scanned, skipped);
        }
    }
    scan_indexable_files_walkdir(cfg, options, scanned, skipped)
}

fn scan_indexable_files_walkdir(
    cfg: &ProjectConfig,
    options: &Options,
    scanned: &mut u64,
    skipped: &mut u64,
) -> Result<Vec<CurrentFile>> {
    let mut out = Vec::new();
    for entry in WalkDir::new(&cfg.root)
        .follow_links(options.follow)
        .into_iter()
        .filter_entry(|entry| should_descend(cfg, options, entry))
    {
        let Ok(entry) = entry else {
            *skipped += 1;
            continue;
        };
        if !entry.file_type().is_file() {
            continue;
        }
        let ordinal = *scanned as usize;
        *scanned += 1;
        let path = entry.path();
        let Some(rel) = rel_path(&cfg.root, path) else {
            *skipped += 1;
            continue;
        };
        if (!options.hidden && is_hidden(&rel)) || !is_searchable(cfg, &rel) {
            *skipped += 1;
            continue;
        }
        let Ok(meta) = entry.metadata() else {
            *skipped += 1;
            continue;
        };
        if meta.len() > options.max_filesize {
            *skipped += 1;
            continue;
        }
        out.push(CurrentFile {
            ordinal,
            path: path.to_path_buf(),
            rel,
            mtime: mtime_ns(&meta),
            size: meta.len(),
        });
    }
    Ok(out)
}

fn should_descend(cfg: &ProjectConfig, options: &Options, entry: &DirEntry) -> bool {
    if !entry.file_type().is_dir() {
        return true;
    }
    let Ok(rel) = entry.path().strip_prefix(&cfg.root) else {
        return true;
    };
    let rel = rel.to_string_lossy().replace('\\', "/");
    if rel.is_empty() {
        return true;
    }
    should_descend_rel(cfg, options, &rel)
}

fn should_descend_rel(cfg: &ProjectConfig, options: &Options, rel: &str) -> bool {
    if !options.hidden && is_hidden(rel) {
        return false;
    }
    if cfg.paths_ignore.is_match(rel) {
        return false;
    }
    true
}

#[cfg(windows)]
fn scan_indexable_files_windows(
    cfg: &ProjectConfig,
    options: &Options,
    scanned: &mut u64,
    skipped: &mut u64,
) -> Result<Vec<CurrentFile>> {
    let mut out = Vec::new();
    scan_indexable_dir_windows(cfg, options, &cfg.root, "", &mut out, scanned, skipped)?;
    Ok(out)
}

#[cfg(windows)]
fn scan_indexable_dir_windows(
    cfg: &ProjectConfig,
    options: &Options,
    dir: &Path,
    dir_rel: &str,
    out: &mut Vec<CurrentFile>,
    scanned: &mut u64,
    skipped: &mut u64,
) -> Result<()> {
    use std::ptr::null;
    use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT, FIND_FIRST_EX_LARGE_FETCH,
        FindClose, FindExInfoBasic, FindExSearchNameMatch, FindFirstFileExW, FindNextFileW,
        WIN32_FIND_DATAW,
    };

    let mut data: WIN32_FIND_DATAW = unsafe { std::mem::zeroed() };
    let pattern = windows_find_pattern(dir);
    let handle = unsafe {
        FindFirstFileExW(
            pattern.as_ptr(),
            FindExInfoBasic,
            &mut data as *mut _ as *mut std::ffi::c_void,
            FindExSearchNameMatch,
            null(),
            FIND_FIRST_EX_LARGE_FETCH,
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        *skipped += 1;
        return Ok(());
    }
    loop {
        let name = windows_find_name(&data.cFileName);
        let name_text = name.to_string_lossy();
        if name_text != "." && name_text != ".." {
            let name_rel = name_text.replace('\\', "/");
            let rel = if dir_rel.is_empty() {
                name_rel
            } else {
                format!("{dir_rel}/{name_rel}")
            };
            let attrs = data.dwFileAttributes;
            let is_reparse = attrs & FILE_ATTRIBUTE_REPARSE_POINT != 0;
            if attrs & FILE_ATTRIBUTE_DIRECTORY != 0 {
                if !is_reparse && should_descend_rel(cfg, options, &rel) {
                    scan_indexable_dir_windows(
                        cfg,
                        options,
                        &dir.join(&name),
                        &rel,
                        out,
                        scanned,
                        skipped,
                    )?;
                }
            } else if !is_reparse {
                let ordinal = *scanned as usize;
                *scanned += 1;
                if (!options.hidden && is_hidden(&rel)) || !is_searchable(cfg, &rel) {
                    *skipped += 1;
                } else {
                    let size = ((data.nFileSizeHigh as u64) << 32) | data.nFileSizeLow as u64;
                    if size > options.max_filesize {
                        *skipped += 1;
                    } else {
                        out.push(CurrentFile {
                            ordinal,
                            path: dir.join(&name),
                            rel,
                            mtime: windows_find_mtime_ns(&data),
                            size,
                        });
                    }
                }
            }
        }
        if unsafe { FindNextFileW(handle, &mut data) } == 0 {
            break;
        }
    }
    unsafe {
        FindClose(handle);
    }
    Ok(())
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

#[cfg(windows)]
fn windows_find_pattern(dir: &Path) -> Vec<u16> {
    let mut pattern: Vec<u16> = dir.as_os_str().encode_wide().collect();
    if !pattern
        .last()
        .is_some_and(|&ch| ch == b'\\' as u16 || ch == b'/' as u16)
    {
        pattern.push(b'\\' as u16);
    }
    pattern.push(b'*' as u16);
    pattern.push(0);
    pattern
}

#[cfg(windows)]
fn windows_find_name(name: &[u16]) -> OsString {
    let len = name.iter().position(|&ch| ch == 0).unwrap_or(name.len());
    OsString::from_wide(&name[..len])
}

#[cfg(windows)]
fn windows_filetime_to_unix_ns(ticks_100ns: u64) -> i64 {
    const WINDOWS_TO_UNIX_EPOCH_100NS: u64 = 116_444_736_000_000_000;
    ticks_100ns
        .saturating_sub(WINDOWS_TO_UNIX_EPOCH_100NS)
        .saturating_mul(100) as i64
}

#[cfg(windows)]
fn windows_find_mtime_ns(data: &windows_sys::Win32::Storage::FileSystem::WIN32_FIND_DATAW) -> i64 {
    let ticks = ((data.ftLastWriteTime.dwHighDateTime as u64) << 32)
        | data.ftLastWriteTime.dwLowDateTime as u64;
    windows_filetime_to_unix_ns(ticks)
}

fn mtime_ns(meta: &fs::Metadata) -> i64 {
    #[cfg(windows)]
    {
        return windows_filetime_to_unix_ns(meta.last_write_time());
    }
    #[cfg(not(windows))]
    {
        meta.modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_nanos() as i64)
            .unwrap_or(0)
    }
}

fn is_binary(bytes: &[u8]) -> bool {
    bytes
        .get(..bytes.len().min(65536))
        .is_some_and(|prefix| memchr(0, prefix).is_some())
}

#[allow(dead_code)]
struct ChunkExtensionBuild {
    records: Vec<ChunkRecord>,
    bloom_data: Vec<u8>,
}

#[allow(dead_code)]
fn build_chunk_extension(files: &[FileEntry]) -> ChunkExtensionBuild {
    struct FileChunkBuild {
        records: Vec<ChunkRecord>,
        bloom_data: Vec<u8>,
    }

    let builds: Vec<FileChunkBuild> = files
        .par_iter()
        .enumerate()
        .map(|(file_id, file)| {
            let mut records = Vec::new();
            let mut bloom_data = Vec::new();
            let mut line_no = 1u32;
            for (chunk_index, chunk) in file.content.chunks(CHUNK_SIZE).enumerate() {
                let start = chunk_index * CHUNK_SIZE;
                let slice_start = start.saturating_sub(CHUNK_OVERLAP);
                let slice_end = (start + chunk.len() + CHUNK_OVERLAP).min(file.content.len());
                bloom_data.extend_from_slice(&chunk_bloom(&file.content[slice_start..slice_end]));
                records.push(ChunkRecord {
                    file_id: file_id as u32,
                    start: start as u64,
                    size: chunk.len() as u32,
                    line_no,
                });
                line_no = line_no.saturating_add(bytecount_newlines(chunk) as u32);
            }
            FileChunkBuild {
                records,
                bloom_data,
            }
        })
        .collect();

    let mut records = Vec::new();
    let mut bloom_data = Vec::new();
    for build in builds {
        for rec in build.records {
            records.push(rec);
        }
        bloom_data.extend_from_slice(&build.bloom_data);
    }
    ChunkExtensionBuild {
        records,
        bloom_data,
    }
}

fn chunk_bloom(bytes: &[u8]) -> [u8; CHUNK_BLOOM_BYTES] {
    let mut bloom = [0u8; CHUNK_BLOOM_BYTES];
    if bytes.len() < 3 {
        return bloom;
    }
    for window in bytes.windows(3) {
        let gram = trigram(window[0], window[1], window[2]);
        set_chunk_bloom_bit(&mut bloom, gram, 0x9e37_79b9);
        set_chunk_bloom_bit(&mut bloom, gram, 0x85eb_ca6b);
    }
    bloom
}

fn set_chunk_bloom_bit(bloom: &mut [u8; CHUNK_BLOOM_BYTES], gram: u32, salt: u32) {
    let bit = chunk_bloom_bit(gram, salt);
    bloom[bit / 8] |= 1u8 << (bit % 8);
}

fn chunk_bloom_maybe(bloom: &[u8], gram: u32) -> bool {
    chunk_bloom_has_bit(bloom, gram, 0x9e37_79b9) && chunk_bloom_has_bit(bloom, gram, 0x85eb_ca6b)
}

fn chunk_bloom_has_bit(bloom: &[u8], gram: u32, salt: u32) -> bool {
    let bit = chunk_bloom_bit(gram, salt);
    bloom
        .get(bit / 8)
        .is_some_and(|byte| byte & (1u8 << (bit % 8)) != 0)
}

fn chunk_bloom_bit(gram: u32, salt: u32) -> usize {
    let mut hash = gram ^ salt;
    hash ^= hash >> 16;
    hash = hash.wrapping_mul(0x7feb_352d);
    hash ^= hash >> 15;
    hash = hash.wrapping_mul(0x846c_a68b);
    hash ^= hash >> 16;
    (hash as usize) & (CHUNK_BLOOM_BYTES * 8 - 1)
}

fn bytecount_newlines(bytes: &[u8]) -> usize {
    bytes.iter().filter(|&&byte| byte == b'\n').count()
}

fn save_index(index: &BuiltIndex, path: &Path) -> Result<()> {
    save_index_profiled_inner(index, path, None, None)
}

fn save_index_with_progress(
    index: &BuiltIndex,
    path: &Path,
    progress: Option<&ProgressLine>,
) -> Result<()> {
    save_index_profiled_inner(index, path, None, progress)
}

fn save_index_profiled(
    index: &BuiltIndex,
    path: &Path,
    timings: Option<&mut Timings>,
) -> Result<()> {
    save_index_profiled_inner(index, path, timings, None)
}

fn save_index_profiled_with_progress(
    index: &BuiltIndex,
    path: &Path,
    timings: Option<&mut Timings>,
    progress: Option<&ProgressLine>,
) -> Result<()> {
    save_index_profiled_inner(index, path, timings, progress)
}

fn save_index_profiled_inner(
    index: &BuiltIndex,
    path: &Path,
    mut timings: Option<&mut Timings>,
    progress: Option<&ProgressLine>,
) -> Result<()> {
    fs::create_dir_all(path.parent().context("index path has no parent")?)?;
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(INDEX_FILE);
    let tmp_path = path.with_file_name(format!("{file_name}.tmp.{}", std::process::id()));
    let root = index.root.to_string_lossy().as_bytes().to_vec();

    let content_blob_size: u64 = index
        .files
        .iter()
        .map(|file| file.compressed_content.len() as u64)
        .sum();

    let prepare_files_timer = Instant::now();
    let path_blob_size: usize = index.files.iter().map(|file| file.path.len()).sum();
    let mut path_blob = Vec::with_capacity(path_blob_size);
    let mut file_records = Vec::with_capacity(index.files.len());
    let mut content_offset = 0u64;
    for file in &index.files {
        let path_offset = path_blob.len() as u64;
        path_blob.extend_from_slice(file.path.as_bytes());
        file_records.push(FileRecord {
            path_offset,
            path_size: file.path.len() as u64,
            content_offset,
            content_size: file.compressed_content.len() as u64,
            mtime: file.mtime,
            size: file.size,
        });
        content_offset += file.compressed_content.len() as u64;
    }
    if let Some(timings) = timings.as_deref_mut() {
        timings.write_prepare_files += prepare_files_timer.elapsed().as_secs_f64();
    }
    let prepare_postings_timer = Instant::now();
    let mut posting_entries: Vec<_> = index.postings.iter().collect();
    posting_entries.sort_by_key(|entry| *entry.0);
    let mut posting_records = Vec::with_capacity(posting_entries.len());
    let mut posting_data = Vec::new();
    for (&gram, ids) in posting_entries {
        posting_records.push(PostingRecord {
            gram,
            offset: posting_data.len() as u64,
            count: ids.len() as u64,
        });
        write_varint_postings(ids, &mut posting_data);
    }
    if let Some(timings) = timings.as_deref_mut() {
        timings.write_prepare_postings += prepare_postings_timer.elapsed().as_secs_f64();
    }
    let prepare_chunks_timer = Instant::now();
    let chunk_extension = ENABLE_CHUNK_EXTENSION.then(|| build_chunk_extension(&index.files));
    if let Some(timings) = timings.as_deref_mut() {
        timings.write_prepare_chunks += prepare_chunks_timer.elapsed().as_secs_f64();
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
    let postings_data_offset = cursor;
    cursor += posting_data.len() as u64;
    let path_blob_offset = cursor;
    cursor += path_blob.len() as u64;
    let content_blob_offset = cursor;
    cursor += content_blob_size;
    let mut chunk_table_offset = 0;
    let mut chunk_posting_table_offset = 0;
    let mut chunk_posting_data_offset = 0;
    let mut chunk_blob_offset = 0;
    if let Some(chunk_extension) = &chunk_extension {
        cursor = align_to(cursor, 8);
        chunk_table_offset = cursor;
        cursor += chunk_extension.records.len() as u64 * 24;
        cursor = align_to(cursor, 8);
        chunk_posting_table_offset = cursor;
        cursor += 0;
        chunk_posting_data_offset = cursor;
        cursor += 0;
        chunk_blob_offset = cursor;
        cursor += chunk_extension.bloom_data.len() as u64 + CHUNK_FOOTER_SIZE as u64;
    }

    if let Some(progress) = progress {
        progress.begin_determinate("Writing index", cursor);
    }
    let mut written_progress = 0u64;
    let file = File::create(&tmp_path)?;
    file.set_len(cursor)?;
    let mut writer = BufWriter::with_capacity(1024 * 1024, file);
    let header_timer = Instant::now();
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
    if let Some(timings) = timings.as_deref_mut() {
        timings.write_header_tables += header_timer.elapsed().as_secs_f64();
    }
    advance_write_progress(progress, &mut written_progress, postings_data_offset);
    let postings_paths_timer = Instant::now();
    writer.write_all(&posting_data)?;
    writer.write_all(&path_blob)?;
    advance_write_progress(
        progress,
        &mut written_progress,
        posting_data.len() as u64 + path_blob.len() as u64,
    );
    if let Some(timings) = timings.as_deref_mut() {
        timings.write_postings_paths += postings_paths_timer.elapsed().as_secs_f64();
    }
    let content_timer = Instant::now();
    let mut pending_content_progress = 0u64;
    for file in &index.files {
        writer.write_all(&file.compressed_content)?;
        pending_content_progress += file.compressed_content.len() as u64;
        if pending_content_progress >= WRITE_PROGRESS_GRANULARITY {
            advance_write_progress(progress, &mut written_progress, pending_content_progress);
            pending_content_progress = 0;
        }
    }
    advance_write_progress(progress, &mut written_progress, pending_content_progress);
    if let Some(timings) = timings.as_deref_mut() {
        timings.write_content += content_timer.elapsed().as_secs_f64();
    }
    let chunk_write_timer = Instant::now();
    if let Some(chunk_extension) = &chunk_extension {
        write_padding(
            &mut writer,
            content_blob_offset + content_blob_size,
            chunk_table_offset,
        )?;
        for rec in &chunk_extension.records {
            write_u32(&mut writer, rec.file_id)?;
            write_u32(&mut writer, rec.size)?;
            write_u64(&mut writer, rec.start)?;
            write_u32(&mut writer, rec.line_no)?;
            write_u32(&mut writer, 0)?;
        }
        write_padding(
            &mut writer,
            chunk_table_offset + chunk_extension.records.len() as u64 * 24,
            chunk_posting_table_offset,
        )?;
        writer.write_all(&chunk_extension.bloom_data)?;
        writer.write_all(CHUNK_FOOTER_MAGIC)?;
        write_u64(&mut writer, chunk_extension.records.len() as u64)?;
        write_u64(&mut writer, chunk_table_offset)?;
        write_u64(&mut writer, 0)?;
        write_u64(&mut writer, chunk_posting_table_offset)?;
        write_u64(&mut writer, chunk_posting_data_offset)?;
        write_u64(&mut writer, chunk_blob_offset)?;
    }
    flush_write_progress(progress, &mut written_progress, cursor);
    if let Some(timings) = timings.as_deref_mut() {
        timings.write_prepare_chunks += chunk_write_timer.elapsed().as_secs_f64();
    }
    let publish_timer = Instant::now();
    writer.flush()?;
    drop(writer);
    if let Err(err) = fs::rename(&tmp_path, path) {
        let _ = fs::remove_file(path);
        fs::rename(&tmp_path, path).with_context(|| {
            format!(
                "failed to replace {} after initial rename error: {err}",
                display_path(path)
            )
        })?;
    }
    if let Some(timings) = timings {
        timings.write_flush_publish += publish_timer.elapsed().as_secs_f64();
    }
    Ok(())
}

const WRITE_PROGRESS_GRANULARITY: u64 = 8 * 1024 * 1024;

fn advance_write_progress(progress: Option<&ProgressLine>, written: &mut u64, amount: u64) {
    if amount == 0 {
        return;
    }
    *written = written.saturating_add(amount);
    if let Some(progress) = progress {
        progress.advance(amount);
    }
}

fn flush_write_progress(progress: Option<&ProgressLine>, written: &mut u64, total: u64) {
    if total > *written {
        advance_write_progress(progress, written, total - *written);
    }
}

fn save_compacted_index(index: &BuiltIndex, path: &Path) -> Result<()> {
    let compact_path = compacted_index_temp_path(path);
    save_index(index, &compact_path)?;
    publish_compacted_index(&compact_path, path)
}

fn compacted_index_temp_path(path: &Path) -> PathBuf {
    path.with_file_name(format!(
        "{}.compact.{}",
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or(INDEX_FILE),
        std::process::id()
    ))
}

fn publish_compacted_index(compact_path: &Path, path: &Path) -> Result<()> {
    let parent = path.parent().context("index path has no parent")?;
    fs::create_dir_all(parent)?;
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
    if let Err(err) = fs::rename(compact_path, path) {
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
        let meta = file.metadata()?;
        let mmap = unsafe { Mmap::map(&file)? };
        let header = parse_header(&mmap)?;
        let chunk_info = parse_chunk_info(&mmap, header.version)?;
        let root_bytes = checked_slice(&mmap, header.root_offset, header.root_size)?;
        let root = PathBuf::from(String::from_utf8_lossy(root_bytes).to_string());
        Ok(Self {
            mmap,
            index_size: meta.len(),
            index_mtime: mtime_ns(&meta),
            version: header.version,
            root,
            config_hash: header.config_hash,
            file_count: header.file_count as usize,
            posting_count: header.posting_count as usize,
            file_table_offset: header.file_table_offset as usize,
            posting_table_offset: header.posting_table_offset as usize,
            postings_data_offset: header.postings_data_offset as usize,
            path_blob_offset: header.path_blob_offset as usize,
            content_blob_offset: header.content_blob_offset as usize,
            chunk_info,
        })
    }

    fn file(&self, id: usize) -> Result<FileView<'_>> {
        if id >= self.file_count {
            bail!("file id out of bounds");
        }
        let rec = self.file_record(id)?;
        Ok(FileView {
            content: self.file_content_for_id(id, &rec)?,
        })
    }

    fn file_for_search<'a>(
        &'a self,
        id: usize,
        scratch: &'a mut Vec<u8>,
    ) -> Result<(&'a [u8], &'a [u8])> {
        if id >= self.file_count {
            bail!("file id out of bounds");
        }
        let rec = self.file_record(id)?;
        let path = checked_slice(
            &self.mmap,
            self.path_blob_offset as u64 + rec.path_offset,
            rec.path_size,
        )?;
        let content = self.file_content_for_search(&rec, scratch)?;
        Ok((path, content))
    }

    fn file_content_for_search<'a>(
        &'a self,
        rec: &FileRecord,
        scratch: &'a mut Vec<u8>,
    ) -> Result<&'a [u8]> {
        let bytes = checked_slice(
            &self.mmap,
            self.content_blob_offset as u64 + rec.content_offset,
            rec.content_size,
        )?;
        if self.version >= 3 {
            let (size, compressed) = lz4_flex::block::uncompressed_size(bytes)
                .context("failed to read indexed file content size")?;
            scratch.resize(size, 0);
            let written = lz4_flex::decompress_into(compressed, scratch)
                .context("failed to decompress indexed file content")?;
            scratch.truncate(written);
            Ok(scratch.as_slice())
        } else {
            Ok(bytes)
        }
    }

    fn file_compressed_content(&self, rec: &FileRecord) -> Result<Cow<'_, [u8]>> {
        let bytes = checked_slice(
            &self.mmap,
            self.content_blob_offset as u64 + rec.content_offset,
            rec.content_size,
        )?;
        if self.version >= 3 {
            Ok(Cow::Borrowed(bytes))
        } else {
            Ok(Cow::Owned(lz4_flex::compress_prepend_size(bytes)))
        }
    }

    fn file_content_for_id<'a>(&'a self, id: usize, rec: &FileRecord) -> Result<Cow<'a, [u8]>> {
        let _ = id;
        let bytes = checked_slice(
            &self.mmap,
            self.content_blob_offset as u64 + rec.content_offset,
            rec.content_size,
        )?;
        if self.version >= 3 {
            let content = lz4_flex::decompress_size_prepended(bytes)
                .context("failed to decompress indexed file content")?;
            Ok(Cow::Owned(content))
        } else {
            Ok(Cow::Borrowed(bytes))
        }
    }

    fn file_path(&self, id: usize) -> Result<&[u8]> {
        if id >= self.file_count {
            bail!("file id out of bounds");
        }
        let rec = self.file_record(id)?;
        checked_slice(
            &self.mmap,
            self.path_blob_offset as u64 + rec.path_offset,
            rec.path_size,
        )
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
                    return Ok(Some(PostingView {
                        data: self.posting_data_for_record(mid, &rec)?,
                    }));
                }
            }
        }
        Ok(None)
    }

    fn posting_data_for_record<'a>(
        &'a self,
        posting_id: usize,
        rec: &PostingRecord,
    ) -> Result<Cow<'a, [u32]>> {
        if self.version >= 4 {
            let start = self.postings_data_offset as u64 + rec.offset;
            let end = self.posting_data_end(posting_id)?;
            let bytes = checked_slice(&self.mmap, start, end - start)?;
            Ok(Cow::Owned(read_varint_postings(bytes, rec.count as usize)?))
        } else {
            let start = self.postings_data_offset as u64 + rec.offset * 4;
            let bytes = checked_slice(&self.mmap, start, rec.count * 4)?;
            let data = unsafe {
                std::slice::from_raw_parts(bytes.as_ptr() as *const u32, rec.count as usize)
            };
            Ok(Cow::Borrowed(data))
        }
    }

    fn posting_data_end(&self, posting_id: usize) -> Result<u64> {
        if posting_id + 1 < self.posting_count {
            Ok(self.postings_data_offset as u64 + self.posting_record(posting_id + 1)?.offset)
        } else {
            Ok(self.path_blob_offset as u64)
        }
    }

    fn posting_record(&self, id: usize) -> Result<PostingRecord> {
        let off = self.posting_table_offset + id * 24;
        Ok(PostingRecord {
            gram: read_u32_at(&self.mmap, off)?,
            offset: read_u64_at(&self.mmap, off + 8)?,
            count: read_u64_at(&self.mmap, off + 16)?,
        })
    }

    fn chunk_record(&self, id: usize) -> Result<ChunkRecord> {
        let info = self.chunk_info.context("index has no chunk extension")?;
        if id >= info.chunk_count {
            bail!("chunk id out of bounds");
        }
        let off = info.chunk_table_offset + id * 24;
        Ok(ChunkRecord {
            file_id: read_u32_at(&self.mmap, off)?,
            size: read_u32_at(&self.mmap, off + 4)?,
            start: read_u64_at(&self.mmap, off + 8)?,
            line_no: read_u32_at(&self.mmap, off + 16)?,
        })
    }

    fn chunk_range_for_file(&self, file_id: u32) -> Result<Option<(usize, usize)>> {
        let Some(info) = self.chunk_info else {
            return Ok(None);
        };
        let mut lo = 0usize;
        let mut hi = info.chunk_count;
        while lo < hi {
            let mid = (lo + hi) / 2;
            if self.chunk_record(mid)?.file_id < file_id {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        let start = lo;
        if start >= info.chunk_count || self.chunk_record(start)?.file_id != file_id {
            return Ok(None);
        }
        let mut lo = start;
        let mut hi = info.chunk_count;
        while lo < hi {
            let mid = (lo + hi) / 2;
            if self.chunk_record(mid)?.file_id <= file_id {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        Ok(Some((start, lo)))
    }

    fn chunk_bloom(&self, id: usize) -> Result<&[u8]> {
        let info = self.chunk_info.context("index has no chunk extension")?;
        if id >= info.chunk_count {
            bail!("chunk id out of bounds");
        }
        checked_slice(
            &self.mmap,
            info.chunk_blob_offset as u64 + (id * CHUNK_BLOOM_BYTES) as u64,
            CHUNK_BLOOM_BYTES as u64,
        )
    }

    fn chunk_posting(&self, gram: u32) -> Result<Option<PostingView<'_>>> {
        let Some(info) = self.chunk_info else {
            return Ok(None);
        };
        let mut lo = 0usize;
        let mut hi = info.chunk_posting_count;
        while lo < hi {
            let mid = (lo + hi) / 2;
            let rec = self.chunk_posting_record(mid)?;
            match rec.gram.cmp(&gram) {
                Ordering::Less => lo = mid + 1,
                Ordering::Greater => hi = mid,
                Ordering::Equal => {
                    let start = info.chunk_posting_data_offset as u64 + rec.offset;
                    let end = self.chunk_posting_data_end(mid)?;
                    let bytes = checked_slice(&self.mmap, start, end - start)?;
                    let data = read_varint_postings(bytes, rec.count as usize)?;
                    return Ok(Some(PostingView {
                        data: Cow::Owned(data),
                    }));
                }
            }
        }
        Ok(None)
    }

    fn chunk_posting_data_end(&self, posting_id: usize) -> Result<u64> {
        let info = self.chunk_info.context("index has no chunk extension")?;
        if posting_id + 1 < info.chunk_posting_count {
            Ok(info.chunk_posting_data_offset as u64
                + self.chunk_posting_record(posting_id + 1)?.offset)
        } else {
            Ok(info.chunk_blob_offset as u64)
        }
    }

    fn chunk_posting_record(&self, id: usize) -> Result<PostingRecord> {
        let info = self.chunk_info.context("index has no chunk extension")?;
        let off = info.chunk_posting_table_offset + id * 24;
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
    if !(2..=VERSION).contains(&version) {
        bail!("unsupported index version {version}");
    }
    Ok(Header {
        version,
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

fn parse_chunk_info(data: &[u8], version: u32) -> Result<Option<ChunkInfo>> {
    if version < 5 {
        return Ok(None);
    }
    if data.len() < CHUNK_FOOTER_SIZE {
        return Ok(None);
    }
    let footer = data.len() - CHUNK_FOOTER_SIZE;
    if &data[footer..footer + 8] != CHUNK_FOOTER_MAGIC {
        return Ok(None);
    }
    let chunk_count = read_u64_at(data, footer + 8)? as usize;
    let chunk_table_offset = read_u64_at(data, footer + 16)? as usize;
    let chunk_posting_count = read_u64_at(data, footer + 24)? as usize;
    let chunk_posting_table_offset = read_u64_at(data, footer + 32)? as usize;
    let chunk_posting_data_offset = read_u64_at(data, footer + 40)? as usize;
    let chunk_blob_offset = read_u64_at(data, footer + 48)? as usize;
    Ok(Some(ChunkInfo {
        chunk_count,
        chunk_table_offset,
        chunk_posting_count,
        chunk_posting_table_offset,
        chunk_posting_data_offset,
        chunk_blob_offset,
    }))
}

fn execute_search_rendered_segments(
    base: &MappedIndex,
    deltas: &[DeltaSegment],
    options: &Options,
    searched: &mut u64,
) -> Result<Vec<RenderedFileResult>> {
    let exclusions = segment_exclusions(base, deltas)?;
    let mut results = execute_search_rendered(base, options, searched, &exclusions[0])?;
    for (idx, delta) in deltas.iter().enumerate() {
        let mut segment_results =
            execute_search_rendered(&delta.index, options, searched, &exclusions[idx + 1])?;
        results.append(&mut segment_results);
    }
    if options.sort_path {
        results.sort_unstable_by(|a, b| a.path.cmp(&b.path));
    }
    Ok(results)
}

fn execute_search_rendered_segments_to_writer<W: Write>(
    base: &MappedIndex,
    deltas: &[DeltaSegment],
    options: &Options,
    searched: &mut u64,
    out: &mut W,
) -> Result<RenderStats> {
    if !deltas.is_empty() {
        let results = execute_search_rendered_segments(base, deltas, options, searched)?;
        if !options.quiet {
            write_rendered_results(out, &results, options)?;
        }
        return Ok(RenderStats {
            match_count: results.iter().map(|result| result.match_count).sum(),
            matched_files: results.len(),
        });
    }
    let exclusions = segment_exclusions(base, deltas)?;
    execute_search_rendered_to_writer(base, options, searched, &exclusions[0], out)
}

#[cold]
#[inline(never)]
fn execute_search_any_segments(
    base: &MappedIndex,
    deltas: &[DeltaSegment],
    options: &Options,
) -> Result<bool> {
    let exclusions = segment_exclusions(base, deltas)?;
    if execute_search_any(base, options, &exclusions[0])? {
        return Ok(true);
    }
    for (idx, delta) in deltas.iter().enumerate() {
        if execute_search_any(&delta.index, options, &exclusions[idx + 1])? {
            return Ok(true);
        }
    }
    Ok(false)
}

fn requires_all_candidate_files(options: &Options) -> bool {
    options.invert_match || options.files_without_match || options.include_zero
}

fn special_line_scan_requested(options: &Options) -> bool {
    options.invert_match
        || options.files_without_match
        || options.count_matches
        || options.include_zero
        || options.line_regexp
        || options.only_matching
}

fn candidate_file_ids(index: &MappedIndex, options: &Options) -> Result<Vec<u32>> {
    if requires_all_candidate_files(options) {
        return Ok((0..index.file_count as u32).collect());
    }
    let candidates = if let Some(prefixes) = whole_word_literal_prefixes(options) {
        if let Some(candidates) = prefix_candidate_files(index, &prefixes)? {
            candidates
        } else if let Some(candidates) = word_fragment_candidate_files(index, options)? {
            candidates
        } else {
            let alternatives = query_trigram_alternatives(options);
            candidate_files(index, &alternatives)?
        }
    } else if let Some(candidates) = word_fragment_candidate_files(index, options)? {
        candidates
    } else if let Some(prefixes) = boundary_word_prefixes(options) {
        if let Some(candidates) = prefix_candidate_files(index, &prefixes)? {
            candidates
        } else {
            let alternatives = query_trigram_alternatives(options);
            candidate_files(index, &alternatives)?
        }
    } else if let Some(spec) = qualified_call_spec(&options.pattern) {
        if let Some(candidates) = qualified_call_candidate_files(index, &spec)? {
            candidates
        } else {
            let alternatives = query_trigram_alternatives(options);
            candidate_files(index, &alternatives)?
        }
    } else {
        let alternatives = query_trigram_alternatives(options);
        candidate_files(index, &alternatives)?
    };
    chunk_bloom_filter_candidates(index, options, candidates)
}

fn search_candidate_files(
    index: &MappedIndex,
    options: &Options,
    excluded_paths: &HashSet<String>,
) -> Result<Vec<u32>> {
    let candidates = candidate_file_ids(index, options)?;
    if excluded_paths.is_empty() && !has_path_restriction(options, &index.root) {
        return Ok(candidates);
    }
    let path_filter = PathFilter::new(options, &index.root)?;
    if excluded_paths.is_empty() && path_filter.is_unrestricted() {
        return Ok(candidates);
    }
    Ok(candidates
        .into_iter()
        .filter_map(|id| {
            let path = bytes_to_string(index.file_path(id as usize).ok()?);
            path_filter.allows(&path, excluded_paths).then_some(id)
        })
        .collect())
}

#[cold]
#[inline(never)]
fn execute_search_any(
    index: &MappedIndex,
    options: &Options,
    excluded_paths: &HashSet<String>,
) -> Result<bool> {
    let candidates = search_candidate_files(index, options, excluded_paths)?;
    if candidates.is_empty() {
        return Ok(false);
    }
    let matcher = QueryMatcher::new(options)?;
    Ok(candidates.par_iter().any(|id| {
        let mut scratch = Vec::new();
        let Ok((_path_bytes, content)) = index.file_for_search(*id as usize, &mut scratch) else {
            return false;
        };
        matcher.search_file_has_match(content, options)
    }))
}

fn execute_search_rendered(
    index: &MappedIndex,
    options: &Options,
    searched: &mut u64,
    excluded_paths: &HashSet<String>,
) -> Result<Vec<RenderedFileResult>> {
    if !requires_all_candidate_files(options)
        && let Some(chunk_candidates) = candidate_chunks(index, options)?
    {
        return execute_search_rendered_chunks(
            index,
            options,
            searched,
            excluded_paths,
            chunk_candidates,
        );
    }

    let candidates = candidate_file_ids(index, options)?;
    let filtered = if excluded_paths.is_empty() && !has_path_restriction(options, &index.root) {
        candidates
    } else {
        let path_filter = PathFilter::new(options, &index.root)?;
        if excluded_paths.is_empty() && path_filter.is_unrestricted() {
            candidates
        } else {
            candidates
                .into_iter()
                .filter_map(|id| {
                    let path = bytes_to_string(index.file_path(id as usize).ok()?);
                    path_filter.allows(&path, excluded_paths).then_some(id)
                })
                .collect()
        }
    };
    *searched += filtered.len() as u64;

    let matcher = QueryMatcher::new(options)?;
    let show_path = should_show_path(options);
    let display_path = DisplayPathMapper::new(&index.root, options);
    let mut results: Vec<(usize, RenderedFileResult)> = filtered
        .par_iter()
        .enumerate()
        .map_init(Vec::new, |scratch, (ordinal, id)| {
            let (path_bytes, content) = index.file_for_search(*id as usize, scratch).ok()?;
            let rel_path = bytes_to_string(path_bytes);
            let path = display_path.display(&rel_path);
            let rendered = matcher
                .search_file_rendered(content, &path, options, show_path)
                .ok()??;
            Some((ordinal, rendered))
        })
        .filter_map(|result| result)
        .collect();
    if options.sort_path {
        results.sort_unstable_by(|(_, left), (_, right)| left.path.cmp(&right.path));
    } else {
        results.sort_by_key(|(ordinal, _)| *ordinal);
    }
    Ok(results.into_iter().map(|(_, result)| result).collect())
}

fn execute_search_rendered_to_writer<W: Write>(
    index: &MappedIndex,
    options: &Options,
    searched: &mut u64,
    excluded_paths: &HashSet<String>,
    out: &mut W,
) -> Result<RenderStats> {
    if !requires_all_candidate_files(options)
        && let Some(chunk_candidates) = candidate_chunks(index, options)?
    {
        return execute_search_rendered_chunks_to_writer(
            index,
            options,
            searched,
            excluded_paths,
            chunk_candidates,
            out,
        );
    }

    let candidates = candidate_file_ids(index, options)?;
    let mut filtered = if excluded_paths.is_empty() && !has_path_restriction(options, &index.root) {
        candidates
    } else {
        let path_filter = PathFilter::new(options, &index.root)?;
        if excluded_paths.is_empty() && path_filter.is_unrestricted() {
            candidates
        } else {
            candidates
                .into_iter()
                .filter_map(|id| {
                    let path = bytes_to_string(index.file_path(id as usize).ok()?);
                    path_filter.allows(&path, excluded_paths).then_some(id)
                })
                .collect()
        }
    };
    *searched += filtered.len() as u64;
    if filtered.len() < STREAMING_PIPELINE_MIN_CANDIDATES {
        return collect_file_ids_to_writer(index, &filtered, options, out);
    }
    if options.sort_path {
        sort_file_ids_by_path(index, &mut filtered);
    }

    let matcher = QueryMatcher::new(options)?;
    let show_path = should_show_path(options);
    let display_path = DisplayPathMapper::new(&index.root, options);
    let separated = grouped_heading_output(options, show_path);
    let (tx, rx) = mpsc::channel();
    let total = filtered.len();
    std::thread::scope(|scope| {
        scope.spawn(|| {
            filtered
                .par_iter()
                .enumerate()
                .for_each_with(tx, |tx, (ordinal, id)| {
                    let mut scratch = Vec::new();
                    let result = index
                        .file_for_search(*id as usize, &mut scratch)
                        .ok()
                        .and_then(|(path_bytes, content)| {
                            let rel_path = bytes_to_string(path_bytes);
                            let path = display_path.display(&rel_path);
                            matcher
                                .search_file_rendered(content, &path, options, show_path)
                                .ok()
                                .flatten()
                        });
                    let _ = tx.send((ordinal, result));
                });
        });
        consume_ordered_rendered_results(rx, total, separated, out)
    })
}

fn chunk_bloom_filter_candidates(
    index: &MappedIndex,
    options: &Options,
    candidates: Vec<u32>,
) -> Result<Vec<u32>> {
    if index.chunk_info.is_none() || candidates.len() < 2 {
        return Ok(candidates);
    }
    let Some(alternatives) = safe_chunk_trigram_alternatives(options) else {
        return Ok(candidates);
    };
    if alternatives.is_empty() || alternatives.iter().any(|grams| grams.is_empty()) {
        return Ok(candidates);
    }

    let mut filtered = Vec::with_capacity(candidates.len());
    for id in candidates {
        if file_may_match_chunk_bloom(index, id, &alternatives)? {
            filtered.push(id);
        }
    }
    Ok(filtered)
}

fn file_may_match_chunk_bloom(
    index: &MappedIndex,
    file_id: u32,
    alternatives: &[Vec<u32>],
) -> Result<bool> {
    let Some((start, end)) = index.chunk_range_for_file(file_id)? else {
        return Ok(true);
    };
    for chunk_id in start..end {
        let bloom = index.chunk_bloom(chunk_id)?;
        for grams in alternatives {
            if grams.iter().all(|&gram| chunk_bloom_maybe(bloom, gram)) {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

fn execute_search_rendered_chunks(
    index: &MappedIndex,
    options: &Options,
    searched: &mut u64,
    excluded_paths: &HashSet<String>,
    chunk_candidates: Vec<u32>,
) -> Result<Vec<RenderedFileResult>> {
    let mut by_file: BTreeMap<u32, Vec<usize>> = BTreeMap::new();
    for chunk_id in chunk_candidates {
        let rec = index.chunk_record(chunk_id as usize)?;
        by_file
            .entry(rec.file_id)
            .or_default()
            .push(chunk_id as usize);
    }

    let filtered: Vec<(u32, Vec<usize>)> =
        if excluded_paths.is_empty() && !has_path_restriction(options, &index.root) {
            by_file.into_iter().collect()
        } else {
            let path_filter = PathFilter::new(options, &index.root)?;
            if excluded_paths.is_empty() && path_filter.is_unrestricted() {
                by_file.into_iter().collect()
            } else {
                by_file
                    .into_iter()
                    .filter_map(|(file_id, chunks)| {
                        let path = bytes_to_string(index.file_path(file_id as usize).ok()?);
                        path_filter
                            .allows(&path, excluded_paths)
                            .then_some((file_id, chunks))
                    })
                    .collect()
            }
        };
    *searched += filtered.len() as u64;

    let matcher = QueryMatcher::new(options)?;
    let show_path = should_show_path(options);
    let display_path = DisplayPathMapper::new(&index.root, options);
    let mut results: Vec<(u32, RenderedFileResult)> = filtered
        .par_iter()
        .map_init(Vec::new, |scratch, (file_id, chunks)| {
            let (path_bytes, content) = index.file_for_search(*file_id as usize, scratch).ok()?;
            let rel_path = bytes_to_string(path_bytes);
            let path = display_path.display(&rel_path);
            let rendered = matcher
                .search_file_rendered(content, &path, options, show_path)
                .ok()??;
            let _ = chunks;
            Some((*file_id, rendered))
        })
        .filter_map(|result| result)
        .collect();
    if options.sort_path {
        results.sort_unstable_by(|(_, left), (_, right)| left.path.cmp(&right.path));
    } else {
        results.sort_by_key(|(file_id, _)| *file_id);
    }
    Ok(results.into_iter().map(|(_, result)| result).collect())
}

fn execute_search_rendered_chunks_to_writer<W: Write>(
    index: &MappedIndex,
    options: &Options,
    searched: &mut u64,
    excluded_paths: &HashSet<String>,
    chunk_candidates: Vec<u32>,
    out: &mut W,
) -> Result<RenderStats> {
    let mut by_file: BTreeMap<u32, Vec<usize>> = BTreeMap::new();
    for chunk_id in chunk_candidates {
        let rec = index.chunk_record(chunk_id as usize)?;
        by_file
            .entry(rec.file_id)
            .or_default()
            .push(chunk_id as usize);
    }

    let mut filtered: Vec<(u32, Vec<usize>)> =
        if excluded_paths.is_empty() && !has_path_restriction(options, &index.root) {
            by_file.into_iter().collect()
        } else {
            let path_filter = PathFilter::new(options, &index.root)?;
            if excluded_paths.is_empty() && path_filter.is_unrestricted() {
                by_file.into_iter().collect()
            } else {
                by_file
                    .into_iter()
                    .filter_map(|(file_id, chunks)| {
                        let path = bytes_to_string(index.file_path(file_id as usize).ok()?);
                        path_filter
                            .allows(&path, excluded_paths)
                            .then_some((file_id, chunks))
                    })
                    .collect()
            }
        };
    *searched += filtered.len() as u64;
    if filtered.len() < STREAMING_PIPELINE_MIN_CANDIDATES {
        let file_ids: Vec<u32> = filtered.iter().map(|(file_id, _)| *file_id).collect();
        return collect_file_ids_to_writer(index, &file_ids, options, out);
    }
    if options.sort_path {
        sort_file_entries_by_path(index, &mut filtered);
    }

    let matcher = QueryMatcher::new(options)?;
    let show_path = should_show_path(options);
    let display_path = DisplayPathMapper::new(&index.root, options);
    let separated = grouped_heading_output(options, show_path);
    let (tx, rx) = mpsc::channel();
    let total = filtered.len();
    std::thread::scope(|scope| {
        scope.spawn(|| {
            filtered.par_iter().enumerate().for_each_with(
                tx,
                |tx, (ordinal, (file_id, chunks))| {
                    let mut scratch = Vec::new();
                    let result = index
                        .file_for_search(*file_id as usize, &mut scratch)
                        .ok()
                        .and_then(|(path_bytes, content)| {
                            let rel_path = bytes_to_string(path_bytes);
                            let path = display_path.display(&rel_path);
                            let rendered = matcher
                                .search_file_rendered(content, &path, options, show_path)
                                .ok()
                                .flatten();
                            let _ = chunks;
                            rendered
                        });
                    let _ = tx.send((ordinal, result));
                },
            );
        });
        consume_ordered_rendered_results(rx, total, separated, out)
    })
}

fn collect_file_ids_to_writer<W: Write>(
    index: &MappedIndex,
    file_ids: &[u32],
    options: &Options,
    out: &mut W,
) -> Result<RenderStats> {
    let matcher = QueryMatcher::new(options)?;
    let show_path = should_show_path(options);
    let display_path = DisplayPathMapper::new(&index.root, options);
    let mut results: Vec<(usize, RenderedFileResult)> = file_ids
        .par_iter()
        .enumerate()
        .map_init(Vec::new, |scratch, id| {
            let (ordinal, id) = id;
            let (path_bytes, content) = index.file_for_search(*id as usize, scratch).ok()?;
            let rel_path = bytes_to_string(path_bytes);
            let path = display_path.display(&rel_path);
            let rendered = matcher
                .search_file_rendered(content, &path, options, show_path)
                .ok()??;
            Some((ordinal, rendered))
        })
        .filter_map(|result| result)
        .collect();
    if options.sort_path {
        results.sort_unstable_by(|(_, left), (_, right)| left.path.cmp(&right.path));
    } else {
        results.sort_by_key(|(ordinal, _)| *ordinal);
    }
    let stats = RenderStats {
        match_count: results.iter().map(|(_, result)| result.match_count).sum(),
        matched_files: results.len(),
    };
    if !options.quiet {
        let separated = grouped_heading_output(options, show_path);
        for (idx, (_, result)) in results.iter().enumerate() {
            if separated && idx != 0 {
                writeln!(out)?;
            }
            out.write_all(&result.output)?;
        }
    }
    Ok(stats)
}

fn sort_file_ids_by_path(index: &MappedIndex, file_ids: &mut [u32]) {
    file_ids.sort_unstable_by(|left, right| compare_index_paths(index, *left, *right));
}

fn sort_file_entries_by_path(index: &MappedIndex, files: &mut [(u32, Vec<usize>)]) {
    files.sort_unstable_by(|left, right| compare_index_paths(index, left.0, right.0));
}

fn compare_index_paths(index: &MappedIndex, left: u32, right: u32) -> Ordering {
    let left_path = index.file_path(left as usize).unwrap_or_default();
    let right_path = index.file_path(right as usize).unwrap_or_default();
    left_path.cmp(right_path)
}

fn consume_ordered_rendered_results<W: Write>(
    rx: mpsc::Receiver<(usize, Option<RenderedFileResult>)>,
    total: usize,
    separated: bool,
    out: &mut W,
) -> Result<RenderStats> {
    let mut pending: BTreeMap<usize, Option<RenderedFileResult>> = BTreeMap::new();
    let mut next = 0usize;
    let mut stats = RenderStats::default();
    for _ in 0..total {
        let (ordinal, result) = rx.recv()?;
        pending.insert(ordinal, result);
        while let Some(result) = pending.remove(&next) {
            if let Some(result) = result {
                if separated && stats.matched_files != 0 {
                    writeln!(out)?;
                }
                out.write_all(&result.output)?;
                stats.match_count += result.match_count;
                stats.matched_files += 1;
            }
            next += 1;
        }
    }
    Ok(stats)
}

enum QueryMatcher {
    Fixed {
        needle: Vec<u8>,
        finder: memmem::Finder<'static>,
        ac: Option<AhoCorasick>,
        whole_word: bool,
        ignore_case: bool,
        line_regexp: bool,
    },
    WordPrefix {
        ac: AhoCorasick,
        boundary_start: bool,
        boundary_end: bool,
    },
    LiteralSet {
        ac: AhoCorasick,
    },
    QualifiedCall {
        spec: QualifiedCallSpec,
        finder: memmem::Finder<'static>,
    },
    OrderedLiterals {
        literals: Vec<Vec<u8>>,
        finder: Option<memmem::Finder<'static>>,
        ignore_case: bool,
    },
    OrderedWordSpanLiterals {
        literals: Vec<Vec<u8>>,
        finder: Option<memmem::Finder<'static>>,
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
                line_regexp: options.line_regexp,
            });
        }
        if options.line_regexp {
            let regex = RegexBuilder::new(&format!("^(?:{})$", options.pattern))
                .case_insensitive(options.ignore_case)
                .multi_line(true)
                .build()?;
            return Ok(Self::Regex(regex));
        }
        if let Some(word_prefix) = word_prefix_regex(options)? {
            return Ok(word_prefix);
        }
        if let Some(literals) = exact_parenthesized_literal_alternatives(&options.pattern) {
            let ac = AhoCorasickBuilder::new()
                .ascii_case_insensitive(options.ignore_case)
                .match_kind(MatchKind::LeftmostFirst)
                .build(literals)?;
            return Ok(Self::LiteralSet { ac });
        }
        if let Some(qualified_call) = qualified_call_regex(options) {
            return Ok(qualified_call);
        }
        if let Some(literals) = ordered_dotstar_literals(options) {
            let finder = (!options.ignore_case && !literals.is_empty())
                .then(|| memmem::Finder::new(&literals[0]).into_owned());
            return Ok(Self::OrderedLiterals {
                literals,
                finder,
                ignore_case: options.ignore_case,
            });
        }
        if let Some(literals) = ordered_wordspan_literals(options) {
            let finder = (!options.ignore_case && !literals.is_empty())
                .then(|| memmem::Finder::new(&literals[0]).into_owned());
            return Ok(Self::OrderedWordSpanLiterals {
                literals,
                finder,
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

    fn search_file_rendered(
        &self,
        content: &[u8],
        path: &str,
        options: &Options,
        show_path: bool,
    ) -> Result<Option<RenderedFileResult>> {
        if special_line_scan_requested(options) {
            return self.search_file_line_scan_rendered(content, path, options, show_path);
        }
        if context_requested(options) && !options.json && !options.vimgrep {
            let matches = self.search_file(content, options);
            if matches.is_empty() {
                return Ok(None);
            }
            let output =
                render_file_result_with_content(path, content, &matches, options, show_path)?;
            return Ok(Some(RenderedFileResult {
                path: path.to_string(),
                output,
                match_count: matches.len(),
            }));
        }
        if let QueryMatcher::Fixed {
            needle,
            finder,
            ac,
            whole_word,
            ignore_case,
            ..
        } = self
        {
            return search_fixed_rendered(
                content,
                path,
                needle,
                finder,
                ac.as_ref(),
                *whole_word,
                *ignore_case,
                options,
                show_path,
            );
        }
        if let QueryMatcher::WordPrefix {
            ac,
            boundary_start,
            boundary_end,
        } = self
        {
            return search_word_prefix_rendered(
                content,
                path,
                ac,
                *boundary_start,
                *boundary_end,
                options,
                show_path,
            );
        }
        if let QueryMatcher::LiteralSet { ac } = self {
            return search_literal_set_rendered(content, path, ac, options, show_path);
        }
        if let QueryMatcher::QualifiedCall { spec, finder } = self {
            return search_qualified_call_rendered(content, path, spec, finder, options, show_path);
        }
        if let QueryMatcher::OrderedLiterals {
            literals,
            finder: Some(finder),
            ignore_case: false,
        } = self
        {
            return search_ordered_literals_rendered(
                content, path, literals, finder, options, show_path,
            );
        }
        if let QueryMatcher::OrderedWordSpanLiterals {
            literals,
            finder: Some(finder),
            ignore_case: false,
        } = self
        {
            return search_ordered_wordspan_rendered(
                content, path, literals, finder, options, show_path,
            );
        }

        let matches = self.search_file(content, options);
        if matches.is_empty() {
            return Ok(None);
        }
        let output = render_file_result(path, &matches, options, show_path)?;
        Ok(Some(RenderedFileResult {
            path: path.to_string(),
            output,
            match_count: matches.len(),
        }))
    }

    #[cold]
    #[inline(never)]
    fn search_file_has_match(&self, content: &[u8], options: &Options) -> bool {
        if special_line_scan_requested(options) {
            return self.search_file_line_scan_has_match(content, options);
        }
        match self {
            QueryMatcher::Fixed {
                needle,
                finder,
                ac,
                whole_word,
                ignore_case,
                ..
            } => {
                if needle.is_empty() {
                    return false;
                }
                if *ignore_case {
                    let Some(ac) = ac else {
                        return false;
                    };
                    return ac.find_iter(content).any(|found| {
                        !*whole_word || content_word_boundary(content, found.start(), found.end())
                    });
                }
                finder.find_iter(content).any(|start| {
                    !*whole_word || content_word_boundary(content, start, start + needle.len())
                })
            }
            QueryMatcher::WordPrefix {
                ac,
                boundary_start,
                boundary_end,
            } => ac.find_iter(content).any(|found| {
                let start = found.start();
                if *boundary_start && start > 0 && is_word_byte(content[start - 1]) {
                    return false;
                }
                let mut end = found.end();
                while end < content.len() && is_word_byte(content[end]) {
                    end += 1;
                }
                !(*boundary_end && end < content.len() && is_word_byte(content[end]))
            }),
            QueryMatcher::LiteralSet { ac } => ac.is_match(content),
            QueryMatcher::QualifiedCall { spec, finder } => {
                qualified_call_has_match(content, spec, finder)
            }
            QueryMatcher::OrderedLiterals {
                literals,
                finder: Some(finder),
                ignore_case: false,
            } => ordered_literals_has_match(content, literals, finder),
            QueryMatcher::OrderedWordSpanLiterals {
                literals,
                finder: Some(finder),
                ignore_case: false,
            } => ordered_wordspan_has_match(content, literals, finder),
            QueryMatcher::Regex(regex) => {
                let mut found = false;
                for_each_line(content, |_, line| {
                    found = regex.is_match(line);
                    !found
                });
                found
            }
            _ => !self.search_file(content, options).is_empty(),
        }
    }

    fn search_file_line_scan_has_match(&self, content: &[u8], options: &Options) -> bool {
        let mut selected_lines = 0usize;
        for_each_line(content, |_, line| {
            let spans = self.line_match_spans(line, options);
            let selected = if options.invert_match {
                spans.is_empty()
            } else {
                !spans.is_empty()
            };
            if selected {
                selected_lines += 1;
                if !options.files_without_match {
                    return false;
                }
            }
            true
        });
        if options.files_without_match {
            selected_lines == 0
        } else {
            selected_lines != 0
        }
    }

    fn search_file_line_scan_rendered(
        &self,
        content: &[u8],
        path: &str,
        options: &Options,
        show_path: bool,
    ) -> Result<Option<RenderedFileResult>> {
        let mut output = Vec::new();
        let mut path_written = false;
        let mut selected_lines = 0usize;
        let mut match_count = 0usize;
        for_each_line(content, |line_no, line| {
            let spans = self.line_match_spans(line, options);
            let selected = if options.invert_match {
                spans.is_empty()
            } else {
                !spans.is_empty()
            };
            if !selected {
                return true;
            }
            selected_lines += 1;
            let line_match_count = if options.count_matches && !options.invert_match {
                spans.len()
            } else {
                1
            };
            match_count += line_match_count;
            if options.files_without_match || options.count {
                return options.max_count.is_none_or(|max| selected_lines < max);
            }
            if options.files_with_matches {
                if !path_written {
                    let _ = writeln!(output, "{path}");
                    path_written = true;
                }
                return !files_with_matches_can_stop(options);
            }
            let mut render_one = |start: usize, end: usize| -> Result<()> {
                let match_show_path =
                    prepare_match_render(&mut output, path, options, show_path, &mut path_written)?;
                render_match_to(
                    &mut output,
                    path,
                    line_no,
                    start + 1,
                    line,
                    &line[start..end.min(line.len())],
                    options,
                    match_show_path,
                )
            };
            if options.invert_match {
                if let Err(_err) = render_one(0, 0) {
                    return false;
                }
            } else if options.only_matching {
                for (start, end) in spans {
                    if let Err(_err) = render_one(start, end) {
                        return false;
                    }
                }
            } else if let Some((start, end)) = spans.first().copied() {
                if let Err(_err) = render_one(start, end) {
                    return false;
                }
            }
            options.max_count.is_none_or(|max| selected_lines < max)
        });

        if options.files_without_match {
            if selected_lines == 0 {
                writeln!(output, "{path}")?;
                return Ok(Some(RenderedFileResult {
                    path: path.to_string(),
                    output,
                    match_count: 0,
                }));
            }
            return Ok(None);
        }
        if selected_lines == 0 && !(options.count && options.include_zero) {
            return Ok(None);
        }
        if options.count {
            if show_path {
                write!(output, "{path}:")?;
            }
            writeln!(output, "{match_count}")?;
        }
        Ok(Some(RenderedFileResult {
            path: path.to_string(),
            output,
            match_count,
        }))
    }

    fn line_match_spans(&self, line: &[u8], options: &Options) -> Vec<(usize, usize)> {
        match self {
            QueryMatcher::Fixed {
                needle,
                finder,
                ac,
                whole_word,
                ignore_case,
                line_regexp,
            } => {
                if needle.is_empty() {
                    return Vec::new();
                }
                if *line_regexp {
                    let matched = if *ignore_case {
                        line.eq_ignore_ascii_case(options.pattern.as_bytes())
                    } else {
                        line == needle.as_slice()
                    };
                    return matched.then_some((0, line.len())).into_iter().collect();
                }
                if *ignore_case {
                    let Some(ac) = ac else {
                        return Vec::new();
                    };
                    return ac
                        .find_iter(line)
                        .filter_map(|found| {
                            (!*whole_word || word_boundary(line, found.start(), found.len()))
                                .then_some((found.start(), found.end()))
                        })
                        .collect();
                }
                finder
                    .find_iter(line)
                    .filter_map(|start| {
                        let end = start + needle.len();
                        (!*whole_word || word_boundary(line, start, needle.len()))
                            .then_some((start, end))
                    })
                    .collect()
            }
            QueryMatcher::WordPrefix {
                ac,
                boundary_start,
                boundary_end,
            } => ac
                .find_iter(line)
                .filter_map(|found| {
                    let start = found.start();
                    if *boundary_start && start > 0 && is_word_byte(line[start - 1]) {
                        return None;
                    }
                    let mut end = found.end();
                    while end < line.len() && is_word_byte(line[end]) {
                        end += 1;
                    }
                    if *boundary_end && end < line.len() && is_word_byte(line[end]) {
                        return None;
                    }
                    Some((start, end))
                })
                .collect(),
            QueryMatcher::LiteralSet { ac } => ac
                .find_iter(line)
                .map(|found| (found.start(), found.end()))
                .collect(),
            QueryMatcher::QualifiedCall { spec, finder } => {
                let mut spans = Vec::new();
                for found in finder.find_iter(line) {
                    if found == 0 || found + 2 >= line.len() {
                        continue;
                    }
                    let token_start = rewind_word(line, found);
                    let Some(class_start) =
                        qualified_call_match_start(line, token_start, found, spec)
                    else {
                        continue;
                    };
                    let method_start = found + 2;
                    if method_start >= line.len() || !is_word_byte(line[method_start]) {
                        continue;
                    }
                    let method_end = advance_word(line, method_start);
                    if method_end < line.len() && line[method_end] == b'(' {
                        spans.push((class_start, method_end + 1));
                    }
                }
                spans
            }
            QueryMatcher::OrderedLiterals {
                literals,
                finder,
                ignore_case,
            } => ordered_literal_line_spans(line, literals, finder.as_ref(), *ignore_case),
            QueryMatcher::OrderedWordSpanLiterals {
                literals,
                finder,
                ignore_case,
            } => ordered_wordspan_line_spans(line, literals, finder.as_ref(), *ignore_case),
            QueryMatcher::Regex(regex) => regex
                .find_iter(line)
                .map(|found| (found.start(), found.end()))
                .collect(),
        }
    }

    fn search_file(&self, content: &[u8], options: &Options) -> Vec<MatchLine> {
        if let QueryMatcher::Fixed {
            needle,
            finder,
            whole_word: false,
            ignore_case: false,
            line_regexp: false,
            ..
        } = self
        {
            return search_fixed_content(content, finder, needle.len(), options);
        }
        if let QueryMatcher::WordPrefix {
            ac,
            boundary_start,
            boundary_end,
        } = self
        {
            return search_word_prefix_content(
                content,
                ac,
                *boundary_start,
                *boundary_end,
                options,
            );
        }
        if let QueryMatcher::QualifiedCall { spec, finder } = self {
            return search_qualified_call_content(content, spec, finder, options);
        }

        let mut matches = Vec::new();
        for_each_line(content, |line_no, line| {
            match self {
                QueryMatcher::Fixed {
                    needle,
                    finder,
                    ac,
                    whole_word,
                    ignore_case,
                    ..
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
                QueryMatcher::WordPrefix { .. } => unreachable!("handled before line scan"),
                QueryMatcher::LiteralSet { .. } => unreachable!("handled before line scan"),
                QueryMatcher::QualifiedCall { .. } => unreachable!("handled before line scan"),
                QueryMatcher::OrderedLiterals {
                    literals,
                    finder: _,
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
                QueryMatcher::OrderedWordSpanLiterals {
                    literals,
                    finder: _,
                    ignore_case,
                } => {
                    let lowered;
                    let haystack = if *ignore_case {
                        lowered = lower_bytes(line);
                        lowered.as_slice()
                    } else {
                        line
                    };
                    if let Some((start, end)) = find_ordered_wordspan_match(haystack, literals) {
                        matches.push(MatchLine {
                            line_no,
                            column: start + 1,
                            line: line.to_vec(),
                            matched: line[start..end.min(line.len())].to_vec(),
                        });
                    }
                }
            }
            options.max_count.is_none_or(|max| matches.len() < max)
        });
        matches
    }
}

fn word_prefix_regex(options: &Options) -> Result<Option<QueryMatcher>> {
    if options.fixed || options.whole_word || options.pattern.is_empty() {
        return Ok(None);
    }
    let mut pattern = options.pattern.as_str();
    let boundary_start = pattern.strip_prefix(r"\b").is_some();
    if boundary_start {
        pattern = &pattern[2..];
    }
    let boundary_end = pattern.strip_suffix(r"\b").is_some();
    if boundary_end {
        pattern = &pattern[..pattern.len() - 2];
    }
    let Some(prefix_part) = pattern.strip_suffix("[A-Za-z0-9_]*") else {
        return Ok(None);
    };
    let prefixes = if prefix_part.starts_with('(') && prefix_part.ends_with(')') {
        let body = &prefix_part[1..prefix_part.len() - 1];
        if body.is_empty() || !body.contains('|') {
            return Ok(None);
        }
        let mut out = Vec::new();
        for part in body.split('|') {
            if !is_ascii_word_literal(part) {
                return Ok(None);
            }
            out.push(part.as_bytes().to_vec());
        }
        out
    } else if is_ascii_word_literal(prefix_part) {
        vec![prefix_part.as_bytes().to_vec()]
    } else {
        return Ok(None);
    };
    let ac = AhoCorasickBuilder::new()
        .ascii_case_insensitive(options.ignore_case)
        .match_kind(MatchKind::LeftmostFirst)
        .build(prefixes)?;
    Ok(Some(QueryMatcher::WordPrefix {
        ac,
        boundary_start,
        boundary_end,
    }))
}

fn boundary_word_prefixes(options: &Options) -> Option<Vec<Vec<u8>>> {
    let mut pattern = options.pattern.as_str();
    pattern = pattern.strip_prefix(r"\b")?;
    pattern = pattern.strip_suffix(r"\b")?;
    let prefix_part = pattern.strip_suffix("[A-Za-z0-9_]*")?;
    let prefixes = if prefix_part.starts_with('(') && prefix_part.ends_with(')') {
        let body = &prefix_part[1..prefix_part.len() - 1];
        if body.is_empty() || !body.contains('|') {
            return None;
        }
        let mut out = Vec::new();
        for part in body.split('|') {
            if !is_ascii_word_literal(part) || part.len() < PREFIX_MIN_LEN {
                return None;
            }
            out.push(lower_bytes(part.as_bytes()));
        }
        out
    } else if is_ascii_word_literal(prefix_part) && prefix_part.len() >= PREFIX_MIN_LEN {
        vec![lower_bytes(prefix_part.as_bytes())]
    } else {
        return None;
    };
    Some(prefixes)
}

fn whole_word_literal_prefixes(options: &Options) -> Option<Vec<Vec<u8>>> {
    if !options.whole_word || options.ignore_case && !options.pattern.is_ascii() {
        return None;
    }
    if !(options.fixed || regex_meta_free(&options.pattern)) {
        return None;
    }
    if !is_ascii_word_literal(&options.pattern) || options.pattern.len() < PREFIX_MIN_LEN {
        return None;
    }
    Some(vec![lower_bytes(options.pattern.as_bytes())])
}

fn qualified_call_regex(options: &Options) -> Option<QueryMatcher> {
    if options.ignore_case || options.fixed || options.whole_word {
        return None;
    }
    let spec = qualified_call_spec(&options.pattern)?;
    Some(QueryMatcher::QualifiedCall {
        spec,
        finder: memmem::Finder::new("::").into_owned(),
    })
}

fn qualified_call_spec(pattern: &str) -> Option<QualifiedCallSpec> {
    let (class_part, method_part) = pattern.split_once("::")?;
    if method_part != r"[A-Za-z0-9_]+\(" && method_part != r"[A-Za-z_][A-Za-z0-9_]*\(" {
        return None;
    }
    let (class_prefix, class_min_extra) = identifier_pattern_prefix(class_part)?;
    Some(QualifiedCallSpec {
        class_prefix,
        class_min_extra,
    })
}

fn identifier_pattern_prefix(pattern: &str) -> Option<(Option<Vec<u8>>, usize)> {
    for suffix in [
        "[A-Za-z0-9_]+",
        "[A-Za-z_][A-Za-z0-9_]*",
        "[A-Za-z][A-Za-z0-9_]*",
        "[A-Z][A-Za-z0-9_]*",
        "[a-z][A-Za-z0-9_]*",
    ] {
        if pattern == suffix {
            return Some((None, 0));
        }
        if let Some(prefix) = pattern.strip_suffix(suffix) {
            if is_ascii_word_literal(prefix) {
                return Some((Some(prefix.as_bytes().to_vec()), 1));
            }
        }
    }
    None
}

fn is_ascii_word_literal(text: &str) -> bool {
    !text.is_empty()
        && text
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
}

struct LineCursor<'a> {
    content: &'a [u8],
    line_no: usize,
    line_start: usize,
    line_end: usize,
}

impl<'a> LineCursor<'a> {
    fn new(content: &'a [u8]) -> Self {
        Self {
            content,
            line_no: 1,
            line_start: 0,
            line_end: next_line_end(content, 0),
        }
    }

    fn advance_to(&mut self, pos: usize) {
        while self.line_end < self.content.len() && pos > self.line_end {
            self.line_no += 1;
            self.line_start = self.line_end + 1;
            self.line_end = next_line_end(self.content, self.line_start);
        }
    }

    fn line(&self) -> &'a [u8] {
        let mut line = &self.content[self.line_start..self.line_end];
        if line.ends_with(b"\r") {
            line = &line[..line.len() - 1];
        }
        line
    }
}

fn next_line_end(content: &[u8], start: usize) -> usize {
    memchr(b'\n', &content[start..])
        .map(|pos| start + pos)
        .unwrap_or(content.len())
}

fn finish_rendered_match_output(
    mut output: Vec<u8>,
    match_count: usize,
    path: &str,
    options: &Options,
    show_path: bool,
) -> Result<Option<RenderedFileResult>> {
    if match_count == 0 {
        return Ok(None);
    }
    if options.count {
        if show_path {
            write!(output, "{path}:")?;
        }
        writeln!(output, "{match_count}")?;
    }
    Ok(Some(RenderedFileResult {
        path: path.to_string(),
        output,
        match_count,
    }))
}

fn search_fixed_content(
    content: &[u8],
    finder: &memmem::Finder<'_>,
    needle_len: usize,
    options: &Options,
) -> Vec<MatchLine> {
    if needle_len == 0 {
        return Vec::new();
    }
    let mut matches = Vec::new();
    let mut last_line_start = usize::MAX;
    let mut line_scan_pos = 0usize;
    let mut line_no = 1usize;
    for start in finder.find_iter(content) {
        let line_start = memrchr(b'\n', &content[..start]).map_or(0, |pos| pos + 1);
        if line_start == last_line_start {
            continue;
        }
        while line_scan_pos < line_start {
            if let Some(pos) = memchr(b'\n', &content[line_scan_pos..line_start]) {
                line_no += 1;
                line_scan_pos += pos + 1;
            } else {
                line_scan_pos = line_start;
            }
        }
        let line_end = memchr(b'\n', &content[start..])
            .map(|pos| start + pos)
            .unwrap_or(content.len());
        let mut line = &content[line_start..line_end];
        if line.ends_with(b"\r") {
            line = &line[..line.len() - 1];
        }
        let end = (start + needle_len).min(content.len());
        matches.push(MatchLine {
            line_no,
            column: start.saturating_sub(line_start) + 1,
            line: line.to_vec(),
            matched: content[start..end].to_vec(),
        });
        last_line_start = line_start;
        if options.max_count.is_some_and(|max| matches.len() >= max) {
            break;
        }
    }
    matches
}

#[allow(clippy::too_many_arguments)]
fn search_fixed_rendered(
    content: &[u8],
    path: &str,
    needle: &[u8],
    finder: &memmem::Finder<'_>,
    ac: Option<&AhoCorasick>,
    whole_word: bool,
    ignore_case: bool,
    options: &Options,
    show_path: bool,
) -> Result<Option<RenderedFileResult>> {
    if needle.is_empty() {
        return Ok(None);
    }
    let mut output = Vec::new();
    let mut match_count = 0usize;
    let mut path_written = false;
    let mut last_line_start = usize::MAX;
    let mut cursor = LineCursor::new(content);

    let mut handle_match = |start: usize, end: usize| -> Result<bool> {
        cursor.advance_to(start);
        if cursor.line_start == last_line_start {
            return Ok(true);
        }
        if whole_word && !word_boundary(cursor.line(), start - cursor.line_start, end - start) {
            return Ok(true);
        }
        match_count += 1;
        if !options.count && !options.files_with_matches {
            let match_show_path =
                prepare_match_render(&mut output, path, options, show_path, &mut path_written)?;
            render_match_to(
                &mut output,
                path,
                cursor.line_no,
                start.saturating_sub(cursor.line_start) + 1,
                cursor.line(),
                &content[start..end],
                options,
                match_show_path,
            )?;
        } else if options.files_with_matches && !path_written {
            writeln!(output, "{path}")?;
            path_written = true;
            if files_with_matches_can_stop(options) {
                return Ok(false);
            }
        }
        last_line_start = cursor.line_start;
        Ok(options.max_count.is_none_or(|max| match_count < max))
    };

    if ignore_case {
        let Some(ac) = ac else {
            return Ok(None);
        };
        for found in ac.find_iter(content) {
            if !handle_match(found.start(), found.end())? {
                break;
            }
        }
    } else {
        for start in finder.find_iter(content) {
            if !handle_match(start, start + needle.len())? {
                break;
            }
        }
    }

    finish_rendered_match_output(output, match_count, path, options, show_path)
}

fn search_word_prefix_content(
    content: &[u8],
    ac: &AhoCorasick,
    boundary_start: bool,
    boundary_end: bool,
    options: &Options,
) -> Vec<MatchLine> {
    let mut matches = Vec::new();
    let mut last_line_start = usize::MAX;
    let mut line_scan_pos = 0usize;
    let mut line_no = 1usize;
    for found in ac.find_iter(content) {
        let start = found.start();
        if boundary_start && start > 0 && is_word_byte(content[start - 1]) {
            continue;
        }
        let mut end = found.end();
        while end < content.len() && is_word_byte(content[end]) {
            end += 1;
        }
        if boundary_end && end < content.len() && is_word_byte(content[end]) {
            continue;
        }
        let line_start = memrchr(b'\n', &content[..start]).map_or(0, |pos| pos + 1);
        if line_start == last_line_start {
            continue;
        }
        while line_scan_pos < line_start {
            if let Some(pos) = memchr(b'\n', &content[line_scan_pos..line_start]) {
                line_no += 1;
                line_scan_pos += pos + 1;
            } else {
                line_scan_pos = line_start;
            }
        }
        let line_end = memchr(b'\n', &content[start..])
            .map(|pos| start + pos)
            .unwrap_or(content.len());
        let mut line = &content[line_start..line_end];
        if line.ends_with(b"\r") {
            line = &line[..line.len() - 1];
        }
        matches.push(MatchLine {
            line_no,
            column: start.saturating_sub(line_start) + 1,
            line: line.to_vec(),
            matched: content[start..end].to_vec(),
        });
        last_line_start = line_start;
        if options.max_count.is_some_and(|max| matches.len() >= max) {
            break;
        }
    }
    matches
}

fn search_word_prefix_rendered(
    content: &[u8],
    path: &str,
    ac: &AhoCorasick,
    boundary_start: bool,
    boundary_end: bool,
    options: &Options,
    show_path: bool,
) -> Result<Option<RenderedFileResult>> {
    let mut output = Vec::new();
    let mut match_count = 0usize;
    let mut path_written = false;
    let mut last_line_start = usize::MAX;
    let mut cursor = LineCursor::new(content);

    for found in ac.find_iter(content) {
        let start = found.start();
        if boundary_start && start > 0 && is_word_byte(content[start - 1]) {
            continue;
        }
        let mut end = found.end();
        while end < content.len() && is_word_byte(content[end]) {
            end += 1;
        }
        if boundary_end && end < content.len() && is_word_byte(content[end]) {
            continue;
        }
        cursor.advance_to(start);
        if cursor.line_start == last_line_start {
            continue;
        }
        match_count += 1;
        if !options.count && !options.files_with_matches {
            let match_show_path =
                prepare_match_render(&mut output, path, options, show_path, &mut path_written)?;
            render_match_to(
                &mut output,
                path,
                cursor.line_no,
                start.saturating_sub(cursor.line_start) + 1,
                cursor.line(),
                &content[start..end],
                options,
                match_show_path,
            )?;
        } else if options.files_with_matches && !path_written {
            writeln!(output, "{path}")?;
            path_written = true;
            if files_with_matches_can_stop(options) {
                break;
            }
        }
        last_line_start = cursor.line_start;
        if options.max_count.is_some_and(|max| match_count >= max) {
            break;
        }
    }

    finish_rendered_match_output(output, match_count, path, options, show_path)
}

fn search_literal_set_rendered(
    content: &[u8],
    path: &str,
    ac: &AhoCorasick,
    options: &Options,
    show_path: bool,
) -> Result<Option<RenderedFileResult>> {
    let mut output = Vec::new();
    let mut match_count = 0usize;
    let mut path_written = false;
    let mut last_line_start = usize::MAX;
    let mut cursor = LineCursor::new(content);

    for found in ac.find_iter(content) {
        let start = found.start();
        cursor.advance_to(start);
        if cursor.line_start == last_line_start {
            continue;
        }
        match_count += 1;
        if !options.count && !options.files_with_matches {
            let match_show_path =
                prepare_match_render(&mut output, path, options, show_path, &mut path_written)?;
            render_match_to(
                &mut output,
                path,
                cursor.line_no,
                start.saturating_sub(cursor.line_start) + 1,
                cursor.line(),
                &content[start..found.end()],
                options,
                match_show_path,
            )?;
        } else if options.files_with_matches && !path_written {
            writeln!(output, "{path}")?;
            path_written = true;
            if files_with_matches_can_stop(options) {
                break;
            }
        }
        last_line_start = cursor.line_start;
        if options.max_count.is_some_and(|max| match_count >= max) {
            break;
        }
    }

    finish_rendered_match_output(output, match_count, path, options, show_path)
}

fn search_ordered_literals_rendered(
    content: &[u8],
    path: &str,
    literals: &[Vec<u8>],
    finder: &memmem::Finder<'_>,
    options: &Options,
    show_path: bool,
) -> Result<Option<RenderedFileResult>> {
    if literals.is_empty() {
        return Ok(None);
    }
    let mut output = Vec::new();
    let mut match_count = 0usize;
    let mut path_written = false;
    let mut last_line_start = usize::MAX;
    let mut cursor = LineCursor::new(content);

    for start in finder.find_iter(content) {
        cursor.advance_to(start);
        if cursor.line_start == last_line_start {
            continue;
        }
        let line = cursor.line();
        let first_start = start.saturating_sub(cursor.line_start);
        if first_start >= line.len() {
            continue;
        }
        let mut pos = first_start + literals[0].len();
        let mut last_end = pos;
        let mut ok = true;
        for literal in literals.iter().skip(1) {
            if let Some(found) = memmem::find(&line[pos..], literal) {
                let literal_start = pos + found;
                last_end = literal_start + literal.len();
                pos = last_end;
            } else {
                ok = false;
                break;
            }
        }
        if !ok {
            continue;
        }

        match_count += 1;
        if !options.count && !options.files_with_matches {
            let match_show_path =
                prepare_match_render(&mut output, path, options, show_path, &mut path_written)?;
            render_match_to(
                &mut output,
                path,
                cursor.line_no,
                first_start + 1,
                line,
                &line[first_start..last_end.min(line.len())],
                options,
                match_show_path,
            )?;
        } else if options.files_with_matches && !path_written {
            writeln!(output, "{path}")?;
            path_written = true;
            if files_with_matches_can_stop(options) {
                break;
            }
        }
        last_line_start = cursor.line_start;
        if options.max_count.is_some_and(|max| match_count >= max) {
            break;
        }
    }

    finish_rendered_match_output(output, match_count, path, options, show_path)
}

fn ordered_literal_line_spans(
    line: &[u8],
    literals: &[Vec<u8>],
    finder: Option<&memmem::Finder<'_>>,
    ignore_case: bool,
) -> Vec<(usize, usize)> {
    let Some(first) = literals.first() else {
        return Vec::new();
    };
    let lowered;
    let haystack = if ignore_case {
        lowered = lower_bytes(line);
        lowered.as_slice()
    } else {
        line
    };
    let mut spans = Vec::new();
    if !ignore_case && let Some(finder) = finder {
        for start in finder.find_iter(line) {
            if let Some(span) = ordered_literal_match_at(haystack, literals, start) {
                spans.push(span);
            }
        }
        return spans;
    }
    let first_finder = memmem::Finder::new(first);
    for start in first_finder.find_iter(haystack) {
        if let Some(span) = ordered_literal_match_at(haystack, literals, start) {
            spans.push(span);
        }
    }
    spans
}

fn ordered_literal_match_at(
    line: &[u8],
    literals: &[Vec<u8>],
    start: usize,
) -> Option<(usize, usize)> {
    let first = literals.first()?;
    if start + first.len() > line.len() || &line[start..start + first.len()] != first.as_slice() {
        return None;
    }
    let mut pos = start + first.len();
    let mut last_end = pos;
    for literal in literals.iter().skip(1) {
        let found = memmem::find(&line[pos..], literal)?;
        let literal_start = pos + found;
        last_end = literal_start + literal.len();
        pos = last_end;
    }
    Some((start, last_end))
}

fn search_ordered_wordspan_rendered(
    content: &[u8],
    path: &str,
    literals: &[Vec<u8>],
    finder: &memmem::Finder<'_>,
    options: &Options,
    show_path: bool,
) -> Result<Option<RenderedFileResult>> {
    if literals.is_empty() {
        return Ok(None);
    }
    let mut output = Vec::new();
    let mut match_count = 0usize;
    let mut path_written = false;
    let mut last_line_start = usize::MAX;
    let mut cursor = LineCursor::new(content);

    for start in finder.find_iter(content) {
        cursor.advance_to(start);
        if cursor.line_start == last_line_start {
            continue;
        }
        let line = cursor.line();
        let first_start = start.saturating_sub(cursor.line_start);
        if let Some((match_start, match_end)) =
            find_ordered_wordspan_match_at(line, first_start, literals)
        {
            match_count += 1;
            if !options.count && !options.files_with_matches {
                let match_show_path =
                    prepare_match_render(&mut output, path, options, show_path, &mut path_written)?;
                render_match_to(
                    &mut output,
                    path,
                    cursor.line_no,
                    match_start + 1,
                    line,
                    &line[match_start..match_end.min(line.len())],
                    options,
                    match_show_path,
                )?;
            } else if options.files_with_matches && !path_written {
                writeln!(output, "{path}")?;
                path_written = true;
                if files_with_matches_can_stop(options) {
                    break;
                }
            }
            last_line_start = cursor.line_start;
            if options.max_count.is_some_and(|max| match_count >= max) {
                break;
            }
        }
    }

    finish_rendered_match_output(output, match_count, path, options, show_path)
}

fn ordered_wordspan_line_spans(
    line: &[u8],
    literals: &[Vec<u8>],
    finder: Option<&memmem::Finder<'_>>,
    ignore_case: bool,
) -> Vec<(usize, usize)> {
    let Some(first) = literals.first() else {
        return Vec::new();
    };
    let lowered;
    let haystack = if ignore_case {
        lowered = lower_bytes(line);
        lowered.as_slice()
    } else {
        line
    };
    let mut spans = Vec::new();
    if !ignore_case && let Some(finder) = finder {
        for start in finder.find_iter(line) {
            if let Some(span) = find_ordered_wordspan_match_at(haystack, start, literals) {
                spans.push(span);
            }
        }
        return spans;
    }
    let first_finder = memmem::Finder::new(first);
    for start in first_finder.find_iter(haystack) {
        if let Some(span) = find_ordered_wordspan_match_at(haystack, start, literals) {
            spans.push(span);
        }
    }
    spans
}

fn find_ordered_wordspan_match(content: &[u8], literals: &[Vec<u8>]) -> Option<(usize, usize)> {
    let first = literals.first()?;
    let finder = memmem::Finder::new(first);
    for start in finder.find_iter(content) {
        if let Some(found) = find_ordered_wordspan_match_at(content, start, literals) {
            return Some(found);
        }
    }
    None
}

#[cold]
#[inline(never)]
fn ordered_literals_has_match(
    content: &[u8],
    literals: &[Vec<u8>],
    finder: &memmem::Finder<'_>,
) -> bool {
    if literals.is_empty() {
        return false;
    }
    for start in finder.find_iter(content) {
        let line_start = memrchr(b'\n', &content[..start]).map_or(0, |pos| pos + 1);
        let line_end = memchr(b'\n', &content[start..])
            .map(|pos| start + pos)
            .unwrap_or(content.len());
        let line = &content[line_start..line_end];
        let mut pos = start.saturating_sub(line_start) + literals[0].len();
        let mut ok = true;
        for literal in literals.iter().skip(1) {
            if let Some(found) = memmem::find(&line[pos..], literal) {
                pos += found + literal.len();
            } else {
                ok = false;
                break;
            }
        }
        if ok {
            return true;
        }
    }
    false
}

#[cold]
#[inline(never)]
fn ordered_wordspan_has_match(
    content: &[u8],
    literals: &[Vec<u8>],
    finder: &memmem::Finder<'_>,
) -> bool {
    if literals.is_empty() {
        return false;
    }
    for start in finder.find_iter(content) {
        let line_start = memrchr(b'\n', &content[..start]).map_or(0, |pos| pos + 1);
        let line_end = memchr(b'\n', &content[start..])
            .map(|pos| start + pos)
            .unwrap_or(content.len());
        let line = &content[line_start..line_end];
        let first_start = start.saturating_sub(line_start);
        if find_ordered_wordspan_match_at(line, first_start, literals).is_some() {
            return true;
        }
    }
    false
}

fn find_ordered_wordspan_match_at(
    line: &[u8],
    start: usize,
    literals: &[Vec<u8>],
) -> Option<(usize, usize)> {
    let first = literals.first()?;
    if start + first.len() > line.len() || &line[start..start + first.len()] != first.as_slice() {
        return None;
    }
    let mut pos = start + first.len();
    let mut last_end = pos;
    for literal in literals.iter().skip(1) {
        let span_end = advance_word(line, pos);
        let literal_start = pos + memmem::find(&line[pos..span_end], literal)?;
        last_end = literal_start + literal.len();
        pos = last_end;
    }
    Some((start, last_end))
}

fn search_qualified_call_content(
    content: &[u8],
    spec: &QualifiedCallSpec,
    finder: &memmem::Finder<'_>,
    options: &Options,
) -> Vec<MatchLine> {
    let mut matches = Vec::new();
    let mut last_line_start = usize::MAX;
    let mut line_scan_pos = 0usize;
    let mut line_no = 1usize;
    for found in finder.find_iter(content) {
        if found == 0 || found + 2 >= content.len() {
            continue;
        }
        let token_start = rewind_word(content, found);
        let Some(class_start) = qualified_call_match_start(content, token_start, found, spec)
        else {
            continue;
        };
        let method_start = found + 2;
        if method_start >= content.len() || !is_word_byte(content[method_start]) {
            continue;
        }
        let method_end = advance_word(content, method_start);
        if method_end >= content.len() || content[method_end] != b'(' {
            continue;
        }
        let line_start = memrchr(b'\n', &content[..class_start]).map_or(0, |pos| pos + 1);
        if line_start == last_line_start {
            continue;
        }
        while line_scan_pos < line_start {
            if let Some(pos) = memchr(b'\n', &content[line_scan_pos..line_start]) {
                line_no += 1;
                line_scan_pos += pos + 1;
            } else {
                line_scan_pos = line_start;
            }
        }
        let line_end = memchr(b'\n', &content[class_start..])
            .map(|pos| class_start + pos)
            .unwrap_or(content.len());
        let mut line = &content[line_start..line_end];
        if line.ends_with(b"\r") {
            line = &line[..line.len() - 1];
        }
        matches.push(MatchLine {
            line_no,
            column: class_start.saturating_sub(line_start) + 1,
            line: line.to_vec(),
            matched: content[class_start..=method_end].to_vec(),
        });
        last_line_start = line_start;
        if options.max_count.is_some_and(|max| matches.len() >= max) {
            break;
        }
    }
    matches
}

fn search_qualified_call_rendered(
    content: &[u8],
    path: &str,
    spec: &QualifiedCallSpec,
    finder: &memmem::Finder<'_>,
    options: &Options,
    show_path: bool,
) -> Result<Option<RenderedFileResult>> {
    let mut output = Vec::new();
    let mut match_count = 0usize;
    let mut path_written = false;
    let mut last_line_start = usize::MAX;
    let mut line_scan_pos = 0usize;
    let mut line_no = 1usize;
    for found in finder.find_iter(content) {
        if found == 0 || found + 2 >= content.len() {
            continue;
        }
        let token_start = rewind_word(content, found);
        let Some(class_start) = qualified_call_match_start(content, token_start, found, spec)
        else {
            continue;
        };
        let method_start = found + 2;
        if method_start >= content.len() || !is_word_byte(content[method_start]) {
            continue;
        }
        let method_end = advance_word(content, method_start);
        if method_end >= content.len() || content[method_end] != b'(' {
            continue;
        }
        let line_start = memrchr(b'\n', &content[..class_start]).map_or(0, |pos| pos + 1);
        if line_start == last_line_start {
            continue;
        }
        while line_scan_pos < line_start {
            if let Some(pos) = memchr(b'\n', &content[line_scan_pos..line_start]) {
                line_no += 1;
                line_scan_pos += pos + 1;
            } else {
                line_scan_pos = line_start;
            }
        }
        match_count += 1;
        if !options.count && !options.files_with_matches {
            let line_end = memchr(b'\n', &content[class_start..])
                .map(|pos| class_start + pos)
                .unwrap_or(content.len());
            let mut line = &content[line_start..line_end];
            if line.ends_with(b"\r") {
                line = &line[..line.len() - 1];
            }
            let match_show_path =
                prepare_match_render(&mut output, path, options, show_path, &mut path_written)?;
            render_match_to(
                &mut output,
                path,
                line_no,
                class_start.saturating_sub(line_start) + 1,
                line,
                &content[class_start..=method_end],
                options,
                match_show_path,
            )?;
        } else if options.files_with_matches && !path_written {
            writeln!(output, "{path}")?;
            path_written = true;
            if files_with_matches_can_stop(options) {
                break;
            }
        }
        last_line_start = line_start;
        if options.max_count.is_some_and(|max| match_count >= max) {
            break;
        }
    }

    if match_count == 0 {
        return Ok(None);
    }
    if options.count {
        if show_path {
            write!(output, "{path}:")?;
        }
        writeln!(output, "{match_count}")?;
    }
    Ok(Some(RenderedFileResult {
        path: path.to_string(),
        output,
        match_count,
    }))
}

#[cold]
#[inline(never)]
fn qualified_call_has_match(
    content: &[u8],
    spec: &QualifiedCallSpec,
    finder: &memmem::Finder<'_>,
) -> bool {
    for found in finder.find_iter(content) {
        if found == 0 || found + 2 >= content.len() {
            continue;
        }
        let token_start = rewind_word(content, found);
        if qualified_call_match_start(content, token_start, found, spec).is_none() {
            continue;
        }
        let method_start = found + 2;
        if method_start >= content.len() || !is_word_byte(content[method_start]) {
            continue;
        }
        let method_end = advance_word(content, method_start);
        if method_end < content.len() && content[method_end] == b'(' {
            return true;
        }
    }
    false
}

fn qualified_call_match_start(
    content: &[u8],
    token_start: usize,
    scope_start: usize,
    spec: &QualifiedCallSpec,
) -> Option<usize> {
    if token_start >= scope_start {
        return None;
    }
    let class = &content[token_start..scope_start];
    let Some(prefix) = &spec.class_prefix else {
        return Some(token_start);
    };
    if prefix.is_empty() || prefix.len() + spec.class_min_extra > class.len() {
        return None;
    }
    if prefix.len() == 1 {
        for (start, _) in class
            .iter()
            .enumerate()
            .filter(|(_, byte)| **byte == prefix[0])
        {
            if start + prefix.len() + spec.class_min_extra <= class.len() {
                return Some(token_start + start);
            }
        }
        return None;
    }
    let finder = memmem::Finder::new(prefix);
    for start in finder.find_iter(class) {
        if start + prefix.len() + spec.class_min_extra <= class.len() {
            return Some(token_start + start);
        }
    }
    None
}

fn rewind_word(content: &[u8], mut pos: usize) -> usize {
    while pos > 0 && is_word_byte(content[pos - 1]) {
        pos -= 1;
    }
    pos
}

fn advance_word(content: &[u8], mut pos: usize) -> usize {
    while pos < content.len() && is_word_byte(content[pos]) {
        pos += 1;
    }
    pos
}

fn is_word_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
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

fn ordered_wordspan_literals(options: &Options) -> Option<Vec<Vec<u8>>> {
    if options.fixed || options.whole_word || options.pattern.is_empty() {
        return None;
    }
    let separator = "[A-Za-z0-9_]*";
    if !options.pattern.contains(separator) {
        return None;
    }
    let mut literals = Vec::new();
    for part in options.pattern.split(separator) {
        if part.is_empty() || !is_ascii_word_literal(part) {
            return None;
        }
        literals.push(if options.ignore_case {
            lower_bytes(part.as_bytes())
        } else {
            part.as_bytes().to_vec()
        });
    }
    (literals.len() >= 2).then_some(literals)
}

fn exact_parenthesized_literal_alternatives(pattern: &str) -> Option<Vec<&[u8]>> {
    let body = pattern
        .strip_prefix("(?:")
        .and_then(|rest| rest.strip_suffix(')'))
        .or_else(|| {
            pattern
                .strip_prefix('(')
                .and_then(|rest| rest.strip_suffix(')'))
        })?;
    if body.is_empty() || !body.contains('|') {
        return None;
    }
    let mut out = Vec::new();
    for part in body.split('|') {
        if part.len() < 3 || !part.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_') {
            return None;
        }
        out.push(part.as_bytes());
    }
    Some(out)
}

fn query_trigram_alternatives(options: &Options) -> Vec<Vec<u32>> {
    if options.fixed || regex_meta_free(&options.pattern) {
        return vec![literal_trigrams(options.pattern.as_bytes())];
    }
    if let Some(literals) = simple_parenthesized_literal_alternatives(&options.pattern) {
        return literals
            .into_iter()
            .map(|literal| literal_trigrams(literal.as_bytes()))
            .filter(|grams| !grams.is_empty())
            .collect();
    }
    if let Some(alternatives) = double_colon_word_trigram_alternatives(&options.pattern) {
        return alternatives;
    }
    let literals = required_regex_literals(&options.pattern);
    let mut grams = Vec::new();
    for literal in literals {
        grams.extend(literal_trigrams(literal.as_bytes()));
    }
    grams.sort_unstable();
    grams.dedup();
    vec![grams]
}

fn simple_parenthesized_literal_alternatives(pattern: &str) -> Option<Vec<String>> {
    let bytes = pattern.as_bytes();
    let mut escaped = false;
    let mut in_class = false;
    let mut start = None;
    let mut end = None;
    for (idx, &byte) in bytes.iter().enumerate() {
        if escaped {
            escaped = false;
            continue;
        }
        if byte == b'\\' {
            escaped = true;
            continue;
        }
        if in_class {
            if byte == b']' {
                in_class = false;
            }
            continue;
        }
        match byte {
            b'[' => in_class = true,
            b'(' if start.is_none() => start = Some(idx + 1),
            b')' if start.is_some() => {
                end = Some(idx);
                break;
            }
            _ => {}
        }
    }
    let start = start?;
    let end = end?;
    if matches!(bytes.get(end + 1), Some(b'?' | b'*')) {
        return None;
    }
    let body = &pattern[start..end];
    if !body.contains('|') {
        return None;
    }
    let mut out = Vec::new();
    for part in body.split('|') {
        if part.len() < 3 || !part.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_') {
            return None;
        }
        out.push(part.to_string());
    }
    Some(out)
}

fn double_colon_word_trigram_alternatives(pattern: &str) -> Option<Vec<Vec<u32>>> {
    if !pattern.contains("[A-Za-z0-9_]+::") && !pattern.contains("[A-Za-z_][A-Za-z0-9_]*::") {
        return None;
    }
    Some(
        ascii_word_bytes()
            .into_iter()
            .map(|byte| vec![trigram(byte, b':', b':')])
            .collect(),
    )
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
    let mut result = postings[0].as_ref().to_vec();
    for posting in postings.into_iter().skip(1) {
        let posting = posting.as_ref();
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

fn candidate_files(index: &MappedIndex, alternatives: &[Vec<u32>]) -> Result<Vec<u32>> {
    if alternatives.is_empty() {
        return Ok((0..index.file_count as u32).collect());
    }
    if alternatives.len() == 1 {
        return intersect_postings(index, &alternatives[0]);
    }
    if alternatives.iter().any(|grams| grams.is_empty()) {
        return Ok((0..index.file_count as u32).collect());
    }
    let mut seen = vec![false; index.file_count];
    let mut result = Vec::new();
    for grams in alternatives {
        for id in intersect_postings(index, grams)? {
            let slot = id as usize;
            if !seen[slot] {
                seen[slot] = true;
                result.push(id);
            }
        }
    }
    result.sort_unstable();
    Ok(result)
}

fn candidate_chunks(index: &MappedIndex, options: &Options) -> Result<Option<Vec<u32>>> {
    if index.chunk_info.is_none() {
        return Ok(None);
    }
    if let Some(prefixes) = boundary_word_prefixes(options) {
        let alternatives: Vec<Vec<u32>> = prefixes
            .iter()
            .map(|prefix| literal_trigrams(prefix))
            .filter(|grams| !grams.is_empty())
            .collect();
        if let Some(chunks) = candidate_chunks_from_alternatives(index, &alternatives)? {
            return Ok(usable_chunk_candidates(index, chunks));
        }
    }
    let Some(alternatives) = safe_chunk_trigram_alternatives(options) else {
        return Ok(None);
    };
    if alternatives.is_empty() || alternatives.iter().any(|grams| grams.is_empty()) {
        return Ok(None);
    }
    let Some(chunks) = candidate_chunks_from_alternatives(index, &alternatives)? else {
        return Ok(None);
    };
    Ok(usable_chunk_candidates(index, chunks))
}

fn usable_chunk_candidates(index: &MappedIndex, chunks: Vec<u32>) -> Option<Vec<u32>> {
    if chunks.is_empty() {
        return Some(chunks);
    }
    let max_useful = index.file_count.max(1) / 2;
    (chunks.len() <= max_useful).then_some(chunks)
}

fn safe_chunk_trigram_alternatives(options: &Options) -> Option<Vec<Vec<u32>>> {
    if options.fixed || regex_meta_free(&options.pattern) {
        let grams = safe_chunk_literal_trigrams(&options.pattern)?;
        return Some(vec![grams]);
    }
    if let Some(literals) = simple_parenthesized_literal_alternatives(&options.pattern) {
        let alternatives: Vec<Vec<u32>> = literals
            .into_iter()
            .filter_map(|literal| safe_chunk_literal_trigrams(&literal))
            .collect();
        return (!alternatives.is_empty()).then_some(alternatives);
    }
    let literals = required_regex_literals(&options.pattern);
    if literals.len() == 1 {
        let grams = safe_chunk_literal_trigrams(&literals[0])?;
        return Some(vec![grams]);
    }
    None
}

fn safe_chunk_literal_trigrams(literal: &str) -> Option<Vec<u32>> {
    if literal.len() > CHUNK_OVERLAP || literal.len() < 3 {
        return None;
    }
    let grams = literal_trigrams(literal.as_bytes());
    (!grams.is_empty()).then_some(grams)
}

fn candidate_chunks_from_alternatives(
    index: &MappedIndex,
    alternatives: &[Vec<u32>],
) -> Result<Option<Vec<u32>>> {
    if alternatives.len() == 1 {
        return intersect_chunk_postings(index, &alternatives[0]);
    }
    let Some(info) = index.chunk_info else {
        return Ok(None);
    };
    let mut seen = vec![false; info.chunk_count];
    let mut result = Vec::new();
    for grams in alternatives {
        let Some(ids) = intersect_chunk_postings(index, grams)? else {
            return Ok(None);
        };
        for id in ids {
            let slot = id as usize;
            if !seen[slot] {
                seen[slot] = true;
                result.push(id);
            }
        }
    }
    result.sort_unstable();
    Ok(Some(result))
}

fn intersect_chunk_postings(index: &MappedIndex, grams: &[u32]) -> Result<Option<Vec<u32>>> {
    if grams.is_empty() {
        return Ok(None);
    }
    let mut postings = Vec::new();
    for &gram in grams {
        if let Some(posting) = index.chunk_posting(gram)? {
            postings.push(posting.data);
        }
    }
    if postings.is_empty() {
        return Ok(None);
    }
    postings.sort_by_key(|p| p.len());
    let mut result = postings[0].as_ref().to_vec();
    for posting in postings.into_iter().skip(1) {
        result = intersect_sorted_u32(&result, posting.as_ref());
        if result.is_empty() {
            break;
        }
    }
    Ok(Some(result))
}

fn qualified_call_candidate_files(
    index: &MappedIndex,
    spec: &QualifiedCallSpec,
) -> Result<Option<Vec<u32>>> {
    let Some(posting) = index.posting(SPECIAL_QUALIFIED_CALL)? else {
        return Ok(None);
    };
    let mut candidates = posting.data.as_ref().to_vec();
    if let Some(prefix) = &spec.class_prefix {
        let key_len = prefix.len().min(QUALIFIED_CLASS_FRAGMENT_MAX_LEN);
        if let Some(fragment) = index.posting(qualified_class_fragment_key(&prefix[..key_len]))? {
            candidates = intersect_sorted_u32(&candidates, fragment.data.as_ref());
        }
    }
    Ok(Some(candidates))
}

fn intersect_sorted_u32(left: &[u32], right: &[u32]) -> Vec<u32> {
    let mut out = Vec::with_capacity(left.len().min(right.len()));
    let mut i = 0usize;
    let mut j = 0usize;
    while i < left.len() && j < right.len() {
        match left[i].cmp(&right[j]) {
            Ordering::Less => i += 1,
            Ordering::Greater => j += 1,
            Ordering::Equal => {
                out.push(left[i]);
                i += 1;
                j += 1;
            }
        }
    }
    out
}

fn prefix_candidate_files(index: &MappedIndex, prefixes: &[Vec<u8>]) -> Result<Option<Vec<u32>>> {
    let mut seen = vec![false; index.file_count];
    let mut found_any_posting = false;
    for prefix in prefixes {
        if prefix.len() < PREFIX_MIN_LEN {
            return Ok(None);
        }
        let key_len = prefix.len().min(PREFIX_MAX_LEN);
        if let Some(posting) = index.posting(prefix_posting_key(&prefix[..key_len]))? {
            found_any_posting = true;
            for &id in posting.data.as_ref() {
                if let Some(slot) = seen.get_mut(id as usize) {
                    *slot = true;
                }
            }
        }
    }
    if !found_any_posting {
        return Ok(None);
    }
    Ok(Some(
        seen.into_iter()
            .enumerate()
            .filter_map(|(id, matched)| matched.then_some(id as u32))
            .collect(),
    ))
}

fn word_fragment_candidate_files(
    index: &MappedIndex,
    options: &Options,
) -> Result<Option<Vec<u32>>> {
    let Some(keys) = query_word_fragment_keys(options) else {
        return Ok(None);
    };
    let mut postings = Vec::new();
    for key in keys {
        if let Some(posting) = index.posting(key)? {
            postings.push(posting.data);
        }
    }
    if postings.is_empty() {
        return Ok(None);
    }
    postings.sort_by_key(|posting| posting.len());
    let mut result = postings[0].as_ref().to_vec();
    for posting in postings.into_iter().skip(1) {
        result = intersect_sorted_u32(&result, posting.as_ref());
        if result.is_empty() {
            break;
        }
    }
    Ok(Some(result))
}

fn query_word_fragment_keys(options: &Options) -> Option<Vec<u32>> {
    if options.ignore_case && !options.pattern.is_ascii() {
        return None;
    }
    if options.fixed || regex_meta_free(&options.pattern) {
        return word_fragment_keys_for_literal(&options.pattern);
    }
    if let Some(prefixes) = word_prefix_literals(&options.pattern) {
        let mut keys = Vec::new();
        for prefix in prefixes {
            let text = bytes_to_string(&prefix);
            keys.extend(word_fragment_keys_for_literal(&text)?);
        }
        keys.sort_unstable();
        keys.dedup();
        return (!keys.is_empty()).then_some(keys);
    }
    let literals = required_regex_literals(&options.pattern);
    if literals.len() == 1 {
        return word_fragment_keys_for_literal(&literals[0]);
    }
    None
}

fn word_fragment_keys_for_literal(literal: &str) -> Option<Vec<u32>> {
    if !is_ascii_word_literal(literal) || literal.len() < WORD_FRAGMENT_MIN_LEN {
        return None;
    }
    let bytes = literal.as_bytes();
    let len = bytes.len().min(WORD_FRAGMENT_MAX_LEN);
    let mut keys = Vec::new();
    for start in 0..=bytes.len() - len {
        keys.push(word_fragment_key(&bytes[start..start + len]));
    }
    keys.sort_unstable();
    keys.dedup();
    Some(keys)
}

fn word_prefix_literals(pattern: &str) -> Option<Vec<Vec<u8>>> {
    let mut pattern = pattern;
    if let Some(stripped) = pattern.strip_prefix(r"\b") {
        pattern = stripped;
    }
    if let Some(stripped) = pattern.strip_suffix(r"\b") {
        pattern = stripped;
    }
    let prefix_part = pattern.strip_suffix("[A-Za-z0-9_]*")?;
    if prefix_part.starts_with('(') && prefix_part.ends_with(')') {
        let body = &prefix_part[1..prefix_part.len() - 1];
        if body.is_empty() || !body.contains('|') {
            return None;
        }
        let mut out = Vec::new();
        for part in body.split('|') {
            if !is_ascii_word_literal(part) {
                return None;
            }
            out.push(part.as_bytes().to_vec());
        }
        return Some(out);
    }
    is_ascii_word_literal(prefix_part).then(|| vec![prefix_part.as_bytes().to_vec()])
}

fn ascii_word_bytes() -> Vec<u8> {
    let mut out = Vec::with_capacity(63);
    out.extend(b'a'..=b'z');
    out.extend(b'A'..=b'Z');
    out.extend(b'0'..=b'9');
    out.push(b'_');
    out
}

fn render_file_result(
    path: &str,
    matches: &[MatchLine],
    options: &Options,
    show_path: bool,
) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    if options.files_with_matches {
        writeln!(out, "{path}")?;
        return Ok(out);
    }
    if options.count {
        if show_path {
            write!(out, "{path}:")?;
        }
        writeln!(out, "{}", matches.len())?;
        return Ok(out);
    }
    let grouped = grouped_heading_output(options, show_path);
    if grouped {
        render_file_heading(&mut out, path, options)?;
    }
    for m in matches {
        render_match_to(
            &mut out,
            path,
            m.line_no,
            m.column,
            &m.line,
            &m.matched,
            options,
            show_path && !grouped,
        )?;
    }
    Ok(out)
}

fn render_file_result_with_content(
    path: &str,
    content: &[u8],
    matches: &[MatchLine],
    options: &Options,
    show_path: bool,
) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    if options.files_with_matches {
        writeln!(out, "{path}")?;
        return Ok(out);
    }
    if options.count {
        if show_path {
            write!(out, "{path}:")?;
        }
        writeln!(out, "{}", matches.len())?;
        return Ok(out);
    }
    if matches.is_empty() {
        return Ok(out);
    }

    let lines = file_lines(content);
    let grouped = grouped_heading_output(options, show_path);
    if grouped {
        render_file_heading(&mut out, path, options)?;
    }

    let ranges = context_ranges(matches, lines.len(), options);
    let mut match_idx = 0usize;
    for (range_idx, (start, end)) in ranges.into_iter().enumerate() {
        if range_idx != 0 {
            writeln!(out, "--")?;
        }
        for line_no in start..=end {
            while match_idx < matches.len() && matches[match_idx].line_no < line_no {
                match_idx += 1;
            }
            if matches
                .get(match_idx)
                .is_some_and(|matched| matched.line_no == line_no)
            {
                let matched = &matches[match_idx];
                render_match_to(
                    &mut out,
                    path,
                    matched.line_no,
                    matched.column,
                    &matched.line,
                    &matched.matched,
                    options,
                    show_path && !grouped,
                )?;
            } else if let Some(line) = lines.get(line_no.saturating_sub(1)) {
                render_context_line_to(
                    &mut out,
                    path,
                    line_no,
                    line,
                    options,
                    show_path && !grouped,
                )?;
            }
        }
    }
    Ok(out)
}

fn context_requested(options: &Options) -> bool {
    options.before_context != 0 || options.after_context != 0
}

fn context_ranges(
    matches: &[MatchLine],
    line_count: usize,
    options: &Options,
) -> Vec<(usize, usize)> {
    let mut ranges: Vec<(usize, usize)> = Vec::new();
    for m in matches {
        let start = m.line_no.saturating_sub(options.before_context).max(1);
        let end = (m.line_no + options.after_context).min(line_count);
        if let Some((_, prev_end)) = ranges.last_mut() {
            if start <= *prev_end + 1 {
                *prev_end = (*prev_end).max(end);
                continue;
            }
        }
        ranges.push((start, end));
    }
    ranges
}

fn file_lines(content: &[u8]) -> Vec<&[u8]> {
    let mut lines = Vec::new();
    for line in content.split(|byte| *byte == b'\n') {
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        lines.push(line);
    }
    if content.ends_with(b"\n") {
        lines.pop();
    }
    lines
}

fn grouped_heading_output(options: &Options, show_path: bool) -> bool {
    show_path
        && options
            .heading
            .unwrap_or_else(stdout_supports_search_decoration)
        && !options.json
        && !options.vimgrep
        && !options.count
        && !options.files_with_matches
}

fn prepare_match_render<W: Write>(
    out: &mut W,
    path: &str,
    options: &Options,
    show_path: bool,
    heading_written: &mut bool,
) -> Result<bool> {
    let grouped = grouped_heading_output(options, show_path);
    if grouped && !*heading_written {
        render_file_heading(out, path, options)?;
        *heading_written = true;
    }
    Ok(show_path && !grouped)
}

fn render_file_heading<W: Write>(out: &mut W, path: &str, options: &Options) -> Result<()> {
    write_styled(out, path.as_bytes(), options.color.enabled(), "35")?;
    writeln!(out)?;
    Ok(())
}

fn render_match_to<W: Write>(
    out: &mut W,
    path: &str,
    line_no: usize,
    column: usize,
    line: &[u8],
    matched: &[u8],
    options: &Options,
    show_path: bool,
) -> Result<()> {
    if options.json {
        writeln!(
            out,
            "{{\"type\":\"match\",\"data\":{{\"path\":{{\"text\":\"{}\"}},\"lines\":{{\"text\":\"{}\"}},\"line_number\":{},\"absolute_offset\":0,\"submatches\":[{{\"match\":{{\"text\":\"{}\"}},\"start\":{},\"end\":{}}}]}}}}",
            json_escape(path),
            json_escape_bytes(line),
            line_no,
            json_escape_bytes(matched),
            column - 1,
            column - 1 + matched.len()
        )?;
        return Ok(());
    }
    if options.vimgrep {
        write!(out, "{}:{}:{}:", path, line_no, column)?;
        out.write_all(line)?;
        writeln!(out)?;
        return Ok(());
    }
    let color = options.color.enabled();
    if show_path {
        write_styled(out, path.as_bytes(), color, "35")?;
        write!(out, ":")?;
    }
    if options.line_number {
        write_styled_display(out, line_no, color, "32")?;
        write!(out, ":")?;
    }
    if options.column {
        write_styled_display(out, column, color, "32")?;
        write!(out, ":")?;
    }
    let (display_line, display_match_start, display_match_len) =
        display_line_match(line, column.saturating_sub(1), matched.len(), options.trim);
    if options.only_matching && !matched.is_empty() {
        write_styled(out, matched, color, "1;31")?;
    } else {
        write_line_with_match(
            out,
            display_line,
            display_match_start,
            display_match_len,
            color,
        )?;
    }
    writeln!(out)?;
    Ok(())
}

fn render_context_line_to<W: Write>(
    out: &mut W,
    path: &str,
    line_no: usize,
    line: &[u8],
    options: &Options,
    show_path: bool,
) -> Result<()> {
    let color = options.color.enabled();
    if show_path {
        write_styled(out, path.as_bytes(), color, "35")?;
        write!(out, "-")?;
    }
    if options.line_number {
        write_styled_display(out, line_no, color, "32")?;
        write!(out, "-")?;
    }
    out.write_all(display_context_line(line, options.trim))?;
    writeln!(out)?;
    Ok(())
}

fn display_line_match(
    line: &[u8],
    match_start: usize,
    match_len: usize,
    trim: bool,
) -> (&[u8], usize, usize) {
    if !trim {
        return (line, match_start, match_len);
    }
    let trim_start = line
        .iter()
        .position(|byte| !matches!(*byte, b' ' | b'\t'))
        .unwrap_or(line.len());
    let display_line = &line[trim_start..];
    if match_start < trim_start {
        return (display_line, 0, 0);
    }
    let display_start = match_start - trim_start;
    let display_len = match_len.min(display_line.len().saturating_sub(display_start));
    (display_line, display_start, display_len)
}

fn display_context_line(line: &[u8], trim: bool) -> &[u8] {
    if !trim {
        return line;
    }
    let trim_start = line
        .iter()
        .position(|byte| !matches!(*byte, b' ' | b'\t'))
        .unwrap_or(line.len());
    &line[trim_start..]
}

fn write_line_with_match<W: Write>(
    out: &mut W,
    line: &[u8],
    start: usize,
    len: usize,
    color: bool,
) -> Result<()> {
    if !color || len == 0 || start >= line.len() {
        out.write_all(line)?;
        return Ok(());
    }
    let end = (start + len).min(line.len());
    out.write_all(&line[..start])?;
    write_styled(out, &line[start..end], true, "1;31")?;
    out.write_all(&line[end..])?;
    Ok(())
}

fn write_styled<W: Write>(out: &mut W, bytes: &[u8], color: bool, code: &str) -> Result<()> {
    if color {
        write!(out, "\x1b[{code}m")?;
        out.write_all(bytes)?;
        write!(out, "\x1b[0m")?;
    } else {
        out.write_all(bytes)?;
    }
    Ok(())
}

fn write_styled_display<W: Write, T: std::fmt::Display>(
    out: &mut W,
    value: T,
    color: bool,
    code: &str,
) -> Result<()> {
    if color {
        write!(out, "\x1b[{code}m{value}\x1b[0m")?;
    } else {
        write!(out, "{value}")?;
    }
    Ok(())
}

fn write_rendered_results<W: Write>(
    out: &mut W,
    results: &[RenderedFileResult],
    options: &Options,
) -> Result<()> {
    let separated = grouped_heading_output(options, should_show_path(options));
    for (idx, result) in results.iter().enumerate() {
        if separated && idx != 0 {
            writeln!(out)?;
        }
        out.write_all(&result.output)?;
    }
    Ok(())
}

fn build_path_matcher(options: &Options) -> Result<Option<(MatcherSet, MatcherSet)>> {
    let mut positives: Vec<String> = options
        .globs
        .iter()
        .filter(|g| !g.starts_with('!'))
        .cloned()
        .collect();
    let mut negatives: Vec<String> = options
        .globs
        .iter()
        .filter_map(|g| g.strip_prefix('!').map(ToOwned::to_owned))
        .collect();
    for ty in &options.type_includes {
        if let Some(globs) = search_type_globs(ty) {
            positives.extend(globs.iter().map(|glob| (*glob).to_string()));
        }
    }
    for ty in &options.type_excludes {
        if let Some(globs) = search_type_globs(ty) {
            negatives.extend(globs.iter().map(|glob| (*glob).to_string()));
        }
    }
    for ignore_file in &options.ignore_files {
        let path = resolve_search_path(options, ignore_file);
        let lines = load_pattern_lines(&path);
        negatives.extend(lines);
    }
    if positives.is_empty() && negatives.is_empty() {
        return Ok(None);
    }
    Ok(Some((
        MatcherSet::new_with_case(&positives, options.glob_case_insensitive)?,
        MatcherSet::new_with_case(
            &negatives,
            options.glob_case_insensitive || options.ignore_file_case_insensitive,
        )?,
    )))
}

impl PathFilter {
    fn new(options: &Options, root: &Path) -> Result<Self> {
        let matcher = build_path_matcher(options)?;
        let prefixes = search_path_prefixes(options, root)?;
        Ok(Self {
            prefixes,
            matcher,
            max_depth: options.max_depth,
        })
    }

    fn is_unrestricted(&self) -> bool {
        self.prefixes.is_empty() && self.matcher.is_none() && self.max_depth.is_none()
    }

    fn allows(&self, rel: &str, excluded_paths: &HashSet<String>) -> bool {
        if excluded_paths.contains(rel) {
            return false;
        }
        if !self.prefixes.is_empty()
            && !self.prefixes.iter().any(|prefix| {
                prefix.is_empty()
                    || rel == prefix
                    || rel
                        .strip_prefix(prefix)
                        .is_some_and(|tail| tail.starts_with('/'))
            })
        {
            return false;
        }
        if !self.allows_depth(rel) {
            return false;
        }
        if let Some((positive, negative)) = &self.matcher {
            if positive.set.is_some() && !positive.is_match(rel) {
                return false;
            }
            if negative.is_match(rel) {
                return false;
            }
        }
        true
    }

    fn allows_depth(&self, rel: &str) -> bool {
        let Some(max_depth) = self.max_depth else {
            return true;
        };
        if self.prefixes.is_empty() {
            return path_depth(rel) <= max_depth;
        }
        self.prefixes.iter().any(|prefix| {
            if prefix.is_empty() || rel == prefix {
                return true;
            }
            rel.strip_prefix(prefix)
                .and_then(|tail| tail.strip_prefix('/'))
                .is_some_and(|tail| path_depth(tail) <= max_depth)
        })
    }
}

fn has_path_restriction(options: &Options, root: &Path) -> bool {
    !options.globs.is_empty()
        || !options.ignore_files.is_empty()
        || !options.type_includes.is_empty()
        || !options.type_excludes.is_empty()
        || options.max_depth.is_some()
        || !options.paths.is_empty()
        || implicit_cwd_prefix(options, root).is_some()
}

fn path_depth(rel: &str) -> usize {
    rel.split('/')
        .filter(|part| !part.is_empty())
        .count()
        .saturating_sub(1)
}

fn search_path_prefixes(options: &Options, root: &Path) -> Result<Vec<String>> {
    if options.paths.is_empty() {
        return Ok(implicit_cwd_prefix(options, root).into_iter().collect());
    }
    let mut prefixes = Vec::with_capacity(options.paths.len());
    for raw in &options.paths {
        let abs = resolve_search_path(options, raw);
        let prefix = rel_path_for_filter(root, &abs).unwrap_or_else(|| raw.replace('\\', "/"));
        if prefix.is_empty() {
            return Ok(Vec::new());
        }
        prefixes.push(prefix);
    }
    Ok(prefixes)
}

fn implicit_cwd_prefix(options: &Options, root: &Path) -> Option<String> {
    if options.cwd.as_os_str().is_empty() || same_clean_root(&options.cwd, root) {
        return None;
    }
    let rel = rel_path_for_filter(root, &options.cwd)?;
    (!rel.is_empty()).then_some(rel)
}

fn rel_path_for_filter(root: &Path, path: &Path) -> Option<String> {
    rel_path(root, path).or_else(|| {
        let root = fs::canonicalize(root).ok()?;
        let path = fs::canonicalize(path).ok()?;
        rel_path(&root, &path)
    })
}

fn resolve_search_path(options: &Options, raw: &str) -> PathBuf {
    let path = PathBuf::from(raw);
    if path.is_absolute() || options.cwd.as_os_str().is_empty() {
        path
    } else {
        options.cwd.join(path)
    }
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

#[derive(Clone)]
struct DisplayPathMapper {
    mode: DisplayPathMode,
}

#[derive(Clone)]
enum DisplayPathMode {
    Identity,
    ImplicitCwd {
        prefix: String,
        prefix_slash: String,
    },
    Absolute {
        root: PathBuf,
        display_base: PathBuf,
        prefix: Option<String>,
        prefix_slash: Option<String>,
    },
    Relative {
        trimmed: String,
        prefix: Option<String>,
        prefix_slash: Option<String>,
    },
}

impl DisplayPathMapper {
    fn new(root: &Path, options: &Options) -> Self {
        if options.paths.is_empty() {
            return implicit_cwd_prefix(options, root)
                .map(|prefix| Self {
                    mode: DisplayPathMode::ImplicitCwd {
                        prefix_slash: format!("{prefix}/"),
                        prefix,
                    },
                })
                .unwrap_or_else(Self::identity);
        }
        if options.paths.len() != 1 {
            return Self::identity();
        }

        let raw = &options.paths[0];
        let resolved = resolve_search_path(options, raw);
        let path = Path::new(raw);
        if path.is_file() {
            return Self::identity();
        }
        if path.is_absolute() {
            let comparable_base = fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
            let prefix = rel_path(root, &comparable_base);
            return Self {
                mode: DisplayPathMode::Absolute {
                    root: root.to_path_buf(),
                    display_base: path.to_path_buf(),
                    prefix_slash: prefix.as_ref().map(|prefix| format!("{prefix}/")),
                    prefix,
                },
            };
        }

        let normalized = raw.replace('\\', "/");
        let trimmed = normalized.trim_end_matches('/').to_string();
        let comparable_base = fs::canonicalize(&resolved).unwrap_or(resolved);
        let prefix = rel_path(root, &comparable_base);
        Self {
            mode: DisplayPathMode::Relative {
                trimmed,
                prefix_slash: prefix.as_ref().map(|prefix| format!("{prefix}/")),
                prefix,
            },
        }
    }

    fn identity() -> Self {
        Self {
            mode: DisplayPathMode::Identity,
        }
    }

    fn display(&self, rel: &str) -> String {
        match &self.mode {
            DisplayPathMode::Identity => rel.to_string(),
            DisplayPathMode::ImplicitCwd {
                prefix,
                prefix_slash,
            } => {
                if rel == prefix {
                    ".".to_string()
                } else if let Some(rest) = rel.strip_prefix(prefix_slash) {
                    rest.to_string()
                } else {
                    rel.to_string()
                }
            }
            DisplayPathMode::Absolute {
                root,
                display_base,
                prefix,
                prefix_slash,
            } => {
                if prefix.as_deref() == Some("") {
                    return slash_path(display_base.join(rel));
                }
                if let Some(prefix) = prefix {
                    if rel == prefix {
                        return slash_path(display_base.clone());
                    }
                    if let Some(prefix_slash) = prefix_slash {
                        if let Some(rest) = rel.strip_prefix(prefix_slash) {
                            return slash_path(display_base.join(rest));
                        }
                    }
                }
                slash_path(root.join(rel))
            }
            DisplayPathMode::Relative {
                trimmed,
                prefix,
                prefix_slash,
            } => {
                if prefix.as_deref() == Some("") {
                    return if trimmed == "." {
                        format!("./{rel}")
                    } else {
                        format!("{trimmed}/{rel}")
                    };
                }
                if let Some(prefix) = prefix {
                    if rel == prefix {
                        return trimmed.clone();
                    }
                    if let Some(prefix_slash) = prefix_slash {
                        if let Some(rest) = rel.strip_prefix(prefix_slash) {
                            return if trimmed == "." {
                                format!("./{rest}")
                            } else {
                                format!("{trimmed}/{rest}")
                            };
                        }
                    }
                }
                if trimmed == "." {
                    return format!("./{rel}");
                }
                if let Some(prefix) = trimmed.strip_prefix("./") {
                    if rel == prefix {
                        return trimmed.clone();
                    }
                    if let Some(rest) = rel.strip_prefix(&format!("{prefix}/")) {
                        return format!("{trimmed}/{rest}");
                    }
                }
                rel.to_string()
            }
        }
    }
}

fn slash_path(path: PathBuf) -> String {
    clean_path_string(&path.to_string_lossy()).replace('\\', "/")
}

fn display_path(path: &Path) -> String {
    clean_path_string(&path.to_string_lossy())
}

fn clean_path_string(path: &str) -> String {
    #[cfg(windows)]
    {
        if let Some(rest) = path.strip_prefix(r"\\?\UNC\") {
            return format!(r"\\{rest}");
        }
        if let Some(rest) = path.strip_prefix(r"\\?\") {
            return rest.to_string();
        }
    }
    path.to_string()
}

fn file_trigrams(bytes: &[u8], scratch: &mut TrigramScratch) -> Vec<u32> {
    if bytes.len() < 3 {
        return Vec::new();
    }
    let mut grams = Vec::with_capacity((bytes.len() - 2).min(4096));
    let mut a = bytes[0].to_ascii_lowercase() as u32;
    let mut b = bytes[1].to_ascii_lowercase() as u32;
    for &byte in &bytes[2..] {
        let c = byte.to_ascii_lowercase() as u32;
        let gram = (a << 16) | (b << 8) | c;
        a = b;
        b = c;
        let idx = gram as usize;
        let word = idx / TRIGRAM_WORD_BITS;
        let mask = 1u64 << (idx % TRIGRAM_WORD_BITS);
        let current = scratch.bits[word];
        if current & mask == 0 {
            if current == 0 {
                scratch.touched_words.push(word);
            }
            scratch.bits[word] = current | mask;
            grams.push(gram);
        }
    }
    for word in scratch.touched_words.drain(..) {
        scratch.bits[word] = 0;
    }
    grams
}

fn index_grams(bytes: &[u8], scratch: &mut TrigramScratch) -> Vec<u32> {
    let (mut grams, mut extras, _) = scan_index_keys(bytes, scratch, false);
    add_qualified_call_postings(bytes, &mut extras);
    extras.sort_unstable();
    extras.dedup();
    grams.extend(extras);
    grams
}

fn index_grams_and_word_fragments_into(
    bytes: &[u8],
    scratch: &mut IndexKeyScratch,
) -> (usize, usize, usize) {
    scan_index_keys_into(bytes, scratch, true);
    let gram_len = scratch.grams.len();
    add_qualified_call_postings(bytes, &mut scratch.extras);
    scratch.extras.sort_unstable();
    scratch.extras.dedup();
    let extras_len = scratch.extras.len();
    scratch.grams.extend_from_slice(&scratch.extras);
    scratch.fragments.sort_unstable();
    scratch.fragments.dedup();
    (gram_len, extras_len, scratch.fragments.len())
}

fn index_grams_and_word_fragments_into_profiled(
    bytes: &[u8],
    scratch: &mut IndexKeyScratch,
    scan_ns: &AtomicU64,
    qualified_ns: &AtomicU64,
    sort_extras_ns: &AtomicU64,
    sort_fragments_ns: &AtomicU64,
) -> (usize, usize, usize) {
    let scan_timer = Instant::now();
    scan_index_keys_into(bytes, scratch, true);
    scan_ns.fetch_add(elapsed_ns(scan_timer), AtomicOrdering::Relaxed);
    let gram_len = scratch.grams.len();

    let qualified_timer = Instant::now();
    add_qualified_call_postings(bytes, &mut scratch.extras);
    qualified_ns.fetch_add(elapsed_ns(qualified_timer), AtomicOrdering::Relaxed);

    let sort_extras_timer = Instant::now();
    scratch.extras.sort_unstable();
    scratch.extras.dedup();
    sort_extras_ns.fetch_add(elapsed_ns(sort_extras_timer), AtomicOrdering::Relaxed);
    let extras_len = scratch.extras.len();
    scratch.grams.extend_from_slice(&scratch.extras);

    let sort_fragments_timer = Instant::now();
    scratch.fragments.sort_unstable();
    scratch.fragments.dedup();
    sort_fragments_ns.fetch_add(elapsed_ns(sort_fragments_timer), AtomicOrdering::Relaxed);
    (gram_len, extras_len, scratch.fragments.len())
}

fn scan_index_keys(
    bytes: &[u8],
    scratch: &mut TrigramScratch,
    collect_fragments: bool,
) -> (Vec<u32>, Vec<u32>, Vec<u32>) {
    let mut grams = Vec::with_capacity(bytes.len().saturating_sub(2).min(4096));
    let mut extras = Vec::new();
    let mut fragments = Vec::new();
    let mut word_start = None;

    let mut prev2 = 0u32;
    let mut prev1 = 0u32;
    for (idx, &byte) in bytes.iter().enumerate() {
        let lower = byte.to_ascii_lowercase();
        if idx == 0 {
            prev2 = lower as u32;
        } else if idx == 1 {
            prev1 = lower as u32;
        } else {
            let gram = (prev2 << 16) | (prev1 << 8) | lower as u32;
            prev2 = prev1;
            prev1 = lower as u32;
            push_unique_trigram(gram, scratch, &mut grams);
        }

        if is_word_byte(byte) {
            if word_start.is_none() {
                word_start = Some(idx);
            }
        } else if let Some(start) = word_start.take() {
            let word = &bytes[start..idx];
            add_word_prefix_postings(word, &mut extras);
            if collect_fragments {
                add_word_fragment_keys(word, &mut fragments);
            }
        }
    }
    if let Some(start) = word_start {
        let word = &bytes[start..];
        add_word_prefix_postings(word, &mut extras);
        if collect_fragments {
            add_word_fragment_keys(word, &mut fragments);
        }
    }
    for word in scratch.touched_words.drain(..) {
        scratch.bits[word] = 0;
    }
    (grams, extras, fragments)
}

fn scan_index_keys_into(bytes: &[u8], scratch: &mut IndexKeyScratch, collect_fragments: bool) {
    scratch.grams.clear();
    scratch.extras.clear();
    scratch.fragments.clear();
    let mut word_start = None;

    let mut prev2 = 0u32;
    let mut prev1 = 0u32;
    for (idx, &byte) in bytes.iter().enumerate() {
        let lower = byte.to_ascii_lowercase();
        if idx == 0 {
            prev2 = lower as u32;
        } else if idx == 1 {
            prev1 = lower as u32;
        } else {
            let gram = (prev2 << 16) | (prev1 << 8) | lower as u32;
            prev2 = prev1;
            prev1 = lower as u32;
            push_unique_trigram(gram, &mut scratch.trigram, &mut scratch.grams);
        }

        if is_word_byte(byte) {
            if word_start.is_none() {
                word_start = Some(idx);
            }
        } else if let Some(start) = word_start.take() {
            let word = &bytes[start..idx];
            add_word_prefix_postings(word, &mut scratch.extras);
            if collect_fragments {
                add_word_fragment_keys(word, &mut scratch.fragments);
            }
        }
    }
    if let Some(start) = word_start {
        let word = &bytes[start..];
        add_word_prefix_postings(word, &mut scratch.extras);
        if collect_fragments {
            add_word_fragment_keys(word, &mut scratch.fragments);
        }
    }
    for word in scratch.trigram.touched_words.drain(..) {
        scratch.trigram.bits[word] = 0;
    }
}

fn push_unique_trigram(gram: u32, scratch: &mut TrigramScratch, grams: &mut Vec<u32>) {
    let idx = gram as usize;
    let word = idx / TRIGRAM_WORD_BITS;
    let mask = 1u64 << (idx % TRIGRAM_WORD_BITS);
    let current = scratch.bits[word];
    if current & mask == 0 {
        if current == 0 {
            scratch.touched_words.push(word);
        }
        scratch.bits[word] = current | mask;
        grams.push(gram);
    }
}

fn add_word_fragment_keys(word: &[u8], keys: &mut Vec<u32>) {
    if word.len() < WORD_FRAGMENT_MIN_LEN {
        return;
    }
    if WORD_FRAGMENT_MIN_LEN == 6 && WORD_FRAGMENT_MAX_LEN == 6 {
        for start in 0..=word.len() - 6 {
            keys.push(word_fragment_key6(&word[start..start + 6]));
        }
    } else {
        let max_len = word.len().min(WORD_FRAGMENT_MAX_LEN);
        for len in WORD_FRAGMENT_MIN_LEN..=max_len {
            for start in 0..=word.len() - len {
                keys.push(word_fragment_key(&word[start..start + len]));
            }
        }
    }
}

fn selected_word_fragments<'a>(fragments: impl Iterator<Item = &'a [u32]>) -> HashSet<u32> {
    let mut counts: HashMap<u32, u32> = HashMap::default();
    for file_fragments in fragments {
        for &fragment in file_fragments {
            let count = counts.entry(fragment).or_insert(0);
            if *count <= WORD_FRAGMENT_MAX_FILES {
                *count += 1;
            }
        }
    }
    counts
        .into_iter()
        .filter_map(|(fragment, count)| {
            (count >= WORD_FRAGMENT_MIN_FILES && count <= WORD_FRAGMENT_MAX_FILES)
                .then_some(fragment)
        })
        .collect()
}

fn add_word_prefix_postings(word: &[u8], grams: &mut Vec<u32>) {
    if word.len() < PREFIX_MIN_LEN {
        return;
    }
    let max = word.len().min(PREFIX_MAX_LEN);
    let mut hash = FNV_OFFSET;
    for (idx, &byte) in word.iter().take(max).enumerate() {
        hash = fnv1a_step(hash, byte);
        if idx + 1 >= PREFIX_MIN_LEN {
            grams.push(PREFIX_POSTING_TAG | (hash & 0x3fff_ffff));
        }
    }
}

fn word_fragment_key(fragment: &[u8]) -> u32 {
    let mut hash = FNV_OFFSET;
    for &byte in fragment {
        hash = fnv1a_step(hash, byte);
    }
    WORD_FRAGMENT_TAG | (hash & 0x1fff_ffff)
}

#[inline]
fn word_fragment_key6(fragment: &[u8]) -> u32 {
    debug_assert!(fragment.len() == 6);
    let mut hash = FNV_OFFSET;
    hash = fnv1a_step(hash, fragment[0]);
    hash = fnv1a_step(hash, fragment[1]);
    hash = fnv1a_step(hash, fragment[2]);
    hash = fnv1a_step(hash, fragment[3]);
    hash = fnv1a_step(hash, fragment[4]);
    hash = fnv1a_step(hash, fragment[5]);
    WORD_FRAGMENT_TAG | (hash & 0x1fff_ffff)
}

fn prefix_posting_key(prefix: &[u8]) -> u32 {
    let mut hash = FNV_OFFSET;
    for &byte in prefix {
        hash = fnv1a_step(hash, byte);
    }
    PREFIX_POSTING_TAG | (hash & 0x3fff_ffff)
}

fn add_qualified_call_postings(content: &[u8], grams: &mut Vec<u32>) {
    let finder = memmem::Finder::new("::");
    for found in finder.find_iter(content) {
        if found == 0 || found + 2 >= content.len() {
            continue;
        }
        let token_start = rewind_word(content, found);
        if token_start >= found {
            continue;
        }
        let method_start = found + 2;
        if method_start >= content.len() || !is_word_byte(content[method_start]) {
            continue;
        }
        let method_end = advance_word(content, method_start);
        if method_end < content.len() && content[method_end] == b'(' {
            grams.push(SPECIAL_QUALIFIED_CALL);
            add_qualified_class_fragments(&content[token_start..found], grams);
        }
    }
}

fn add_qualified_class_fragments(class: &[u8], grams: &mut Vec<u32>) {
    for start in 0..class.len() {
        let max_end = (start + QUALIFIED_CLASS_FRAGMENT_MAX_LEN).min(class.len());
        let mut hash = FNV_OFFSET;
        for &byte in &class[start..max_end] {
            hash = fnv1a_step(hash, byte);
            grams.push(QUALIFIED_CLASS_FRAGMENT_TAG | (hash & 0x3fff_ffff));
        }
    }
}

fn qualified_class_fragment_key(fragment: &[u8]) -> u32 {
    let mut hash = FNV_OFFSET;
    for &byte in fragment {
        hash = fnv1a_step(hash, byte);
    }
    QUALIFIED_CLASS_FRAGMENT_TAG | (hash & 0x3fff_ffff)
}

const FNV_OFFSET: u32 = 0x811c_9dc5;
const FNV_PRIME: u32 = 0x0100_0193;

#[inline]
fn fnv1a_step(hash: u32, byte: u8) -> u32 {
    (hash ^ byte.to_ascii_lowercase() as u32).wrapping_mul(FNV_PRIME)
}

fn trigram(a: u8, b: u8, c: u8) -> u32 {
    ((a.to_ascii_lowercase() as u32) << 16)
        | ((b.to_ascii_lowercase() as u32) << 8)
        | (c.to_ascii_lowercase() as u32)
}

fn literal_trigrams(bytes: &[u8]) -> Vec<u32> {
    let mut scratch = TrigramScratch::new();
    file_trigrams(bytes, &mut scratch)
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

fn content_word_boundary(content: &[u8], start: usize, end: usize) -> bool {
    let before = start == 0 || !is_word(content[start - 1]);
    let after = end >= content.len() || !is_word(content[end]);
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

fn search_daemon_record_path(root: &Path) -> PathBuf {
    root.join(INDEX_DIR).join(SEARCH_DAEMON_FILE)
}

#[cfg(unix)]
fn search_daemon_socket_path(root: &Path) -> PathBuf {
    root.join(INDEX_DIR).join("search-daemon.sock")
}

fn project_registry_dir() -> PathBuf {
    home_dir().join(".indexsearch").join(PROJECTS_DIR)
}

fn home_dir() -> PathBuf {
    env::var_os("HOME")
        .or_else(|| env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

fn project_record_path(id: &str) -> PathBuf {
    project_registry_dir().join(format!("{id}.project"))
}

fn default_install_dir() -> PathBuf {
    home_dir().join(".local").join("bin")
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
    fs::copy(src, &tmp).with_context(|| {
        format!(
            "failed to copy {} to {}",
            display_path(src),
            display_path(&tmp)
        )
    })?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&tmp, fs::Permissions::from_mode(0o755))?;
    }
    if dst.exists() {
        let _ = fs::remove_file(dst);
    }
    fs::rename(&tmp, dst).with_context(|| format!("failed to replace {}", display_path(dst)))?;
    Ok(())
}

#[cfg(unix)]
fn install_alias(exe_path: &Path, alias_path: &Path) -> Result<()> {
    if alias_path.exists() {
        let _ = fs::remove_file(alias_path);
    }
    std::os::unix::fs::symlink(
        exe_path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("indexsearch"),
        alias_path,
    )?;
    Ok(())
}

fn install_search_alias(frontend_path: &Path, alias_path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        install_alias(frontend_path, alias_path)
    }
    #[cfg(windows)]
    {
        install_executable(frontend_path, alias_path)
    }
}

#[cfg(windows)]
fn warn_legacy_cmd_shims(install_dir: &Path, target_legacy_alias: &Path) {
    let install_dir = fs::canonicalize(install_dir).unwrap_or_else(|_| install_dir.to_path_buf());
    let target_legacy_alias =
        fs::canonicalize(target_legacy_alias).unwrap_or_else(|_| target_legacy_alias.to_path_buf());
    let Ok(path) = env::var("PATH") else {
        return;
    };
    for dir in env::split_paths(&path) {
        let Ok(dir) = fs::canonicalize(&dir) else {
            continue;
        };
        let legacy = dir.join("is.cmd");
        if !legacy.exists() {
            continue;
        }
        let legacy = fs::canonicalize(&legacy).unwrap_or(legacy);
        if dir == install_dir || legacy == target_legacy_alias {
            continue;
        }
        eprintln!(
            "indexsearch: warning: found legacy is.cmd on PATH: {}",
            display_path(&legacy)
        );
        eprintln!(
            "indexsearch: warning: remove it or call is.exe/indexsearch.exe; cmd shims re-parse PowerShell patterns containing | or >"
        );
    }
}

fn path_contains(dir: &Path) -> bool {
    let Ok(path) = env::var("PATH") else {
        return false;
    };
    env::split_paths(&path).any(|p| p == dir)
}

fn project_log_path(root: &Path) -> PathBuf {
    root.join(INDEX_DIR).join(PROJECT_LOG_FILE)
}

fn project_id(root: &Path) -> String {
    let canonical = fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    format!("{:016x}", fnv1a(canonical.to_string_lossy().as_bytes()))
}

fn search_daemon_token(root: &Path) -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!(
        "{:016x}{:08x}{:016x}",
        fnv1a(root.to_string_lossy().as_bytes()),
        std::process::id(),
        (nanos as u64) ^ ((nanos >> 64) as u64)
    )
}

fn write_search_daemon_record(record: &SearchDaemonRecord) -> Result<()> {
    fs::create_dir_all(record.root.join(INDEX_DIR))?;
    let mut text = format!(
        "version=1\nservice_name={}\nprotocol={}\ncapabilities={}\npid={}\nport={}\ntoken={}\nroot={}\nexe_path={}\nexe_size={}\nexe_mtime={}\nindex_size={}\nindex_mtime={}\n",
        record.service_name,
        record.protocol,
        record.capabilities_text(),
        record.pid,
        record.port,
        record.token,
        record.root.display(),
        record.exe_path.display(),
        record.exe_size,
        record.exe_mtime,
        record.index_size,
        record.index_mtime
    );
    if let Some(socket_path) = record.socket_path.as_ref() {
        text.push_str(&format!("socket_path={}\n", socket_path.display()));
    }
    fs::write(search_daemon_record_path(&record.root), text)?;
    Ok(())
}

fn refresh_search_daemon_record(record: &SearchDaemonRecord) -> Result<()> {
    let mut updated = record.clone();
    let index_meta = fs::metadata(index_path(&updated.root))?;
    updated.index_size = index_meta.len();
    updated.index_mtime = mtime_ns(&index_meta);
    write_search_daemon_record(&updated)
}

fn read_valid_search_daemon_record(
    root: &Path,
    mut profile: Option<&mut SearchProfile>,
) -> Result<Option<SearchDaemonRecord>> {
    let path = search_daemon_record_path(root);
    let read_timer = Instant::now();
    let Ok(record) = read_search_daemon_record(&path) else {
        if let Some(profile) = profile.as_deref_mut() {
            profile.record("client_read_daemon_record", read_timer.elapsed());
        }
        let _ = fs::remove_file(path);
        return Ok(None);
    };
    if let Some(profile) = profile.as_deref_mut() {
        profile.record("client_read_daemon_record", read_timer.elapsed());
    }
    let fingerprint_timer = Instant::now();
    if !search_daemon_fingerprint_matches(&record) {
        if let Some(profile) = profile.as_deref_mut() {
            profile.record("client_daemon_fingerprint", fingerprint_timer.elapsed());
        }
        stop_process(record.pid);
        let _ = fs::remove_file(path);
        return Ok(None);
    }
    if let Some(profile) = profile.as_deref_mut() {
        profile.record("client_daemon_fingerprint", fingerprint_timer.elapsed());
    }
    Ok(Some(record))
}

fn search_daemon_fingerprint_matches(record: &SearchDaemonRecord) -> bool {
    if record.protocol != SEARCH_DAEMON_PROTOCOL {
        return false;
    }
    if !process_alive(record.pid) {
        return false;
    }
    let Ok(record_exe_meta) = fs::metadata(&record.exe_path) else {
        return false;
    };
    if record_exe_meta.len() != record.exe_size || mtime_ns(&record_exe_meta) != record.exe_mtime {
        return false;
    }
    record.service_name != SEARCH_DAEMON_SERVICE_NAME
        || search_daemon_client_exe_candidates()
            .into_iter()
            .any(|candidate| same_clean_root(&candidate, &record.exe_path))
}

fn search_daemon_client_exe_candidates() -> Vec<PathBuf> {
    let mut candidates = Vec::new();
    if let Ok(exe) = env::current_exe() {
        if let Some(dir) = exe.parent() {
            candidates.push(dir.join(executable_name(&format!(
                "is-daemon-{}",
                env!("CARGO_PKG_VERSION")
            ))));
            candidates.push(dir.join(executable_name("is-daemon")));
            candidates.push(dir.join(executable_name("istool")));
        }
        candidates.push(exe);
    }
    candidates
}

fn read_search_daemon_record(path: &Path) -> Result<SearchDaemonRecord> {
    let text = fs::read_to_string(path)?;
    let mut service_name = SEARCH_DAEMON_SERVICE_NAME.to_string();
    let mut protocol = SEARCH_DAEMON_PROTOCOL;
    let mut capabilities = None;
    let mut pid = 0;
    let mut port = 0;
    let mut socket_path = None;
    let mut token = String::new();
    let mut root = PathBuf::new();
    let mut exe_path = PathBuf::new();
    let mut exe_size = 0;
    let mut exe_mtime = 0;
    let mut index_size = 0;
    let mut index_mtime = 0;
    for line in text.lines() {
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        match key {
            "service_name" => service_name = value.to_string(),
            "protocol" => protocol = value.parse().unwrap_or(0),
            "capabilities" => capabilities = Some(parse_search_daemon_capabilities(value)),
            "pid" => pid = value.parse().unwrap_or(0),
            "port" => port = value.parse().unwrap_or(0),
            "socket_path" => socket_path = Some(PathBuf::from(value)),
            "token" => token = value.to_string(),
            "root" => root = PathBuf::from(value),
            "exe_path" => exe_path = PathBuf::from(value),
            "exe_size" => exe_size = value.parse().unwrap_or(0),
            "exe_mtime" => exe_mtime = value.parse().unwrap_or(0),
            "index_size" => index_size = value.parse().unwrap_or(0),
            "index_mtime" => index_mtime = value.parse().unwrap_or(0),
            _ => {}
        }
    }
    let capabilities = capabilities.unwrap_or_else(SearchDaemonRecord::legacy_capabilities);
    if pid == 0
        || (port == 0 && socket_path.is_none())
        || token.is_empty()
        || root.as_os_str().is_empty()
        || service_name.is_empty()
        || protocol == 0
        || capabilities.is_empty()
    {
        bail!("invalid search daemon record {}", display_path(path));
    }
    Ok(SearchDaemonRecord {
        service_name,
        protocol,
        capabilities,
        pid,
        port,
        socket_path,
        token,
        root,
        exe_path,
        exe_size,
        exe_mtime,
        index_size,
        index_mtime,
    })
}

fn parse_search_daemon_capabilities(value: &str) -> Vec<String> {
    let mut capabilities = Vec::new();
    for capability in value.split(',') {
        let capability = capability.trim();
        if capability.is_empty() {
            continue;
        }
        if !capabilities.iter().any(|item| item == capability) {
            capabilities.push(capability.to_string());
        }
    }
    capabilities
}

fn path_is_ancestor(parent: &Path, child: &Path) -> bool {
    child == parent || child.starts_with(parent)
}

fn append_project_log(root: &Path, message: &str) -> Result<()> {
    fs::create_dir_all(root.join(INDEX_DIR))?;
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(project_log_path(root))?;
    writeln!(file, "{} {}", log_timestamp(), message)?;
    Ok(())
}

fn clean_log_text(value: &str, max_len: usize) -> String {
    let mut out = String::new();
    for ch in value.chars() {
        let clean = match ch {
            '\r' | '\n' | '\t' => ' ',
            _ if ch.is_control() => ' ',
            _ => ch,
        };
        out.push(clean);
        if out.len() >= max_len {
            out.truncate(max_len);
            out.push_str("...");
            break;
        }
    }
    out
}

fn log_quote(value: &str, max_len: usize) -> String {
    let value = clean_log_text(value, max_len);
    format!("{value:?}")
}

fn format_search_log_args(args: &[String]) -> String {
    let mut out = String::new();
    for (idx, arg) in args.iter().take(32).enumerate() {
        if idx != 0 {
            out.push(' ');
        }
        out.push_str(&log_quote(arg, 160));
    }
    if args.len() > 32 {
        out.push_str(" ...");
    }
    out
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

fn write_project_record(record: &ProjectRecord) -> Result<()> {
    fs::create_dir_all(project_registry_dir())?;
    let text = format!(
        "id={}\npid={}\nroot={}\n",
        record.id,
        record.pid,
        record.root.display()
    );
    fs::write(project_record_path(&record.id), text)?;
    Ok(())
}

fn read_project_record(path: &Path) -> Result<ProjectRecord> {
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
        bail!("invalid project record {}", display_path(path));
    }
    Ok(ProjectRecord { id, root, pid })
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
        use windows_sys::Win32::Foundation::{CloseHandle, WAIT_TIMEOUT};
        use windows_sys::Win32::System::Threading::{
            OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, WaitForSingleObject,
        };
        const SYNCHRONIZE: u32 = 0x0010_0000;
        unsafe {
            let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION | SYNCHRONIZE, 0, pid);
            if handle.is_null() {
                return false;
            }
            let alive = WaitForSingleObject(handle, 0) == WAIT_TIMEOUT;
            CloseHandle(handle);
            alive
        }
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
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

#[cfg(windows)]
fn spawn_detached_no_inherit(exe: &Path, args: &[std::ffi::OsString]) -> std::io::Result<u32> {
    use std::mem;
    use std::os::windows::ffi::OsStrExt;
    use std::ptr;
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::{
        CREATE_NEW_PROCESS_GROUP, CREATE_NO_WINDOW, CreateProcessW, DETACHED_PROCESS,
        PROCESS_INFORMATION, STARTUPINFOW,
    };

    let mut command_line = Vec::new();
    append_windows_arg(&mut command_line, exe.as_os_str());
    for arg in args {
        command_line.push(b' ' as u16);
        append_windows_arg(&mut command_line, arg.as_os_str());
    }
    command_line.push(0);

    let mut exe_wide: Vec<u16> = exe.as_os_str().encode_wide().chain(Some(0)).collect();
    let mut startup: STARTUPINFOW = unsafe { mem::zeroed() };
    startup.cb = mem::size_of::<STARTUPINFOW>() as u32;
    let mut process_info: PROCESS_INFORMATION = unsafe { mem::zeroed() };
    let ok = unsafe {
        CreateProcessW(
            exe_wide.as_mut_ptr(),
            command_line.as_mut_ptr(),
            ptr::null(),
            ptr::null(),
            0,
            DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW,
            ptr::null(),
            ptr::null(),
            &startup,
            &mut process_info,
        )
    };
    if ok == 0 {
        return Err(std::io::Error::last_os_error());
    }
    let pid = process_info.dwProcessId;
    unsafe {
        CloseHandle(process_info.hThread);
        CloseHandle(process_info.hProcess);
    }
    Ok(pid)
}

#[cfg(windows)]
fn append_windows_arg(out: &mut Vec<u16>, arg: &std::ffi::OsStr) {
    use std::os::windows::ffi::OsStrExt;
    let units: Vec<u16> = arg.encode_wide().collect();
    let needs_quotes = units.is_empty()
        || units
            .iter()
            .any(|&ch| ch == b' ' as u16 || ch == b'\t' as u16 || ch == b'"' as u16);
    if !needs_quotes {
        out.extend_from_slice(&units);
        return;
    }
    out.push(b'"' as u16);
    let mut backslashes = 0usize;
    for ch in units {
        if ch == b'\\' as u16 {
            backslashes += 1;
        } else if ch == b'"' as u16 {
            out.extend(std::iter::repeat_n(b'\\' as u16, backslashes * 2 + 1));
            out.push(ch);
            backslashes = 0;
        } else {
            out.extend(std::iter::repeat_n(b'\\' as u16, backslashes));
            backslashes = 0;
            out.push(ch);
        }
    }
    out.extend(std::iter::repeat_n(b'\\' as u16, backslashes * 2));
    out.push(b'"' as u16);
}

fn print_timings(timings: &Timings) {
    println!(
        "timing: git={} scan={} process={} write={}",
        format_elapsed_secs(timings.git),
        format_elapsed_secs(timings.scan),
        format_elapsed_secs(timings.process),
        format_elapsed_secs(timings.write)
    );
}

fn print_index_profile(timings: &Timings) {
    let stderr = std::io::stderr();
    let mut err = BufWriter::new(stderr.lock());
    let _ = append_index_profile(&mut err, timings);
    let _ = err.flush();
}

fn append_index_profile<W: Write>(out: &mut W, timings: &Timings) -> Result<()> {
    let events = [
        ("index_git", timings.git),
        ("index_open_existing", timings.open_index),
        ("index_scan_walk_filter_meta", timings.scan),
        ("index_current_meta", timings.current_meta),
        ("index_change_diff", timings.change_diff),
        ("index_file_read", timings.file_read),
        ("index_tokenize", timings.tokenize),
        ("index_tokenize_scan_keys", timings.tokenize_scan_keys),
        (
            "index_tokenize_qualified_calls",
            timings.tokenize_qualified_calls,
        ),
        ("index_tokenize_sort_extras", timings.tokenize_sort_extras),
        (
            "index_tokenize_sort_fragments",
            timings.tokenize_sort_fragments,
        ),
        ("index_compress", timings.compress),
        ("index_sort_files", timings.sort),
        ("index_select_fragments", timings.select_fragments),
        ("index_build_postings", timings.postings),
        ("index_build_posting_chunks", timings.postings_build_chunks),
        ("index_merge_postings", timings.postings_merge),
        ("index_process_total", timings.process),
        ("index_write_prepare_files", timings.write_prepare_files),
        (
            "index_write_prepare_postings",
            timings.write_prepare_postings,
        ),
        ("index_write_prepare_chunks", timings.write_prepare_chunks),
        ("index_write_header_tables", timings.write_header_tables),
        ("index_write_postings_paths", timings.write_postings_paths),
        ("index_write_content", timings.write_content),
        ("index_write_flush_publish", timings.write_flush_publish),
        ("index_write_state", timings.write_state),
        ("index_write_delta_meta", timings.write_delta_meta),
        ("index_write_total", timings.write),
    ];
    for (name, secs) in events {
        if secs > 0.0 {
            writeln!(out, "profile: {name}={}", format_elapsed_secs(secs))?;
        }
    }
    let counters = [
        ("index_cpu_threads", timings.index_cpu_threads),
        ("index_io_threads", timings.index_io_threads),
        ("index_posting_merge_shards", timings.postings_merge_shards),
        ("index_files", timings.indexed_files),
        ("index_bytes", timings.indexed_bytes),
        ("index_gram_keys", timings.gram_keys),
        ("index_extra_keys", timings.extra_keys),
        ("index_fragment_keys", timings.fragment_keys),
    ];
    for (name, value) in counters {
        if value != 0 {
            writeln!(out, "profile: {name}={value}")?;
        }
    }
    Ok(())
}

fn timing_summary(timings: &Timings) -> String {
    format!(
        "git={} scan={} process={} write={}",
        format_elapsed_secs(timings.git),
        format_elapsed_secs(timings.scan),
        format_elapsed_secs(timings.process),
        format_elapsed_secs(timings.write)
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

fn write_varint_postings(ids: &[u32], out: &mut Vec<u8>) {
    let mut previous = 0u32;
    for &id in ids {
        let delta = id.wrapping_sub(previous);
        write_varint_u32(delta, out);
        previous = id;
    }
}

fn write_varint_u32(mut value: u32, out: &mut Vec<u8>) {
    while value >= 0x80 {
        out.push((value as u8) | 0x80);
        value >>= 7;
    }
    out.push(value as u8);
}

fn read_varint_postings(data: &[u8], count: usize) -> Result<Vec<u32>> {
    let mut out = Vec::with_capacity(count);
    let mut offset = 0usize;
    let mut previous = 0u32;
    while out.len() < count {
        let delta = read_varint_u32(data, &mut offset)?;
        let id = previous.wrapping_add(delta);
        out.push(id);
        previous = id;
    }
    Ok(out)
}

fn read_varint_u32(data: &[u8], offset: &mut usize) -> Result<u32> {
    let mut value = 0u32;
    let mut shift = 0u32;
    loop {
        let byte = *data
            .get(*offset)
            .ok_or_else(|| anyhow!("truncated posting list"))?;
        *offset += 1;
        value |= ((byte & 0x7f) as u32) << shift;
        if byte & 0x80 == 0 {
            return Ok(value);
        }
        shift += 7;
        if shift >= 32 {
            bail!("invalid posting varint");
        }
    }
}

fn read_u32_at(data: &[u8], offset: usize) -> Result<u32> {
    let bytes: [u8; 4] = data
        .get(offset..offset + 4)
        .ok_or_else(|| anyhow!("index offset out of range"))?
        .try_into()?;
    Ok(u32::from_le_bytes(bytes))
}

fn read_u32_from_reader(reader: &mut impl Read) -> Result<u32> {
    let mut bytes = [0u8; 4];
    reader.read_exact(&mut bytes)?;
    Ok(u32::from_le_bytes(bytes))
}

fn write_bytes_frame(writer: &mut impl Write, bytes: &[u8]) -> Result<()> {
    write_u64(writer, bytes.len() as u64)?;
    writer.write_all(bytes)?;
    Ok(())
}

fn read_bytes_frame(reader: &mut impl Read) -> Result<Vec<u8>> {
    let len = read_u64_from_reader(reader)?;
    if len > usize::MAX as u64 {
        bail!("frame too large");
    }
    let mut bytes = vec![0u8; len as usize];
    reader.read_exact(&mut bytes)?;
    Ok(bytes)
}

fn copy_bytes_frame(reader: &mut impl Read, writer: &mut impl Write) -> Result<()> {
    let mut remaining = read_u64_from_reader(reader)?;
    let mut buffer = vec![0u8; SEARCH_DAEMON_STREAM_BUFFER_SIZE];
    while remaining != 0 {
        let take = remaining.min(buffer.len() as u64) as usize;
        reader.read_exact(&mut buffer[..take])?;
        writer.write_all(&buffer[..take])?;
        remaining -= take as u64;
    }
    Ok(())
}

fn read_string_frame(reader: &mut impl Read) -> Result<String> {
    Ok(String::from_utf8(read_bytes_frame(reader)?)?)
}

fn read_u64_at(data: &[u8], offset: usize) -> Result<u64> {
    let bytes: [u8; 8] = data
        .get(offset..offset + 8)
        .ok_or_else(|| anyhow!("index offset out of range"))?
        .try_into()?;
    Ok(u64::from_le_bytes(bytes))
}

fn read_u64_from_reader(reader: &mut impl Read) -> Result<u64> {
    let mut bytes = [0u8; 8];
    reader.read_exact(&mut bytes)?;
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

#[cfg(test)]
mod tests {
    use super::*;

    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_string()).collect()
    }

    #[test]
    fn unsupported_rg_flags_are_recorded_and_ignored() {
        let options =
            parse_search_args(&args(["--pcre2", "--pre", "cat", "Needle", "."].as_slice()))
                .unwrap();

        assert_eq!(options.pattern, "Needle");
        assert_eq!(options.paths, vec![".".to_string()]);
        assert!(
            options
                .compatibility_notes
                .iter()
                .any(|note| note.flag == "--pcre2")
        );
        assert!(
            options
                .compatibility_notes
                .iter()
                .any(|note| note.flag == "--pre")
        );
    }

    #[test]
    fn type_and_max_depth_filters_restrict_paths() {
        let mut options =
            parse_search_args(&args(["-tcpp", "--max-depth", "0", "Needle"].as_slice())).unwrap();
        let root = env::current_dir().unwrap();
        options.cwd = root.clone();
        let filter = PathFilter::new(&options, &root).unwrap();
        let excluded = HashSet::default();

        assert!(filter.allows("Foo.cpp", &excluded));
        assert!(!filter.allows("Source/Foo.cpp", &excluded));
        assert!(!filter.allows("Foo.py", &excluded));
    }

    #[test]
    fn glob_case_insensitive_matches_paths_without_global_cost() {
        let mut options = parse_search_args(&args(
            ["--glob-case-insensitive", "-g", "*.CPP", "Needle"].as_slice(),
        ))
        .unwrap();
        let root = env::current_dir().unwrap();
        options.cwd = root.clone();
        let filter = PathFilter::new(&options, &root).unwrap();
        let excluded = HashSet::default();

        assert!(filter.allows("Source/Foo.cpp", &excluded));
    }

    #[test]
    fn ignore_file_adds_negative_path_filters() {
        let ignore_path = env::temp_dir().join(format!(
            "indexsearch-ignore-{}-{}.txt",
            std::process::id(),
            1
        ));
        fs::write(&ignore_path, "Generated/\n").unwrap();

        let mut options = parse_search_args(&args(
            ["--ignore-file", ignore_path.to_str().unwrap(), "Needle"].as_slice(),
        ))
        .unwrap();
        let root = env::current_dir().unwrap();
        options.cwd = root.clone();
        let filter = PathFilter::new(&options, &root).unwrap();
        let excluded = HashSet::default();

        assert!(!filter.allows("Generated/Foo.cpp", &excluded));
        assert!(filter.allows("Source/Foo.cpp", &excluded));

        let _ = fs::remove_file(ignore_path);
    }

    #[test]
    fn implicit_cwd_filter_matches_explicit_dot_filter() {
        let root = env::current_dir().unwrap();
        let canonical_root = fs::canonicalize(&root).unwrap_or_else(|_| root.clone());
        let mut options = parse_search_args(&args(["Needle"].as_slice())).unwrap();
        options.cwd = root.join("tests");

        let filter = PathFilter::new(&options, &canonical_root).unwrap();
        let excluded = HashSet::default();

        assert!(filter.allows("tests/smoke.sh", &excluded));
        assert!(!filter.allows("src/main.rs", &excluded));
    }

    #[test]
    fn rg_line_flags_render_expected_results() {
        let options = parse_search_args(&args(["--count-matches", "-F", "e"].as_slice())).unwrap();
        let matcher = QueryMatcher::new(&options).unwrap();
        let rendered = matcher
            .search_file_rendered(b"needle nested\n", "a.txt", &options, true)
            .unwrap()
            .unwrap();
        assert_eq!(String::from_utf8(rendered.output).unwrap(), "a.txt:5\n");

        let options = parse_search_args(&args(["-x", "-F", "needle"].as_slice())).unwrap();
        let matcher = QueryMatcher::new(&options).unwrap();
        assert!(matcher.search_file_has_match(b"needle\n", &options));
        assert!(!matcher.search_file_has_match(b"needle here\n", &options));

        let options =
            parse_search_args(&args(["--files-without-match", "-F", "needle"].as_slice())).unwrap();
        let matcher = QueryMatcher::new(&options).unwrap();
        let rendered = matcher
            .search_file_rendered(b"other\n", "b.txt", &options, true)
            .unwrap()
            .unwrap();
        assert_eq!(String::from_utf8(rendered.output).unwrap(), "b.txt\n");
        assert!(
            matcher
                .search_file_rendered(b"needle\n", "b.txt", &options, true)
                .unwrap()
                .is_none()
        );
    }
}
