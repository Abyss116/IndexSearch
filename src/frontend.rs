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

const INDEX_DIR: &str = ".indexsearch";
const INDEX_FILE: &str = "index.bin";
const PROJECT_CONFIG_FILE: &str = "is-project-config.txt";
const SEARCH_DAEMON_FILE: &str = "search-daemon.txt";
const PROJECT_LOG_FILE: &str = "project.log";
const MAINTENANCE_FILE: &str = "maintenance.txt";
const PROJECTS_DIR: &str = "projects";
const SEARCH_DAEMON_PROTOCOL: u32 = 2;
const GREP_FRONTEND_ARG: &str = "--__indexsearch-grep-frontend";
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
    if stdin_has_searchable_stream() {
        return run_system_grep(args);
    }

    let total_timer = Instant::now();
    let mut profile = FrontendProfile::new(args);
    let path_timer = Instant::now();
    let search_paths = grep_search_path_args(args)?;
    profile.record("frontend_resolve_search_paths", path_timer);
    let missing_project = search_paths
        .iter()
        .any(|path| find_project_root(&path.abs).is_none());
    if missing_project {
        if search_paths.len() == 1 && search_paths[0].arg_index.is_none() {
            return handle_missing_grep_project(args);
        }
        return run_system_grep(args);
    }
    let root_timer = Instant::now();
    let runs = match search_runs(&search_paths) {
        Ok(groups) => groups,
        Err(MissingProject::Single) => return handle_missing_grep_project(args),
        Err(MissingProject::Path(_)) => return run_system_grep(args),
    };
    profile.record("frontend_find_project_roots", root_timer);
    if runs.len() > 1 {
        profile.record_value("frontend_search_run_count", runs.len() as f64);
    }
    let mut final_code = 1;
    for run in runs {
        let SearchRunKind::Indexed(root) = run.kind;
        let group_args = grep_args_for_group(args, &run.path_indexes);
        let daemon_args = grep_daemon_args(&group_args);
        let code = run_search_group_with_daemon_args(
            &daemon_args,
            root,
            &mut profile,
            DaemonFailureFallback::Grep(group_args),
        )?;
        final_code = combine_exit_codes(final_code, code);
    }
    profile.record("frontend_total", total_timer);
    profile.print();
    Ok(final_code)
}

fn print_isgrep_help() {
    println!("Usage");
    println!("  isgrep [GREP_OPTIONS] PATTERN [FILE ...]");
    println!();
    println!("Description");
    println!("  grep-compatible frontend for indexed IndexSearch searches");
    println!("  Defaults to grep Basic Regex syntax, so `A\\|B` works as alternation.");
    println!("  Use `-E` for rg/RTK-style bare `A|B` alternation.");
    println!("  Unsupported grep-only semantics are handled through system grep internally.");
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

fn grep_daemon_args(args: &[String]) -> Vec<String> {
    let grep_args = grep_frontend_daemon_args(args);
    let mut out = Vec::with_capacity(args.len() + 5);
    out.push(GREP_FRONTEND_ARG.to_string());
    out.extend(search_output_default_args(&grep_args));
    out.push("--".to_string());
    out.extend(grep_args);
    out
}

fn grep_frontend_daemon_args(args: &[String]) -> Vec<String> {
    let resolved_color = if stdout_supports_color() {
        "always"
    } else {
        "never"
    };
    args.iter()
        .map(|arg| {
            for flag in ["--color=", "--colour="] {
                if let Some(value) = arg.strip_prefix(flag) {
                    return if value == "auto" {
                        format!("{flag}{resolved_color}")
                    } else {
                        arg.clone()
                    };
                }
            }
            arg.clone()
        })
        .collect()
}

fn handle_missing_grep_project(args: &[String]) -> Result<i32, String> {
    let cwd = env::current_dir().map_err(|err| err.to_string())?;
    if missing_project_should_skip_prompt()
        || !(io::stdin().is_terminal() && io::stderr().is_terminal())
    {
        return run_system_grep(args);
    }
    eprint!(
        "{}: no IndexSearch project found above {}. Create one here? [Y/n] ",
        frontend_command_name(),
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
                    let daemon_args = grep_daemon_args(args);
                    return run_search_group_with_daemon_args(
                        &daemon_args,
                        root,
                        &mut profile,
                        DaemonFailureFallback::Grep(args.to_vec()),
                    );
                }
                return Ok(2);
            }
            "n" | "N" | "no" | "NO" => return run_system_grep(args),
            _ => {}
        }
    }
    run_system_grep(args)
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
    run_with_fallback(args)
}

