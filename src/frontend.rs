use std::env;
use std::fs;
use std::io::{self, IsTerminal, Read, Write};
use std::net::{SocketAddr, TcpStream};
#[cfg(unix)]
use std::os::fd::AsRawFd;
#[cfg(unix)]
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use globset::{GlobBuilder, GlobSet, GlobSetBuilder};

const INDEX_DIR: &str = ".indexsearch";
const INDEX_FILE: &str = "index.bin";
const PROJECT_CONFIG_FILE: &str = "is-project-config.txt";
const LEGACY_PROJECT_FILE: &str = "index-search-project.txt";
const SEARCH_DAEMON_FILE: &str = "search-daemon.txt";
const MAINTENANCE_FILE: &str = "maintenance.txt";
const PROJECTS_DIR: &str = "projects";
const REQUEST_MAGIC: &[u8; 8] = b"ISDREQ1\n";
const RESPONSE_MAGIC: &[u8; 8] = b"ISDRES1\n";
const STDOUT_FRAME: u8 = 1;
const STDERR_FRAME: u8 = 2;
const DONE_FRAME: u8 = 3;
const CONNECT_TIMEOUT: Duration = Duration::from_millis(20);
#[cfg(windows)]
const START_TIMEOUT: Duration = Duration::from_secs(10);
#[cfg(not(windows))]
const START_TIMEOUT: Duration = Duration::from_secs(2);
const MAINTENANCE_WAIT_TIMEOUT: Duration = Duration::from_secs(600);
const MAINTENANCE_WAIT_INTERVAL: Duration = Duration::from_millis(100);
#[cfg(windows)]
const FRAME_COPY_BUFFER_SIZE: usize = 256 * 1024;
#[cfg(not(windows))]
const FRAME_COPY_BUFFER_SIZE: usize = 64 * 1024;

#[derive(Default)]
struct FrontendProfile {
    enabled: bool,
    events: Vec<FrontendProfileEvent>,
}