fn run_with_fallback(args: &[String]) -> Result<i32, String> {
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
    let missing_project = search_paths
        .iter()
        .any(|path| find_project_root(&path.abs).is_none());
    if missing_project {
        if search_paths.len() == 1 && search_paths[0].arg_index.is_none() {
            return handle_missing_project(&search_args);
        }
        return run_system_ripgrep(&ripgrep_stdin_args(&search_args));
    }
    let root_timer = Instant::now();
    let runs = match search_runs(&search_paths) {
        Ok(groups) => groups,
        Err(MissingProject::Single) => return handle_missing_project(&search_args),
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
        let SearchRunKind::Indexed(root) = run.kind;
        let group_args = search_args_for_group(&search_args, &run.path_indexes);
        let code = run_indexed_search_group(&group_args, root, &mut profile)?;
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

fn run_search_group(
    group_args: &[String],
    root: PathBuf,
    profile: &mut FrontendProfile,
) -> Result<i32, String> {
    let daemon_args = search_daemon_args(group_args);
    run_search_group_with_daemon_args(
        &daemon_args,
        root,
        profile,
        DaemonFailureFallback::Ripgrep(ripgrep_stdin_args(group_args)),
    )
}

fn run_search_group_with_daemon_args(
    daemon_args: &[String],
    root: PathBuf,
    profile: &mut FrontendProfile,
    fallback: DaemonFailureFallback,
) -> Result<i32, String> {
    let ensure_timer = Instant::now();
    let (root, record) = ensure_project_service(&root, profile)?;
    profile.record("frontend_ensure_project_service", ensure_timer);
    match request_daemon(&record, daemon_args, profile) {
        Ok(code) => Ok(code),
        Err(err) if is_recoverable_daemon_request_error(&err) => {
            recover_bad_project_service(&root, &record, &err);
            let retry_timer = Instant::now();
            let (_, retry_record) = ensure_project_service(&root, profile)?;
            profile.record("frontend_recover_project_service", retry_timer);
            match request_daemon(&retry_record, daemon_args, profile) {
                Ok(code) => Ok(code),
                Err(retry_err) if is_recoverable_daemon_request_error(&retry_err) => {
                    recover_bad_project_service(&root, &retry_record, &retry_err);
                    append_frontend_project_log(
                        &root,
                        &format!(
                            "frontend-daemon-fallback reason={} action=external",
                            clean_log_text(&retry_err.to_string(), 240)
                        ),
                    );
                    run_daemon_failure_fallback(fallback)
                }
                Err(retry_err) => {
                    recover_bad_project_service(&root, &retry_record, &retry_err);
                    Err(format!(
                        "project service request failed for {} after recovery: {retry_err}",
                        display_path(&root)
                    ))
                }
            }
        }
        Err(err) => {
            recover_bad_project_service(&root, &record, &err);
            Err(format!(
                "project service request failed for {}: {err}",
                display_path(&root)
            ))
        }
    }
}

enum DaemonFailureFallback {
    Ripgrep(Vec<String>),
    Grep(Vec<String>),
}

fn run_daemon_failure_fallback(fallback: DaemonFailureFallback) -> Result<i32, String> {
    match fallback {
        DaemonFailureFallback::Ripgrep(args) => run_system_ripgrep(&args),
        DaemonFailureFallback::Grep(args) => run_system_grep(&args),
    }
}

fn recover_bad_project_service(root: &Path, record: &DaemonRecord, err: &io::Error) {
    append_frontend_project_log(
        root,
        &format!(
            "frontend-daemon-recover pid={} reason={}",
            record.pid,
            clean_log_text(&err.to_string(), 240)
        ),
    );
    stop_process(record.pid);
    let _ = fs::remove_file(record_path(root));
}

fn is_recoverable_daemon_request_error(err: &io::Error) -> bool {
    matches!(
        err.kind(),
        io::ErrorKind::ConnectionRefused
            | io::ErrorKind::ConnectionReset
            | io::ErrorKind::UnexpectedEof
            | io::ErrorKind::InvalidData
            | io::ErrorKind::BrokenPipe
    )
}

fn search_daemon_args(args: &[String]) -> Vec<String> {
    let resolved_color = if stdout_supports_color() {
        "always"
    } else {
        "never"
    };
    let mut out = Vec::with_capacity(args.len() + 3);
    let mut i = 0usize;
    while i < args.len() {
        let arg = &args[i];
        if arg == "--color" {
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
            out.push(if value == "auto" {
                format!("--color={resolved_color}")
            } else {
                arg.clone()
            });
            i += 1;
            continue;
        }
        out.push(arg.clone());
        i += 1;
    }
    let defaults = search_output_default_args(&out);
    insert_before_double_dash(&mut out, defaults);
    out
}

fn search_output_default_args(args: &[String]) -> Vec<String> {
    let resolved_color = if stdout_supports_color() {
        "always"
    } else {
        "never"
    };
    let decorated = stdout_supports_decoration();
    let mut defaults = Vec::with_capacity(3);
    let mut saw_color = false;
    let mut saw_heading = false;
    let mut saw_line_number = false;
    for arg in args {
        if arg == "--color" || arg.starts_with("--color=") {
            saw_color = true;
        } else if arg == "--heading" || arg == "--no-heading" {
            saw_heading = true;
        } else if matches!(
            arg.as_str(),
            "-n" | "--line-number" | "-N" | "--no-line-number"
        ) {
            saw_line_number = true;
        }
    }
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
    defaults
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
}

enum SearchRunKind {
    Indexed(PathBuf),
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

fn grep_search_path_args(args: &[String]) -> Result<Vec<SearchPathArg>, String> {
    let mut patterns = 0usize;
    let mut paths = Vec::new();
    let mut positional_only = false;
    let mut i = 0usize;
    while i < args.len() {
        let arg = &args[i];
        if positional_only {
            if patterns == 0 {
                patterns += 1;
            } else {
                paths.push((i, arg.clone()));
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
            let (name, inline_value) = long.split_once('=').unwrap_or((long, ""));
            match name {
                "regexp" | "file" => {
                    patterns += 1;
                    i += if inline_value.is_empty() { 2 } else { 1 };
                    continue;
                }
                "after-context" | "before-context" | "context" | "max-count" | "include"
                | "exclude" | "exclude-dir" | "directories" | "color" | "colour"
                | "binary-files" | "devices" | "label" => {
                    i += if inline_value.is_empty() { 2 } else { 1 };
                    continue;
                }
                _ => {
                    i += 1;
                    continue;
                }
            }
        }
        if arg.starts_with('-') && arg != "-" {
            if let Some(consumed) = consume_grep_short_path_options(args, i, arg, &mut patterns) {
                i += consumed;
                continue;
            }
            i += 1;
            continue;
        }
        if patterns == 0 {
            patterns += 1;
        } else {
            paths.push((i, arg.clone()));
        }
        i += 1;
    }
    if patterns == 0 {
        return Err("a grep pattern is required".to_string());
    }
    Ok(search_paths_from_raw(paths))
}

fn consume_grep_short_path_options(
    args: &[String],
    index: usize,
    arg: &str,
    patterns: &mut usize,
) -> Option<usize> {
    let bytes = arg.as_bytes();
    let mut pos = 1usize;
    while pos < bytes.len() {
        match bytes[pos] as char {
            'e' | 'f' => {
                *patterns += 1;
                return Some(if pos + 1 < arg.len() { 1 } else { 2 });
            }
            'A' | 'B' | 'C' | 'm' | 'd' | 'D' => {
                return Some(if pos + 1 < arg.len() { 1 } else { 2 });
            }
            _ => pos += 1,
        }
    }
    if index >= args.len() { None } else { Some(1) }
}

fn grep_args_for_group(args: &[String], keep_path_indexes: &[Option<usize>]) -> Vec<String> {
    let explicit_indexes: Vec<usize> = keep_path_indexes.iter().filter_map(|idx| *idx).collect();
    if explicit_indexes.is_empty() {
        return args.to_vec();
    }
    let all_path_indexes = grep_search_path_args(args)
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

fn search_paths_from_raw(paths: Vec<(usize, String)>) -> Vec<SearchPathArg> {
    let cwd = env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    if paths.is_empty() {
        return vec![SearchPathArg {
            arg_index: None,
            abs: cwd,
        }];
    }
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
        .collect()
}

fn search_runs(paths: &[SearchPathArg]) -> Result<Vec<SearchRun>, MissingProject> {
    let mut runs = Vec::new();
    for path in paths {
        let Some(root) = find_project_root(&path.abs) else {
            if paths.len() == 1 && path.arg_index.is_none() {
                return Err(MissingProject::Single);
            }
            return Err(MissingProject::Path(path.abs.clone()));
        };
        push_search_run(&mut runs, SearchRunKind::Indexed(root), path.arg_index);
    }
    Ok(runs)
}

fn push_search_run(
    runs: &mut Vec<SearchRun>,
    kind: SearchRunKind,
    path_index: Option<usize>,
) {
    if let Some(last) = runs.last_mut()
        && search_run_kind_matches(&last.kind, &kind)
    {
        last.path_indexes.push(path_index);
        return;
    }
    runs.push(SearchRun {
        kind,
        path_indexes: vec![path_index],
    });
}

fn search_run_kind_matches(left: &SearchRunKind, right: &SearchRunKind) -> bool {
    match (left, right) {
        (SearchRunKind::Indexed(left), SearchRunKind::Indexed(right)) => {
            normalized_existing_path(left) == normalized_existing_path(right)
        }
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
    if missing_project_should_skip_prompt()
        || !(io::stdin().is_terminal() && io::stderr().is_terminal())
    {
        return run_system_ripgrep(&ripgrep_stdin_args(args));
    }
    if io::stdin().is_terminal() && io::stderr().is_terminal() {
        eprint!(
            "{}: no IndexSearch project found above {}. Create one here? [Y/n] ",
            frontend_command_name(),
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
                        return run_with_fallback(args);
                    }
                    return Ok(2);
                }
                "n" | "N" | "no" | "NO" => {
                    return run_system_ripgrep(&ripgrep_stdin_args(args));
                }
                _ => {}
            }
        }
    }
    run_system_ripgrep(&ripgrep_stdin_args(args))
}

fn missing_project_should_skip_prompt() -> bool {
    if env::var_os("INDEXSEARCH_NO_AGENT_AUTO_PROJECT").is_some() {
        return false;
    }
    env::var_os("INDEXSEARCH_AGENT_AUTO_PROJECT").is_some()
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

#[cfg(windows)]
fn hidden_background_command(program: impl AsRef<std::ffi::OsStr>) -> Command {
    let mut command = Command::new(program);
    use std::os::windows::process::CommandExt;
    use windows_sys::Win32::System::Threading::CREATE_NO_WINDOW;

    command.creation_flags(CREATE_NO_WINDOW);
    command
}

#[cfg(not(windows))]
fn hidden_background_command(program: impl AsRef<std::ffi::OsStr>) -> Command {
    Command::new(program)
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
    if record.protocol != SEARCH_DAEMON_PROTOCOL {
        return false;
    }
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
            format!(
                "invalid daemon response magic: got {} expected {}",
                format_bytes_hex(&magic),
                format_bytes_hex(RESPONSE_MAGIC)
            ),
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
                    format!(
                        "invalid daemon response frame: tag=0x{:02x} expected one of 0x{:02x}/0x{:02x}/0x{:02x}",
                        tag[0], STDOUT_FRAME, STDERR_FRAME, DONE_FRAME
                    ),
                ));
            }
        }
    }
}