enum FrontendProfileEvent {
    DurationMillis(&'static str, f64),
    Value(&'static str, f64),
}

impl FrontendProfile {
    fn new(args: &[String]) -> Self {
        Self {
            enabled: args.iter().any(|arg| {
                matches!(
                    arg.as_str(),
                    "--profile" | "--instrument" | "--profile-search"
                )
            }),
            events: Vec::new(),
        }
    }

    fn record(&mut self, name: &'static str, start: Instant) {
        if self.enabled {
            self.events.push(FrontendProfileEvent::DurationMillis(
                name,
                start.elapsed().as_secs_f64() * 1000.0,
            ));
        }
    }

    fn record_value(&mut self, name: &'static str, value: f64) {
        if self.enabled {
            self.events.push(FrontendProfileEvent::Value(name, value));
        }
    }

    fn print(&self) {
        if !self.enabled {
            return;
        }
        let stderr = io::stderr();
        let mut err = io::BufWriter::new(stderr.lock());
        for event in &self.events {
            match event {
                FrontendProfileEvent::DurationMillis(name, ms) => {
                    let _ = writeln!(err, "profile: {name}={}", format_elapsed_millis(*ms));
                }
                FrontendProfileEvent::Value(name, value) => {
                    let _ = writeln!(err, "profile: {name}={value:.3}");
                }
            }
        }
        let _ = err.flush();
    }
}

fn format_elapsed_millis(ms: f64) -> String {
    if ms > 5_000.0 {
        format!("{:.3}s", ms / 1000.0)
    } else {
        format!("{ms:.3}ms")
    }
}

fn main() {
    let args: Vec<String> = env::args().skip(1).collect();
    if args
        .first()
        .is_some_and(|arg| arg == "--__indexsearch-frontend-noop")
    {
        return;
    }
    let command_name = frontend_command_name();
    let result = if command_name == "isgrep" {
        run_grep_frontend(&args)
    } else {
        run(&args)
    };
    match result {
        Ok(code) => std::process::exit(code),
        Err(message) => {
            eprintln!("{command_name}: {message}");
            std::process::exit(2);
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum GrepRegexMode {
    Basic,
    Extended,
    Fixed,
}

enum GrepTranslation {
    Search(Vec<String>),
    FallbackToGrep,
}

enum ExternalFallback<'a> {
    Ripgrep,
    Grep { original_args: &'a [String] },
}

fn run_grep_frontend(args: &[String]) -> Result<i32, String> {
    let first = first_search_flag(args);
    if args.is_empty() {
        print_isgrep_help();
        return Ok(2);
    }
    if first.is_some_and(|arg| arg == "--help") {
        print_isgrep_help();
        return Ok(0);
    }
    if first.is_some_and(|arg| matches!(arg, "-V" | "--version")) {
        println!("{} {}", frontend_command_name(), display_version());
        return Ok(0);
    }
    match translate_grep_args(args)? {
        GrepTranslation::Search(search_args) => run_with_fallback(
            &search_args,
            ExternalFallback::Grep {
                original_args: args,
            },
        ),
        GrepTranslation::FallbackToGrep => run_system_grep(args),
    }
}

fn print_isgrep_help() {
    println!("Usage");
    println!("  isgrep [GREP_OPTIONS] PATTERN [FILE ...]");
    println!();
    println!("Description");
    println!("  grep-compatible frontend for indexed IndexSearch searches");
    println!("  Defaults to grep Basic Regex syntax, so `A\\|B` works as alternation.");
    println!("  Unsupported grep-only semantics fall back to the system grep when available.");
    println!();
    println!("Common Options");
    println!("  -G, --basic-regexp              use grep Basic Regex syntax (default)");
    println!("  -E, --extended-regexp           use extended regex syntax");
    println!("  -F, --fixed-strings             treat patterns as literals");
    println!("  -e, --regexp PATTERN            search pattern");
    println!("  -f, --file FILE                 read patterns from file");
    println!("  -i, -v, -w, -x                  ignore-case, invert, word, line match");
    println!("  -n, -H, -h                      line numbers and filename controls");
    println!("  -l, -L, -c, -o, -q              file/count/only/quiet modes");
    println!("  -A NUM, -B NUM, -C NUM          context lines");
    println!("  -r, -R                          accepted for recursive grep commands");
}

fn translate_grep_args(args: &[String]) -> Result<GrepTranslation, String> {
    let mut out = Vec::new();
    let mut patterns = Vec::new();
    let mut paths = Vec::new();
    let mut mode = GrepRegexMode::Basic;
    let mut positional_only = false;
    let mut filename_override = false;
    let mut i = 0usize;
    while i < args.len() {
        let arg = &args[i];
        if positional_only {
            consume_grep_positional(arg, &mut patterns, &mut paths);
            i += 1;
            continue;
        }
        if arg == "--" {
            positional_only = true;
            i += 1;
            continue;
        }
        if let Some(long) = arg.strip_prefix("--") {
            if long.is_empty() {
                positional_only = true;
                i += 1;
                continue;
            }
            match translate_grep_long_option(
                args,
                &mut i,
                long,
                &mut out,
                &mut patterns,
                &mut mode,
                &mut filename_override,
            )? {
                GrepOptionOutcome::Translated => {
                    i += 1;
                    continue;
                }
                GrepOptionOutcome::Fallback => return Ok(GrepTranslation::FallbackToGrep),
            }
        }
        if arg.starts_with('-') && arg != "-" {
            match translate_grep_short_options(
                args,
                &mut i,
                arg,
                &mut out,
                &mut patterns,
                &mut mode,
                &mut filename_override,
            )? {
                GrepOptionOutcome::Translated => {
                    i += 1;
                    continue;
                }
                GrepOptionOutcome::Fallback => return Ok(GrepTranslation::FallbackToGrep),
            }
        }
        consume_grep_positional(arg, &mut patterns, &mut paths);
        i += 1;
    }

    if patterns.is_empty() {
        return Err("a grep pattern is required".to_string());
    }
    if paths.is_empty() && !io::stdin().is_terminal() {
        return Ok(GrepTranslation::FallbackToGrep);
    }
    if !filename_override && paths.len() == 1 && PathBuf::from(&paths[0]).is_file() {
        out.push("-I".to_string());
    }
    let Some(pattern) = build_grep_search_pattern(&patterns, mode) else {
        return Ok(GrepTranslation::FallbackToGrep);
    };
    if mode == GrepRegexMode::Fixed && patterns.len() == 1 && !patterns[0].is_empty() {
        out.push("-F".to_string());
    }
    out.push("--".to_string());
    out.push(pattern);
    out.extend(paths);
    Ok(GrepTranslation::Search(out))
}

enum GrepOptionOutcome {
    Translated,
    Fallback,
}

fn consume_grep_positional(arg: &str, patterns: &mut Vec<String>, paths: &mut Vec<String>) {
    if patterns.is_empty() {
        patterns.push(arg.to_string());
    } else {
        paths.push(arg.to_string());
    }
}

fn translate_grep_long_option(
    args: &[String],
    i: &mut usize,
    long: &str,
    out: &mut Vec<String>,
    patterns: &mut Vec<String>,
    mode: &mut GrepRegexMode,
    filename_override: &mut bool,
) -> Result<GrepOptionOutcome, String> {
    let (name, inline_value) = long
        .split_once('=')
        .map(|(name, value)| (name, Some(value.to_string())))
        .unwrap_or((long, None));
    match name {
        "help" => {
            print_isgrep_help();
            std::process::exit(0);
        }
        "version" => {
            println!("{} {}", frontend_command_name(), display_version());
            std::process::exit(0);
        }
        "basic-regexp" => *mode = GrepRegexMode::Basic,
        "extended-regexp" => *mode = GrepRegexMode::Extended,
        "fixed-strings" => *mode = GrepRegexMode::Fixed,
        "perl-regexp" => return Ok(GrepOptionOutcome::Fallback),
        "regexp" => patterns.push(grep_option_value(args, i, "--regexp", inline_value)?),
        "file" => {
            let file = grep_option_value(args, i, "--file", inline_value)?;
            let Ok(text) = fs::read_to_string(file) else {
                return Ok(GrepOptionOutcome::Fallback);
            };
            patterns.extend(text.lines().map(str::to_string));
        }
        "ignore-case" => out.push("-i".to_string()),
        "invert-match" => out.push("-v".to_string()),
        "word-regexp" => out.push("-w".to_string()),
        "line-regexp" => out.push("-x".to_string()),
        "line-number" => out.push("-n".to_string()),
        "with-filename" => {
            *filename_override = true;
            out.push("-H".to_string());
        }
        "no-filename" => {
            *filename_override = true;
            out.push("-I".to_string());
        }
        "files-with-matches" => out.push("-l".to_string()),
        "files-without-match" => out.push("--files-without-match".to_string()),
        "count" => out.push("-c".to_string()),
        "only-matching" => out.push("-o".to_string()),
        "quiet" | "silent" => out.push("-q".to_string()),
        "no-messages" => out.push("--no-messages".to_string()),
        "after-context" => push_grep_value(out, "-A", grep_option_value(args, i, "--after-context", inline_value)?),
        "before-context" => push_grep_value(out, "-B", grep_option_value(args, i, "--before-context", inline_value)?),
        "context" => push_grep_value(out, "-C", grep_option_value(args, i, "--context", inline_value)?),
        "max-count" => push_grep_value(out, "-m", grep_option_value(args, i, "--max-count", inline_value)?),
        "include" => push_grep_value(out, "-g", grep_option_value(args, i, "--include", inline_value)?),
        "exclude" => {
            let value = grep_option_value(args, i, "--exclude", inline_value)?;
            push_grep_value(out, "-g", format!("!{value}"));
        }
        "exclude-dir" => {
            let value = grep_option_value(args, i, "--exclude-dir", inline_value)?;
            push_grep_value(out, "-g", format!("!{value}/**"));
            push_grep_value(out, "-g", format!("!**/{value}/**"));
        }
        "recursive" => {}
        "dereference-recursive" => out.push("--follow".to_string()),
        "color" | "colour" => {
            let value = inline_value.unwrap_or_else(|| "auto".to_string());
            out.push(format!("--color={value}"));
        }
        "binary-files" => {
            let _ = inline_value.unwrap_or_else(|| "binary".to_string());
        }
        "text" | "binary" | "devices" | "mmap" | "line-buffered" | "initial-tab" => {}
        "directories" => {
            let value = grep_option_value(args, i, "--directories", inline_value)?;
            if value != "recurse" {
                return Ok(GrepOptionOutcome::Fallback);
            }
        }
        "null" | "null-data" | "byte-offset" | "label" => return Ok(GrepOptionOutcome::Fallback),
        _ => return Ok(GrepOptionOutcome::Fallback),
    }
    Ok(GrepOptionOutcome::Translated)
}

fn translate_grep_short_options(
    args: &[String],
    i: &mut usize,
    arg: &str,
    out: &mut Vec<String>,
    patterns: &mut Vec<String>,
    mode: &mut GrepRegexMode,
    filename_override: &mut bool,
) -> Result<GrepOptionOutcome, String> {
    let bytes = arg.as_bytes();
    let mut pos = 1usize;
    while pos < bytes.len() {
        let flag = bytes[pos] as char;
        match flag {
            'G' => *mode = GrepRegexMode::Basic,
            'E' => *mode = GrepRegexMode::Extended,
            'F' => *mode = GrepRegexMode::Fixed,
            'P' | 'Z' | 'z' | 'b' => return Ok(GrepOptionOutcome::Fallback),
            'e' => {
                patterns.push(grep_short_value(args, i, arg, pos, "-e")?);
                return Ok(GrepOptionOutcome::Translated);
            }
            'f' => {
                let file = grep_short_value(args, i, arg, pos, "-f")?;
                let Ok(text) = fs::read_to_string(file) else {
                    return Ok(GrepOptionOutcome::Fallback);
                };
                patterns.extend(text.lines().map(str::to_string));
                return Ok(GrepOptionOutcome::Translated);
            }
            'A' => {
                push_grep_value(out, "-A", grep_short_value(args, i, arg, pos, "-A")?);
                return Ok(GrepOptionOutcome::Translated);
            }
            'B' => {
                push_grep_value(out, "-B", grep_short_value(args, i, arg, pos, "-B")?);
                return Ok(GrepOptionOutcome::Translated);
            }
            'C' => {
                push_grep_value(out, "-C", grep_short_value(args, i, arg, pos, "-C")?);
                return Ok(GrepOptionOutcome::Translated);
            }
            'm' => {
                push_grep_value(out, "-m", grep_short_value(args, i, arg, pos, "-m")?);
                return Ok(GrepOptionOutcome::Translated);
            }
            'd' => {
                let value = grep_short_value(args, i, arg, pos, "-d")?;
                if value != "recurse" {
                    return Ok(GrepOptionOutcome::Fallback);
                }
                return Ok(GrepOptionOutcome::Translated);
            }
            'D' => return Ok(GrepOptionOutcome::Fallback),
            'i' | 'y' => out.push("-i".to_string()),
            'v' => out.push("-v".to_string()),
            'w' => out.push("-w".to_string()),
            'x' => out.push("-x".to_string()),
            'n' => out.push("-n".to_string()),
            'H' => {
                *filename_override = true;
                out.push("-H".to_string());
            }
            'h' => {
                *filename_override = true;
                out.push("-I".to_string());
            }
            'l' => out.push("-l".to_string()),
            'L' => out.push("--files-without-match".to_string()),
            'c' => out.push("-c".to_string()),
            'o' => out.push("-o".to_string()),
            'q' => out.push("-q".to_string()),
            's' => out.push("--no-messages".to_string()),
            'r' => {}
            'R' => out.push("--follow".to_string()),
            'a' | 'I' | 'T' | 'U' | 'u' => {}
            'V' => {
                println!("{} {}", frontend_command_name(), display_version());
                std::process::exit(0);
            }
            _ => return Ok(GrepOptionOutcome::Fallback),
        }
        pos += 1;
    }
    Ok(GrepOptionOutcome::Translated)
}

fn grep_option_value(
    args: &[String],
    i: &mut usize,
    name: &str,
    inline_value: Option<String>,
) -> Result<String, String> {
    if let Some(value) = inline_value {
        return Ok(value);
    }
    *i += 1;
    args.get(*i)
        .cloned()
        .ok_or_else(|| format!("option {name} requires an argument"))
}

fn grep_short_value(args: &[String], i: &mut usize, arg: &str, pos: usize, name: &str) -> Result<String, String> {
    if pos + 1 < arg.len() {
        Ok(arg[pos + 1..].to_string())
    } else {
        *i += 1;
        args.get(*i)
            .cloned()
            .ok_or_else(|| format!("option {name} requires an argument"))
    }
}

fn push_grep_value(out: &mut Vec<String>, flag: &str, value: String) {
    out.push(flag.to_string());
    out.push(value);
}

fn build_grep_search_pattern(patterns: &[String], mode: GrepRegexMode) -> Option<String> {
    let mut converted = Vec::with_capacity(patterns.len());
    for pattern in patterns {
        if grep_pattern_has_backreference(pattern) {
            return None;
        }
        converted.push(match mode {
            GrepRegexMode::Basic => translate_basic_grep_regex(pattern),
            GrepRegexMode::Extended => pattern.clone(),
            GrepRegexMode::Fixed => regex::escape(pattern),
        });
    }
    if mode == GrepRegexMode::Fixed && patterns.len() == 1 && !patterns[0].is_empty() {
        return patterns.first().cloned();
    }
    Some(join_regex_alternates(&converted))
}

fn join_regex_alternates(patterns: &[String]) -> String {
    if patterns.len() == 1 {
        if patterns[0].is_empty() {
            "(?:)".to_string()
        } else {
            patterns[0].clone()
        }
    } else {
        patterns
            .iter()
            .map(|pattern| {
                if pattern.is_empty() {
                    "(?:)".to_string()
                } else {
                    format!("(?:{pattern})")
                }
            })
            .collect::<Vec<_>>()
            .join("|")
    }
}

fn translate_basic_grep_regex(pattern: &str) -> String {
    let mut out = String::with_capacity(pattern.len());
    let mut chars = pattern.chars();
    while let Some(ch) = chars.next() {
        if ch == '\\' {
            if let Some(next) = chars.next() {
                if matches!(next, '|' | '(' | ')' | '+' | '?' | '{' | '}') {
                    out.push(next);
                } else {
                    out.push('\\');
                    out.push(next);
                }
            } else {
                out.push('\\');
            }
        } else if matches!(ch, '|' | '(' | ')' | '+' | '?' | '{' | '}') {
            out.push('\\');
            out.push(ch);
        } else {
            out.push(ch);
        }
    }
    out
}

fn grep_pattern_has_backreference(pattern: &str) -> bool {
    let mut escaped = false;
    for ch in pattern.chars() {
        if escaped {
            if matches!(ch, '1'..='9') {
                return true;
            }
            escaped = false;
        } else if ch == '\\' {
            escaped = true;
        }
    }
    false
}

fn run_system_grep(args: &[String]) -> Result<i32, String> {
    let mut command = system_grep_command();
    command.args(args);
    let status = command.status().map_err(|err| {
        format!(
            "cannot translate this grep invocation and failed to run system grep: {err}"
        )
    })?;
    Ok(status.code().unwrap_or(1))
}

fn run_system_grep_or_translated_ripgrep(
    grep_args: &[String],
    ripgrep_args: &[String],
) -> Result<i32, String> {
    let mut command = system_grep_command();
    command.args(grep_args);
    match command.status() {
        Ok(status) => Ok(status.code().unwrap_or(1)),
        Err(err) if err.kind() == io::ErrorKind::NotFound => run_system_ripgrep(ripgrep_args),
        Err(err) => Err(format!(
            "translated grep fallback failed to run system grep: {err}"
        )),
    }
}

fn system_grep_command() -> Command {
    let current = env::current_exe().ok();
    for candidate in ["/usr/bin/grep", "/bin/grep"] {
        let path = PathBuf::from(candidate);
        if path.is_file()
            && current
                .as_ref()
                .is_none_or(|exe| !same_fileish(&path, exe))
        {
            return Command::new(path);
        }
    }
    Command::new("grep")
}

fn run(args: &[String]) -> Result<i32, String> {
    run_with_fallback(args, ExternalFallback::Ripgrep)
}

fn run_with_fallback(args: &[String], fallback: ExternalFallback<'_>) -> Result<i32, String> {
    let first = first_search_flag(args);
    if args.is_empty() || first.is_some_and(|arg| matches!(arg, "-h" | "--help")) {
        return run_backend_search_help();
    }
    if first.is_some_and(|arg| matches!(arg, "-V" | "--version")) {
        println!("{} {}", frontend_command_name(), display_version());
        return Ok(0);
    }
    let total_timer = Instant::now();
    let mut profile = FrontendProfile::new(args);
    let prepare_timer = Instant::now();
    let search_args = args.to_vec();
    profile.record("frontend_prepare_args", prepare_timer);
    if should_fallback_to_ripgrep_stdin(&search_args, stdin_has_searchable_stream()) {
        return run_system_ripgrep(&ripgrep_stdin_args(&search_args));
    }
    if search_args.iter().any(|arg| arg == "--auto-update") {
        return run_backend_search(&search_args);
    }
    if search_args.iter().any(|arg| arg == "--type-list") {
        return run_backend_search(&search_args);
    }
    let path_timer = Instant::now();
    let Some(search_paths) = search_path_args(&search_args) else {
        return run_backend_search(&search_args);
    };
    profile.record("frontend_resolve_search_paths", path_timer);
    if agent_auto_project_mode()
        && search_paths
            .iter()
            .any(|path| find_project_root(&path.abs).is_none())
    {
        let create_timer = Instant::now();
        let root = agent_auto_project_root(&search_paths)?;
        let _ = ensure_project_service(&root, &mut profile)?;
        profile.record("frontend_agent_auto_project", create_timer);
    }
    let root_timer = Instant::now();
    let runs = match search_runs(&search_paths) {
        Ok(groups) => groups,
        Err(MissingProject::Single) => return handle_missing_project(args),
        Err(MissingProject::Path(path)) => {
            return Err(format!(
                "no IndexSearch project found above {}; run `istool index` in that project first",
                display_path(&path)
            ));
        }
    };
    profile.record("frontend_find_project_roots", root_timer);
    if runs.len() > 1 {
        profile.record_value("frontend_search_run_count", runs.len() as f64);
    }
    let mut final_code = 1;
    for run in runs {
        let code = match run.kind {
            SearchRunKind::Indexed(root) => {
                let group_args = search_args_for_group(&search_args, &run.path_indexes);
                run_indexed_search_group(&group_args, root, &mut profile)?
            }
            SearchRunKind::External => match &fallback {
                ExternalFallback::Ripgrep => {
                    let group_args = search_args_for_group(&search_args, &run.path_indexes);
                    run_system_ripgrep(&ripgrep_stdin_args(&group_args))?
                }
                ExternalFallback::Grep { original_args } => {
                    let group_args = grep_args_for_path_ordinals(original_args, &run.path_ordinals);
                    let ripgrep_group_args =
                        search_args_for_group(&search_args, &run.path_indexes);
                    run_system_grep_or_translated_ripgrep(
                        &group_args,
                        &ripgrep_stdin_args(&ripgrep_group_args),
                    )?
                }
            },
        };
        final_code = combine_exit_codes(final_code, code);
    }
    profile.record("frontend_total", total_timer);
    profile.print();
    return Ok(final_code);
}

fn first_search_flag(args: &[String]) -> Option<&str> {
    let mut i = 0;
    while args
        .get(i)
        .is_some_and(|arg| {
            matches!(
                arg.as_str(),
                "--profile" | "--instrument" | "--profile-search"
            )
        })
    {
        i += 1;
    }
    args.get(i).map(String::as_str)
}

fn display_version() -> &'static str {
    concat!(env!("CARGO_PKG_VERSION"), "+", env!("INDEXSEARCH_BUILD_ID"))
}

fn should_fallback_to_ripgrep_stdin(args: &[String], stdin_is_searchable_stream: bool) -> bool {
    if !stdin_is_searchable_stream || args.is_empty() {
        return false;
    }
    if first_search_flag(args).is_some_and(|arg| matches!(arg, "-h" | "--help" | "-V" | "--version")) {
        return false;
    }
    if args.iter().any(|arg| matches!(arg.as_str(), "--files" | "--type-list")) {
        return false;
    }
    let Some(positionals) = search_positionals(args) else {
        return false;
    };
    if !positionals.saw_pattern {
        return false;
    }
    positionals.paths.is_empty() || positionals.paths.iter().any(|path| path == "-")
}

#[derive(Default)]
struct SearchPositionals {
    saw_pattern: bool,
    paths: Vec<String>,
}

fn search_positionals(args: &[String]) -> Option<SearchPositionals> {
    let mut out = SearchPositionals::default();
    let mut after_double_dash = false;
    let mut files_mode = false;
    let mut i = 0usize;
    while i < args.len() {
        let arg = &args[i];
        if after_double_dash {
            consume_search_positional(arg, &mut out, files_mode);
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
        if arg.starts_with("--regexp=") || (arg.starts_with("-e") && arg.len() > 2) {
            out.saw_pattern = true;
            i += 1;
            continue;
        }
        if arg == "-e" || arg == "--regexp" {
            out.saw_pattern = true;
            i += 2;
            continue;
        }
        if option_takes_value(arg) {
            i += 2;
            continue;
        }
        if arg.starts_with("--") {
            i += 1;
            continue;
        }
        if short_option_with_attached_value(arg) || (arg.starts_with('-') && arg != "-" && arg.len() > 1) {
            i += 1;
            continue;
        }
        consume_search_positional(arg, &mut out, files_mode);
        i += 1;
    }
    Some(out)
}

fn consume_search_positional(arg: &str, out: &mut SearchPositionals, files_mode: bool) {
    if files_mode || out.saw_pattern {
        out.paths.push(arg.to_string());
    } else {
        out.saw_pattern = true;
    }
}

fn ripgrep_stdin_args(args: &[String]) -> Vec<String> {
    args.iter()
        .filter(|arg| {
            !matches!(
                arg.as_str(),
                "--profile"
                    | "--instrument"
                    | "--profile-search"
                    | "--auto-update"
                    | "--no-auto-index"
                    | "--no-daemon"
            )
        })
        .cloned()
        .collect()
}

fn run_system_ripgrep(args: &[String]) -> Result<i32, String> {
    let mut command = system_ripgrep_command();
    command.args(args);
    let status = command.status().map_err(|err| {
        format!("stdin search requires ripgrep, but failed to run rg: {err}")
    })?;
    Ok(status.code().unwrap_or(1))
}

fn system_ripgrep_command() -> Command {
    Command::new("rg")
}

fn stdin_has_searchable_stream() -> bool {
    if io::stdin().is_terminal() {
        return false;
    }
    stdin_file_type_is_searchable()
}

#[cfg(unix)]
fn stdin_file_type_is_searchable() -> bool {
    let stdin = io::stdin();
    let fd = stdin.as_raw_fd();
    let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
    let rc = unsafe { libc::fstat(fd, stat.as_mut_ptr()) };
    if rc != 0 {
        return false;
    }
    let stat = unsafe { stat.assume_init() };
    let kind = stat.st_mode & libc::S_IFMT;
    kind == libc::S_IFIFO || kind == libc::S_IFREG || kind == libc::S_IFSOCK
}

#[cfg(windows)]
fn stdin_file_type_is_searchable() -> bool {
    use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
    use windows_sys::Win32::Storage::FileSystem::{FILE_TYPE_DISK, FILE_TYPE_PIPE, GetFileType};
    use windows_sys::Win32::System::Console::{GetStdHandle, STD_INPUT_HANDLE};

    unsafe {
        let handle = GetStdHandle(STD_INPUT_HANDLE);
        if handle.is_null() || handle == INVALID_HANDLE_VALUE {
            return false;
        }
        matches!(GetFileType(handle), FILE_TYPE_PIPE | FILE_TYPE_DISK)
    }
}

#[cfg(not(any(unix, windows)))]
fn stdin_file_type_is_searchable() -> bool {
    !io::stdin().is_terminal()
}

fn load_frontend_project_config(root: &Path) -> Result<FrontendProjectConfig, String> {
    let config = project_config_path(root);
    let legacy = legacy_project_config_path(root);
    let text = if config.is_file() {
        fs::read_to_string(config).map_err(|err| err.to_string())?
    } else if legacy.is_file() {
        fs::read_to_string(legacy).map_err(|err| err.to_string())?
    } else {
        return Ok(FrontendProjectConfig::default());
    };
    let sections = parse_config_sections(&text);
    let paths_ignore =
        FrontendMatcher::new(&clean_config_section(sections.get("IndexSearch.paths.ignore")))?;
    let paths_live =
        FrontendMatcher::new(&clean_config_section(sections.get("IndexSearch.paths.live")))?;
    let files_ignore =
        FrontendMatcher::new(&clean_config_section(sections.get("IndexSearch.files.ignore")))?;
    let mut files_include = clean_config_section(sections.get("IndexSearch.files.include"));
    if files_include.is_empty() {
        files_include.push("*".to_string());
    }
    let files_include = FrontendMatcher::new(&files_include)?;
    Ok(FrontendProjectConfig {
        paths_ignore,
        paths_live,
        files_ignore,
        files_include,
    })
}

#[derive(Default)]
struct FrontendProjectConfig {
    paths_ignore: FrontendMatcher,
    paths_live: FrontendMatcher,
    files_ignore: FrontendMatcher,
    files_include: FrontendMatcher,
}

#[derive(Default)]
struct FrontendMatcher {
    set: Option<GlobSet>,
}

impl FrontendMatcher {
    fn new(patterns: &[String]) -> Result<Self, String> {
        if patterns.is_empty() {
            return Ok(Self::default());
        }
        let mut builder = GlobSetBuilder::new();
        for pattern in patterns {
            add_frontend_glob_pattern(&mut builder, pattern)?;
        }
        Ok(Self {
            set: Some(builder.build().map_err(|err| err.to_string())?),
        })
    }

    fn is_match(&self, rel: &str) -> bool {
        self.set.as_ref().is_some_and(|set| set.is_match(rel))
    }
}

fn explicit_path_requires_external_fallback(root: &Path, path: &Path) -> bool {
    let Ok(cfg) = load_frontend_project_config(root) else {
        return false;
    };
    let root = normalized_existing_path(root);
    let path = normalized_existing_path(path);
    let Ok(rel_path) = path.strip_prefix(&root) else {
        return false;
    };
    let rel = normalize_rel_path_for_match(rel_path);
    if rel.is_empty() {
        return false;
    }
    rel_requires_external_fallback(&cfg, &rel, path.is_file())
}

fn rel_requires_external_fallback(cfg: &FrontendProjectConfig, rel: &str, is_file: bool) -> bool {
    cfg.paths_ignore.is_match(rel)
        || cfg.paths_live.is_match(rel)
        || (is_file && (cfg.files_ignore.is_match(rel) || !cfg.files_include.is_match(rel)))
}

fn normalize_rel_path_for_match(path: &Path) -> String {
    let mut rel = path.to_string_lossy().replace('\\', "/");
    while let Some(stripped) = rel.strip_prefix("./") {
        rel = stripped.to_string();
    }
    rel.trim_end_matches('/').to_string()
}

fn add_frontend_glob_pattern(builder: &mut GlobSetBuilder, raw: &str) -> Result<(), String> {
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
                .build()
                .map_err(|err| err.to_string())?,
        );
    }
    Ok(())
}

fn parse_config_sections(text: &str) -> std::collections::BTreeMap<String, Vec<String>> {
    let mut sections = std::collections::BTreeMap::<String, Vec<String>>::new();
    let mut current = String::new();
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') || trimmed.starts_with(';') {
            continue;
        }
        if trimmed.starts_with('[') && trimmed.ends_with(']') {
            current = trimmed[1..trimmed.len() - 1].trim().to_string();
            sections.entry(current.clone()).or_default();
        } else if !current.is_empty() {
            sections
                .entry(current.clone())
                .or_default()
                .push(trimmed.to_string());
        }
    }
    sections
}

fn clean_config_section(section: Option<&Vec<String>>) -> Vec<String> {
    section
        .into_iter()
        .flat_map(|lines| lines.iter())
        .filter_map(|line| {
            let line = line.split('#').next().unwrap_or("").trim();
            (!line.is_empty()).then(|| line.to_string())
        })
        .collect()
}

fn run_search_group(
    group_args: &[String],
    mut root: PathBuf,
    profile: &mut FrontendProfile,
) -> Result<i32, String> {
    let daemon_args = search_daemon_args(group_args);
    let record;
    let ensure_timer = Instant::now();
    (root, record) = ensure_project_service(&root, profile)?;
    profile.record("frontend_ensure_project_service", ensure_timer);
    request_daemon(&record, &daemon_args, profile).map_err(|err| {
        stop_process(record.pid);
        let _ = fs::remove_file(record_path(&root));
        format!(
            "project service request failed for {}: {err}",
            display_path(&root)
        )
    })
}

fn search_daemon_args(args: &[String]) -> Vec<String> {
    let resolved_color = if stdout_supports_color() {
        "always"
    } else {
        "never"
    };
    let decorated = stdout_supports_decoration();
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
            if decorated {
                "--heading"
            } else {
                "--no-heading"
            }
            .to_string(),
        );
    }
    if !saw_line_number {
        defaults.push(
            if decorated {
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

#[derive(Clone)]
struct SearchPathArg {
    arg_index: Option<usize>,
    abs: PathBuf,
}

struct SearchRun {
    kind: SearchRunKind,
    path_indexes: Vec<Option<usize>>,
    path_ordinals: Vec<usize>,
}

enum SearchRunKind {
    Indexed(PathBuf),
    External,
}

enum MissingProject {
    Single,
    Path(PathBuf),
}

fn search_path_args(args: &[String]) -> Option<Vec<SearchPathArg>> {
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
        if option_takes_value(arg) {
            i += 2;
            continue;
        }
        if arg.starts_with("--") {
            i += 1;
            continue;
        }
        if short_option_with_attached_value(arg) || (arg.starts_with('-') && arg.len() > 1) {
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
        return Some(vec![SearchPathArg {
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
                SearchPathArg {
                    arg_index: Some(arg_index),
                    abs,
                }
            })
            .collect(),
    )
}

fn search_runs(paths: &[SearchPathArg]) -> Result<Vec<SearchRun>, MissingProject> {
    let mut runs = Vec::new();
    let mut explicit_ordinal = 0usize;
    for path in paths {
        let path_ordinal = if path.arg_index.is_some() {
            let ordinal = explicit_ordinal;
            explicit_ordinal += 1;
            Some(ordinal)
        } else {
            None
        };
        let Some(root) = find_project_root(&path.abs) else {
            if paths.len() == 1 && path.arg_index.is_none() {
                return Err(MissingProject::Single);
            }
            return Err(MissingProject::Path(path.abs.clone()));
        };
        let use_external = path
            .arg_index
            .is_some_and(|_| explicit_path_requires_external_fallback(&root, &path.abs));
        push_search_run(
            &mut runs,
            if use_external {
                SearchRunKind::External
            } else {
                SearchRunKind::Indexed(root)
            },
            path.arg_index,
            path_ordinal,
        );
    }
    Ok(runs)
}

fn push_search_run(
    runs: &mut Vec<SearchRun>,
    kind: SearchRunKind,
    path_index: Option<usize>,
    path_ordinal: Option<usize>,
) {
    if let Some(last) = runs.last_mut()
        && search_run_kind_matches(&last.kind, &kind)
    {
        last.path_indexes.push(path_index);
        if let Some(path_ordinal) = path_ordinal {
            last.path_ordinals.push(path_ordinal);
        }
        return;
    }
    runs.push(SearchRun {
        kind,
        path_indexes: vec![path_index],
        path_ordinals: path_ordinal.into_iter().collect(),
    });
}

fn search_run_kind_matches(left: &SearchRunKind, right: &SearchRunKind) -> bool {
    match (left, right) {
        (SearchRunKind::External, SearchRunKind::External) => true,
        (SearchRunKind::Indexed(left), SearchRunKind::Indexed(right)) => {
            normalized_existing_path(left) == normalized_existing_path(right)
        }
        _ => false,
    }
}

fn search_args_for_group(args: &[String], keep_path_indexes: &[Option<usize>]) -> Vec<String> {
    let explicit_indexes: Vec<usize> = keep_path_indexes.iter().filter_map(|idx| *idx).collect();
    if explicit_indexes.is_empty() {
        return args.to_vec();
    }
    let all_path_indexes = search_path_args(args)
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

fn grep_args_for_path_ordinals(args: &[String], keep_path_ordinals: &[usize]) -> Vec<String> {
    let path_indexes = grep_path_arg_indexes(args);
    if path_indexes.is_empty() {
        return args.to_vec();
    }
    let keep_indexes = path_indexes
        .iter()
        .enumerate()
        .filter_map(|(ordinal, index)| keep_path_ordinals.contains(&ordinal).then_some(*index))
        .collect::<Vec<_>>();
    args.iter()
        .enumerate()
        .filter_map(|(idx, arg)| {
            if path_indexes.contains(&idx) && !keep_indexes.contains(&idx) {
                None
            } else {
                Some(arg.clone())
            }
        })
        .collect()
}

fn grep_path_arg_indexes(args: &[String]) -> Vec<usize> {
    let mut path_indexes = Vec::new();
    let mut saw_pattern = false;
    let mut positional_only = false;
    let mut i = 0usize;
    while i < args.len() {
        let arg = &args[i];
        if positional_only {
            if saw_pattern {
                path_indexes.push(i);
            } else {
                saw_pattern = true;
            }
            i += 1;
            continue;
        }
        if arg == "--" {
            positional_only = true;
            i += 1;
            continue;
        }
        if let Some(long) = arg.strip_prefix("--") {
            if long.is_empty() {
                positional_only = true;
                i += 1;
                continue;
            }
            let (name, has_inline_value) = long
                .split_once('=')
                .map(|(name, _)| (name, true))
                .unwrap_or((long, false));
            if matches!(name, "regexp" | "file") {
                saw_pattern = true;
            }
            if grep_long_option_takes_value(name) && !has_inline_value {
                i += 2;
            } else {
                i += 1;
            }
            continue;
        }
        if arg.starts_with('-') && arg != "-" {
            let (takes_value, adds_pattern) = grep_short_option_scan(arg);
            if adds_pattern {
                saw_pattern = true;
            }
            i += if takes_value && grep_short_value_is_separate(arg) {
                2
            } else {
                1
            };
            continue;
        }
        if saw_pattern {
            path_indexes.push(i);
        } else {
            saw_pattern = true;
        }
        i += 1;
    }
    path_indexes
}

fn grep_long_option_takes_value(name: &str) -> bool {
    matches!(
        name,
        "regexp"
            | "file"
            | "after-context"
            | "before-context"
            | "context"
            | "max-count"
            | "include"
            | "exclude"
            | "exclude-dir"
            | "directories"
            | "binary-files"
    )
}

fn grep_short_option_scan(arg: &str) -> (bool, bool) {
    for ch in arg.chars().skip(1) {
        match ch {
            'e' | 'f' => return (true, true),
            'A' | 'B' | 'C' | 'm' | 'd' | 'D' => return (true, false),
            _ => {}
        }
    }
    (false, false)
}

fn grep_short_value_is_separate(arg: &str) -> bool {
    let mut chars = arg.chars();
    let _dash = chars.next();
    let Some(flag) = chars.next() else {
        return false;
    };
    matches!(flag, 'e' | 'f' | 'A' | 'B' | 'C' | 'm' | 'd' | 'D') && chars.next().is_none()
}

fn run_indexed_search_group(
    group_args: &[String],
    root: PathBuf,
    profile: &mut FrontendProfile,
) -> Result<i32, String> {
    if group_args
        .iter()
        .any(|arg| matches!(arg.as_str(), "--auto-update" | "--no-auto-index"))
    {
        return run_backend_search(group_args);
    }
    run_search_group(group_args, root, profile)
}

fn combine_exit_codes(current: i32, next: i32) -> i32 {
    if current > 1 || next > 1 {
        current.max(next)
    } else if current == 0 || next == 0 {
        0
    } else {
        1
    }
}

fn option_takes_value(arg: &str) -> bool {
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

fn short_option_with_attached_value(arg: &str) -> bool {
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

fn find_project_root(start: &Path) -> Option<PathBuf> {
    start
        .ancestors()
        .find(|ancestor| project_marker_exists(ancestor))
        .map(Path::to_path_buf)
}

fn handle_missing_project(args: &[String]) -> Result<i32, String> {
    let cwd = env::current_dir().map_err(|err| err.to_string())?;
    if agent_auto_project_mode() {
        eprintln!(
            "is: no IndexSearch project found above {}; auto-create failed",
            display_path(&cwd)
        );
        return Ok(2);
    }
    if io::stdin().is_terminal() && io::stderr().is_terminal() {
        eprint!(
            "is: no IndexSearch project found above {}. Create one here? [Y/n] ",
            display_path(&cwd)
        );
        let _ = io::stderr().flush();
        let mut answer = String::new();
        if io::stdin().read_line(&mut answer).is_ok() {
            match answer.trim() {
                "" | "y" | "Y" | "yes" | "YES" => {
                    let mut profile = FrontendProfile::default();
                    let (root, _) = ensure_project_service(&cwd, &mut profile)?;
                    if index_path(&root).is_file() {
                        return run(args);
                    }
                    return Ok(2);
                }
                "n" | "N" | "no" | "NO" => return Ok(1),
                _ => {}
            }
        }
    }
    eprintln!(
        "is: no IndexSearch project found; run `istool index .` at the project root"
    );
    Ok(2)
}

fn agent_auto_project_mode() -> bool {
    if env::var_os("INDEXSEARCH_NO_AGENT_AUTO_PROJECT").is_some() {
        return false;
    }
    env::var_os("INDEXSEARCH_AGENT_AUTO_PROJECT").is_some()
        || !(io::stdin().is_terminal() && io::stderr().is_terminal())
}

fn agent_auto_project_root(paths: &[SearchPathArg]) -> Result<PathBuf, String> {
    let cwd = env::current_dir().map_err(|err| err.to_string())?;
    if paths.len() == 1 {
        let path = &paths[0];
        if path.arg_index.is_some() && !path.abs.starts_with(&cwd) {
            if path.abs.is_file() {
                return Ok(path
                    .abs
                    .parent()
                    .map(Path::to_path_buf)
                    .unwrap_or_else(|| path.abs.clone()));
            }
            return Ok(path.abs.clone());
        }
    }
    Ok(cwd)
}

fn ensure_project_service(
    root: &Path,
    profile: &mut FrontendProfile,
) -> Result<(PathBuf, DaemonRecord), String> {
    let maintenance_timer = Instant::now();
    wait_for_project_maintenance(root)?;
    profile.record("frontend_wait_project_maintenance", maintenance_timer);

    let ready_timer = Instant::now();
    if let Some(record) = ready_project_record(root) {
        profile.record("frontend_ready_project_record", ready_timer);
        return Ok((root.to_path_buf(), record));
    }
    profile.record("frontend_ready_project_record", ready_timer);

    let stop_timer = Instant::now();
    stop_project_service_for_root(root);
    profile.record("frontend_stop_stale_project_service", stop_timer);

    let start_timer = Instant::now();
    start_project_service(root)?;
    profile.record("frontend_start_project_service", start_timer);
    let wait_timer = Instant::now();
    let ready_record = wait_for_ready_project_record(root, START_TIMEOUT);
    profile.record("frontend_wait_project_record", wait_timer);
    ready_record
        .map(|record| (root.to_path_buf(), record))
        .ok_or_else(|| format!("project service did not become ready for {}", display_path(root)))
}

fn ready_project_record(root: &Path) -> Option<DaemonRecord> {
    if !index_path(root).is_file() {
        return None;
    }
    read_valid_record(root)
}

fn wait_for_project_maintenance(root: &Path) -> Result<(), String> {
    let start = Instant::now();
    while project_maintenance_active(root) {
        if start.elapsed() >= MAINTENANCE_WAIT_TIMEOUT {
            return Err(format!(
                "project maintenance is still running for {}",
                display_path(root)
            ));
        }
        std::thread::sleep(MAINTENANCE_WAIT_INTERVAL);
    }
    Ok(())
}

fn project_maintenance_active(root: &Path) -> bool {
    let path = maintenance_path(root);
    let Ok(text) = fs::read_to_string(&path) else {
        return false;
    };
    let Some(pid) = maintenance_pid_from_text(&text) else {
        let _ = fs::remove_file(path);
        return false;
    };
    if process_alive(pid) {
        return true;
    }
    let _ = fs::remove_file(path);
    false
}

fn maintenance_pid_from_text(text: &str) -> Option<u32> {
    text.lines()
        .find_map(|line| line.strip_prefix("pid="))
        .and_then(|value| value.parse().ok())
}

fn wait_for_ready_project_record(root: &Path, timeout: Duration) -> Option<DaemonRecord> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(record) = ready_project_record(root) {
            return Some(record);
        }
        if Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn stop_project_service_for_root(root: &Path) {
    let requested_root = normalized_existing_path(root);
    let registry = project_registry_dir();
    let Ok(entries) = fs::read_dir(registry) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.extension().is_some_and(|ext| ext == "project") {
            continue;
        }
        let Ok(record) = read_project_record(&path) else {
            let _ = fs::remove_file(path);
            continue;
        };
        let record_root = normalized_existing_path(&record.root);
        if record_root == requested_root {
            stop_process(record.pid);
            let _ = fs::remove_file(path);
            let _ = fs::remove_file(record_path(&record.root));
        }
    }
}

fn stderr_supports_progress() -> bool {
    io::stderr().is_terminal()
        && env::var("TERM").map(|term| term != "dumb").unwrap_or(true)
        && env::var_os("INDEXSEARCH_NO_PROGRESS").is_none()
}

fn start_project_service(root: &Path) -> Result<(), String> {
    let backend = backend_path()?;
    let show_progress = stderr_supports_progress();
    let mut command = if hide_project_service_startup_parent(show_progress) {
        hidden_background_command(backend)
    } else {
        Command::new(backend)
    };
    command.arg("search-daemon").arg("--detach");
    command.arg(root).stdin(Stdio::null()).stdout(Stdio::null());
    if show_progress {
        command.stderr(Stdio::inherit());
    } else {
        command.env("INDEXSEARCH_NO_PROGRESS", "1");
        command.stderr(Stdio::null());
    }
    let status = command.status().map_err(|err| err.to_string())?;
    if status.success() {
        Ok(())
    } else {
        Err(format!(
            "failed to start project service for {}",
            display_path(root)
        ))
    }
}

fn hide_project_service_startup_parent(show_progress: bool) -> bool {
    !show_progress
}

fn hidden_background_command(program: impl AsRef<std::ffi::OsStr>) -> Command {
    let mut command = Command::new(program);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        use windows_sys::Win32::System::Threading::CREATE_NO_WINDOW;

        command.creation_flags(CREATE_NO_WINDOW);
    }
    command
}

fn read_valid_record(root: &Path) -> Option<DaemonRecord> {
    let record = read_record(&record_path(root)).ok()?;
    if !record_matches(&record) {
        let _ = fs::remove_file(record_path(root));
        return None;
    }
    Some(record)
}

fn record_matches(record: &DaemonRecord) -> bool {
    let Ok(index_meta) = fs::metadata(index_path(&record.root)) else {
        return false;
    };
    if index_meta.len() != record.index_size || mtime_ns(&index_meta) != record.index_mtime {
        return false;
    }
    backend_candidate_paths().into_iter().any(|backend| {
        let Ok(backend_meta) = fs::metadata(&backend) else {
            return false;
        };
        same_fileish(&backend, &record.exe_path)
            && backend_meta.len() == record.exe_size
            && mtime_ns(&backend_meta) == record.exe_mtime
    })
}

fn same_fileish(left: &Path, right: &Path) -> bool {
    left == right || fs::canonicalize(left).ok() == fs::canonicalize(right).ok()
}

fn request_daemon(
    record: &DaemonRecord,
    args: &[String],
    profile: &mut FrontendProfile,
) -> io::Result<i32> {
    let rpc_timer = Instant::now();
    #[cfg(unix)]
    if let Some(socket_path) = record.socket_path.as_ref() {
        let connect_timer = Instant::now();
        let mut stream = UnixStream::connect(socket_path)?;
        profile.record("frontend_daemon_connect", connect_timer);
        let write_timer = Instant::now();
        write_daemon_request(&mut stream, record, args)?;
        send_stdout_fd(&stream)?;
        profile.record("frontend_daemon_write_request", write_timer);
        let read_timer = Instant::now();
        let code = read_daemon_response(&mut stream)?;
        profile.record("frontend_daemon_read_response", read_timer);
        profile.record("frontend_daemon_rpc_total", rpc_timer);
        return Ok(code);
    }

    let addr = SocketAddr::from(([127, 0, 0, 1], record.port));
    let connect_timer = Instant::now();
    let mut stream = TcpStream::connect_timeout(&addr, CONNECT_TIMEOUT)?;
    stream.set_nodelay(true)?;
    profile.record("frontend_daemon_connect", connect_timer);
    let write_timer = Instant::now();
    write_daemon_request(&mut stream, record, args)?;
    send_stdout_handle(&mut stream, record)?;
    profile.record("frontend_daemon_write_request", write_timer);
    let read_timer = Instant::now();
    let code = read_daemon_response(&mut stream)?;
    profile.record("frontend_daemon_read_response", read_timer);
    profile.record("frontend_daemon_rpc_total", rpc_timer);
    Ok(code)
}

fn write_daemon_request(
    mut stream: &mut impl Write,
    record: &DaemonRecord,
    args: &[String],
) -> io::Result<()> {
    stream.write_all(REQUEST_MAGIC)?;
    write_frame(&mut stream, record.token.as_bytes())?;
    let cwd = env::current_dir()
        .ok()
        .map(|path| path.to_string_lossy().into_owned())
        .unwrap_or_default();
    write_frame(&mut stream, cwd.as_bytes())?;
    write_u32(&mut stream, args.len() as u32)?;
    for arg in args {
        write_frame(&mut stream, arg.as_bytes())?;
    }
    stream.flush()?;
    Ok(())
}

fn read_daemon_response(mut stream: &mut impl Read) -> io::Result<i32> {
    let mut magic = [0u8; 8];
    stream.read_exact(&mut magic)?;
    if &magic != RESPONSE_MAGIC {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid daemon response",
        ));
    }
    let stdout = io::stdout();
    let mut out = io::BufWriter::new(stdout.lock());
    let stderr = io::stderr();
    let mut err = io::BufWriter::new(stderr.lock());
    loop {
        let mut tag = [0u8; 1];
        stream.read_exact(&mut tag)?;
        match tag[0] {
            STDOUT_FRAME => {
                if let Err(err) = copy_frame(&mut stream, &mut out) {
                    if is_broken_pipe(&err) {
                        return Ok(0);
                    }
                    return Err(err);
                }
            }
            STDERR_FRAME => copy_frame(&mut stream, &mut err)?,
            DONE_FRAME => {
                let code = read_u32(&mut stream)? as i32;
                if let Err(err) = out.flush() {
                    if is_broken_pipe(&err) {
                        return Ok(0);
                    }
                    return Err(err);
                }
                err.flush()?;
                return Ok(code);
            }
            _ => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "invalid daemon response frame",
                ));
            }
        }
    }
}