fn format_bytes_hex(bytes: &[u8]) -> String {
    bytes
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<Vec<_>>()
        .join(" ")
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
    ["is-daemon", "istool"]
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

fn project_config_exists(root: &Path) -> bool {
    project_config_path(root).is_file()
}

fn project_marker_exists(root: &Path) -> bool {
    project_config_exists(root) || index_path(root).is_file()
}

fn record_path(root: &Path) -> PathBuf {
    root.join(INDEX_DIR).join(SEARCH_DAEMON_FILE)
}

fn project_log_path(root: &Path) -> PathBuf {
    root.join(INDEX_DIR).join(PROJECT_LOG_FILE)
}

fn append_frontend_project_log(root: &Path, message: &str) {
    let _ = fs::create_dir_all(root.join(INDEX_DIR));
    let Ok(mut file) = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(project_log_path(root))
    else {
        return;
    };
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs().to_string())
        .unwrap_or_else(|_| "0".to_string());
    let _ = writeln!(file, "{timestamp} {message}");
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
    protocol: u32,
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
            "protocol" => record.protocol = value.parse().map_err(|_| "invalid daemon protocol")?,
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
        || record.protocol == 0
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
        let _ = Command::new("kill")
            .arg(pid.to_string())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
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
    fn invalid_daemon_response_frame_reports_tag() {
        let mut response = RESPONSE_MAGIC.to_vec();
        response.push(0x7f);
        let err = read_daemon_response(&mut response.as_slice()).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        assert!(err.to_string().contains("tag=0x7f"));
    }

    #[test]
    fn invalid_daemon_response_magic_reports_bytes() {
        let mut response = b"notmagic".to_vec();
        response.push(DONE_FRAME);
        response.extend_from_slice(&0u32.to_le_bytes());
        let err = read_daemon_response(&mut response.as_slice()).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        assert!(err.to_string().contains("invalid daemon response magic"));
        assert!(err.to_string().contains("6e 6f 74"));
    }

    #[test]
    fn daemon_record_requires_protocol() {
        let path = env::temp_dir().join(format!(
            "indexsearch-daemon-record-{}-missing-protocol.txt",
            std::process::id()
        ));
        fs::write(
            &path,
            "pid=1\nport=1\ntoken=t\nroot=/tmp\nexe_path=/bin/echo\nexe_size=1\nexe_mtime=1\nindex_size=1\nindex_mtime=1\n",
        )
        .unwrap();
        let result = read_record(&path);
        let _ = fs::remove_file(path);
        assert!(result.is_err());
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