fn is_broken_pipe(err: &io::Error) -> bool {
    matches!(err.kind(), io::ErrorKind::BrokenPipe)
}

#[cfg(unix)]
fn send_stdout_fd(stream: &UnixStream) -> io::Result<()> {
    use std::mem;
    use std::ptr;

    let fd = io::stdout().as_raw_fd();
    let mut byte = [0u8; 1];
    let mut iov = libc::iovec {
        iov_base: byte.as_mut_ptr().cast(),
        iov_len: byte.len(),
    };
    let fd_size = mem::size_of_val(&fd);
    let mut control = vec![0u8; unsafe { libc::CMSG_SPACE(fd_size as _) } as usize];
    let mut msg: libc::msghdr = unsafe { mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = control.as_mut_ptr().cast();
    msg.msg_controllen = control.len() as _;
    unsafe {
        let cmsg = libc::CMSG_FIRSTHDR(&msg);
        if cmsg.is_null() {
            return Err(io::Error::other("missing fd control header"));
        }
        (*cmsg).cmsg_level = libc::SOL_SOCKET;
        (*cmsg).cmsg_type = libc::SCM_RIGHTS;
        (*cmsg).cmsg_len = libc::CMSG_LEN(fd_size as _) as _;
        ptr::copy_nonoverlapping(
            &fd as *const _ as *const u8,
            libc::CMSG_DATA(cmsg).cast::<u8>(),
            fd_size,
        );
        msg.msg_controllen = (*cmsg).cmsg_len as _;
        if libc::sendmsg(stream.as_raw_fd(), &msg, 0) == -1 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

#[cfg(windows)]
fn send_stdout_handle(stream: &mut TcpStream, _record: &DaemonRecord) -> io::Result<()> {
    stream.write_all(&0u64.to_le_bytes())
}

#[cfg(not(windows))]
fn send_stdout_handle(_stream: &mut TcpStream, _record: &DaemonRecord) -> io::Result<()> {
    Ok(())
}

fn run_backend_search(args: &[String]) -> Result<i32, String> {
    let mut owned = Vec::with_capacity(args.len() + 1);
    owned.push("search".to_string());
    owned.extend(args.iter().cloned());
    run_backend_owned(owned.iter().map(String::as_str))
}

fn run_backend_search_help() -> Result<i32, String> {
    let backend = backend_path()?;
    let mut command = Command::new(backend);
    command
        .args(["search", "--help"])
        .env("INDEXSEARCH_FRONTEND_HELP_NAME", frontend_command_name());
    exec_or_status(command)
}

fn frontend_command_name() -> String {
    env::args()
        .next()
        .and_then(|arg| {
            Path::new(&arg)
                .file_stem()
                .and_then(|stem| stem.to_str())
                .map(str::to_string)
        })
        .or_else(|| {
            env::current_exe()
                .ok()
                .and_then(|path| {
                    path.file_stem()
                        .and_then(|stem| stem.to_str())
                        .map(str::to_string)
                })
        })
        .unwrap_or_else(|| "is".to_string())
}

fn run_backend_owned<'a>(args: impl IntoIterator<Item = &'a str>) -> Result<i32, String> {
    let backend = backend_path()?;
    let mut command = Command::new(backend);
    command.args(args);
    exec_or_status(command)
}

#[cfg(unix)]
fn exec_or_status(mut command: Command) -> Result<i32, String> {
    use std::os::unix::process::CommandExt;
    let err = command.exec();
    Err(err.to_string())
}

#[cfg(not(unix))]
fn exec_or_status(mut command: Command) -> Result<i32, String> {
    let status = command.status().map_err(|err| err.to_string())?;
    Ok(status.code().unwrap_or(1))
}

fn backend_path() -> Result<PathBuf, String> {
    let exe = env::current_exe().map_err(|err| err.to_string())?;
    for sibling in backend_candidate_paths() {
        if sibling.is_file() && fs::canonicalize(&sibling).ok() != fs::canonicalize(&exe).ok() {
            return Ok(sibling);
        }
    }
    if exe
        .file_stem()
        .and_then(|stem| stem.to_str())
        .is_some_and(|stem| stem == "is-daemon" || stem == "istool")
    {
        return Ok(exe);
    }
    Err(format!(
        "cannot find `{}` next to {}",
        executable_name("is-daemon"),
        display_path(&exe)
    ))
}

fn backend_candidate_paths() -> Vec<PathBuf> {
    let Ok(exe) = env::current_exe() else {
        return Vec::new();
    };
    let Some(dir) = exe.parent() else {
        return Vec::new();
    };
    let versioned_daemon = format!("is-daemon-{}", display_version());
    [versioned_daemon.as_str(), "is-daemon", "istool"]
        .into_iter()
        .map(|name| dir.join(executable_name(name)))
        .collect()
}

fn executable_name(stem: &str) -> String {
    if cfg!(windows) {
        format!("{stem}.exe")
    } else {
        stem.to_string()
    }
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

fn index_path(root: &Path) -> PathBuf {
    root.join(INDEX_DIR).join(INDEX_FILE)
}

fn maintenance_path(root: &Path) -> PathBuf {
    root.join(INDEX_DIR).join(MAINTENANCE_FILE)
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

fn record_path(root: &Path) -> PathBuf {
    root.join(INDEX_DIR).join(SEARCH_DAEMON_FILE)
}

fn project_registry_dir() -> PathBuf {
    env::var_os("HOME")
        .or_else(|| env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".indexsearch")
        .join(PROJECTS_DIR)
}

fn normalized_existing_path(path: &Path) -> PathBuf {
    fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

#[derive(Default)]
struct DaemonRecord {
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

fn read_record(path: &Path) -> Result<DaemonRecord, String> {
    let text = fs::read_to_string(path).map_err(|err| err.to_string())?;
    let mut record = DaemonRecord::default();
    for line in text.lines() {
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        match key {
            "pid" => record.pid = value.parse().map_err(|_| "invalid daemon pid")?,
            "port" => record.port = value.parse().map_err(|_| "invalid daemon port")?,
            "socket_path" => record.socket_path = Some(PathBuf::from(value)),
            "token" => record.token = value.to_string(),
            "root" => record.root = PathBuf::from(value),
            "exe_path" => record.exe_path = PathBuf::from(value),
            "exe_size" => record.exe_size = value.parse().map_err(|_| "invalid exe_size")?,
            "exe_mtime" => record.exe_mtime = value.parse().map_err(|_| "invalid exe_mtime")?,
            "index_size" => record.index_size = value.parse().map_err(|_| "invalid index_size")?,
            "index_mtime" => {
                record.index_mtime = value.parse().map_err(|_| "invalid index_mtime")?
            }
            _ => {}
        }
    }
    if record.pid == 0
        || (record.port == 0 && record.socket_path.is_none())
        || record.token.is_empty()
        || record.root.as_os_str().is_empty()
        || record.exe_path.as_os_str().is_empty()
    {
        return Err("invalid daemon record".to_string());
    }
    Ok(record)
}

#[derive(Default)]
struct ProjectRecord {
    pid: u32,
    root: PathBuf,
}

fn read_project_record(path: &Path) -> Result<ProjectRecord, String> {
    let text = fs::read_to_string(path).map_err(|err| err.to_string())?;
    let mut record = ProjectRecord::default();
    for line in text.lines() {
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        match key {
            "pid" => record.pid = value.parse().unwrap_or(0),
            "root" => record.root = PathBuf::from(value),
            _ => {}
        }
    }
    if record.pid == 0 || record.root.as_os_str().is_empty() {
        return Err("invalid project record".to_string());
    }
    Ok(record)
}

fn stop_process(pid: u32) {
    #[cfg(unix)]
    {
        let _ = Command::new("kill").arg(pid.to_string()).status();
    }
    #[cfg(windows)]
    {
        let _ = hidden_background_command("taskkill")
            .args(["/PID", &pid.to_string(), "/T", "/F"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

fn write_frame(writer: &mut impl Write, bytes: &[u8]) -> io::Result<()> {
    writer.write_all(&(bytes.len() as u64).to_le_bytes())?;
    writer.write_all(bytes)
}

fn copy_frame(reader: &mut impl Read, writer: &mut impl Write) -> io::Result<()> {
    let mut remaining = read_u64(reader)?;
    let mut buffer = vec![0u8; FRAME_COPY_BUFFER_SIZE];
    while remaining != 0 {
        let take = remaining.min(buffer.len() as u64) as usize;
        reader.read_exact(&mut buffer[..take])?;
        writer.write_all(&buffer[..take])?;
        remaining -= take as u64;
    }
    Ok(())
}

fn write_u32(writer: &mut impl Write, value: u32) -> io::Result<()> {
    writer.write_all(&value.to_le_bytes())
}

fn read_u32(reader: &mut impl Read) -> io::Result<u32> {
    let mut bytes = [0u8; 4];
    reader.read_exact(&mut bytes)?;
    Ok(u32::from_le_bytes(bytes))
}

fn read_u64(reader: &mut impl Read) -> io::Result<u64> {
    let mut bytes = [0u8; 8];
    reader.read_exact(&mut bytes)?;
    Ok(u64::from_le_bytes(bytes))
}

fn mtime_ns(meta: &fs::Metadata) -> i64 {
    meta.modified()
        .ok()
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|duration| duration.as_nanos() as i64)
        .unwrap_or(0)
}

fn stdout_supports_decoration() -> bool {
    io::stdout().is_terminal()
}

fn stdout_supports_color() -> bool {
    if env::var_os("NO_COLOR").is_some() {
        return false;
    }
    if env::var_os("CLICOLOR_FORCE").is_some() {
        return true;
    }
    stdout_supports_decoration() && stdout_supports_ansi()
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
        fn GetStdHandle(nStdHandle: u32) -> *mut std::ffi::c_void;
        fn GetConsoleMode(hConsoleHandle: *mut std::ffi::c_void, lpMode: *mut u32) -> i32;
        fn SetConsoleMode(hConsoleHandle: *mut std::ffi::c_void, dwMode: u32) -> i32;
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
        hidden_background_command("powershell")
            .args([
                "-NoProfile",
                "-Command",
                &format!("if (Get-Process -Id {pid} -ErrorAction SilentlyContinue) {{ exit 0 }} else {{ exit 1 }}"),
            ])
            .status()
            .is_ok_and(|status| status.success())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn project_service_startup_process_is_visible_when_showing_progress() {
        assert!(!hide_project_service_startup_parent(true));
        assert!(hide_project_service_startup_parent(false));
    }

    #[test]
    fn broken_pipe_is_quiet_output_termination() {
        let err = io::Error::new(io::ErrorKind::BrokenPipe, "closed");
        assert!(is_broken_pipe(&err));
    }

    #[test]
    fn displayed_version_includes_build_metadata() {
        let version = display_version();
        assert!(version.starts_with(concat!(env!("CARGO_PKG_VERSION"), "+build.")));
    }

    #[test]
    fn piped_stdin_without_paths_uses_ripgrep_fallback() {
        let args = vec!["-n".to_string(), "Needle".to_string()];
        assert!(should_fallback_to_ripgrep_stdin(&args, true));
    }

    #[test]
    fn piped_stdin_with_explicit_path_stays_indexed() {
        let args = vec!["-n".to_string(), "Needle".to_string(), ".".to_string()];
        assert!(!should_fallback_to_ripgrep_stdin(&args, true));
    }

    #[test]
    fn non_pipe_without_paths_stays_indexed() {
        let args = vec!["-n".to_string(), "Needle".to_string()];
        assert!(!should_fallback_to_ripgrep_stdin(&args, false));
    }

    #[test]
    fn piped_stdin_explicit_dash_path_uses_ripgrep_fallback() {
        let args = vec!["-n".to_string(), "Needle".to_string(), "-".to_string()];
        assert!(should_fallback_to_ripgrep_stdin(&args, true));
    }

    #[test]
    fn piped_files_mode_stays_indexed() {
        let args = vec!["--files".to_string(), "-g".to_string(), "*.rs".to_string()];
        assert!(!should_fallback_to_ripgrep_stdin(&args, true));
    }

    #[test]
    fn ripgrep_stdin_args_strip_indexsearch_management_flags() {
        let args = vec![
            "--profile".to_string(),
            "--auto-update".to_string(),
            "--no-daemon".to_string(),
            "-n".to_string(),
            "Needle".to_string(),
        ];
        assert_eq!(
            ripgrep_stdin_args(&args),
            vec!["-n".to_string(), "Needle".to_string()]
        );
    }

    #[test]
    fn piped_stdin_with_inline_regexp_uses_ripgrep_fallback() {
        let args = vec!["--regexp=Needle".to_string()];
        assert!(should_fallback_to_ripgrep_stdin(&args, true));

        let args = vec!["-eNeedle".to_string()];
        assert!(should_fallback_to_ripgrep_stdin(&args, true));
    }

    #[test]
    fn live_path_target_falls_back_to_ripgrep() {
        let cfg = FrontendProjectConfig {
            paths_live: FrontendMatcher::new(&["Game/Saved/Logs/".to_string()]).unwrap(),
            ..Default::default()
        };
        assert!(rel_requires_external_fallback(
            &cfg,
            "Game/Saved/Logs",
            false
        ));
    }

    #[test]
    fn project_root_does_not_fallback_just_because_it_contains_live_paths() {
        let cfg = FrontendProjectConfig {
            paths_live: FrontendMatcher::new(&["Game/Saved/Logs/".to_string()]).unwrap(),
            ..Default::default()
        };
        assert!(!rel_requires_external_fallback(&cfg, "", false));
    }

    #[test]
    fn ignored_directory_target_falls_back_to_external_tool() {
        let cfg = FrontendProjectConfig {
            paths_ignore: FrontendMatcher::new(&["Saved/".to_string()]).unwrap(),
            ..Default::default()
        };
        assert!(rel_requires_external_fallback(&cfg, "Game/Saved", false));
        assert!(rel_requires_external_fallback(
            &cfg,
            "Game/Saved/Logs",
            false
        ));
        assert!(!rel_requires_external_fallback(&cfg, "Game", false));
    }

    #[test]
    fn live_suffix_pattern_matches_nested_target() {
        let cfg = FrontendProjectConfig {
            paths_live: FrontendMatcher::new(&["Saved/Logs/".to_string()]).unwrap(),
            ..Default::default()
        };
        assert!(rel_requires_external_fallback(
            &cfg,
            "Game/Saved/Logs",
            false
        ));
    }

    #[test]
    fn explicit_file_not_in_indexed_file_set_falls_back() {
        let cfg = FrontendProjectConfig {
            files_ignore: FrontendMatcher::new(&["*.png".to_string()]).unwrap(),
            files_include: FrontendMatcher::new(&["*.rs".to_string()]).unwrap(),
            ..Default::default()
        };
        assert!(rel_requires_external_fallback(&cfg, "src/logo.png", true));
        assert!(rel_requires_external_fallback(&cfg, "src/readme.md", true));
        assert!(!rel_requires_external_fallback(&cfg, "src/main.rs", true));
        assert!(!rel_requires_external_fallback(&cfg, "src", false));
    }

    #[test]
    fn grep_args_for_path_ordinals_keeps_only_selected_paths() {
        let args = vec![
            "-r".to_string(),
            "-n".to_string(),
            "--include=*.cpp".to_string(),
            "Needle".to_string(),
            "Source".to_string(),
            "Saved/Logs".to_string(),
            "Plugins".to_string(),
        ];
        assert_eq!(
            grep_path_arg_indexes(&args),
            vec![4usize, 5usize, 6usize]
        );
        assert_eq!(
            grep_args_for_path_ordinals(&args, &[1]),
            vec![
                "-r".to_string(),
                "-n".to_string(),
                "--include=*.cpp".to_string(),
                "Needle".to_string(),
                "Saved/Logs".to_string(),
            ]
        );
    }

    #[test]
    fn search_args_for_group_keeps_selected_search_paths() {
        let args = vec![
            "-n".to_string(),
            "Needle".to_string(),
            "Source".to_string(),
            "Saved/Logs".to_string(),
            "Plugins".to_string(),
        ];
        assert_eq!(
            search_args_for_group(&args, &[Some(3)]),
            vec![
                "-n".to_string(),
                "Needle".to_string(),
                "Saved/Logs".to_string(),
            ]
        );
    }
}
