use std::env;
use std::fs;
use std::io::{self, IsTerminal, Read, Write};
use std::net::{SocketAddr, TcpStream};
#[cfg(unix)]
use std::os::fd::AsRawFd;
#[cfg(unix)]
use std::os::unix::net::UnixStream;
#[cfg(windows)]
use std::os::windows::io::{AsRawHandle, RawHandle};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};
use std::time::{Duration, Instant};

const INDEX_DIR: &str = ".indexsearch";
const INDEX_FILE: &str = "index.bin";
const PROJECT_FILE: &str = "index-search-project.txt";
const SEARCH_DAEMON_FILE: &str = "search-daemon.txt";
const STATE_FILE: &str = "state.txt";
const WATCH_DIR: &str = "watches";
const REQUEST_MAGIC: &[u8; 8] = b"ISDREQ1\n";
const RESPONSE_MAGIC: &[u8; 8] = b"ISDRES1\n";
const STDOUT_FRAME: u8 = 1;
const STDERR_FRAME: u8 = 2;
const DONE_FRAME: u8 = 3;
const CONNECT_TIMEOUT: Duration = Duration::from_millis(20);
const START_TIMEOUT: Duration = Duration::from_millis(250);

#[derive(Default)]
struct FrontendProfile {
    enabled: bool,
    events: Vec<(&'static str, f64)>,
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
            self.events
                .push((name, start.elapsed().as_secs_f64() * 1000.0));
        }
    }

    fn print(&self) {
        if !self.enabled {
            return;
        }
        let stderr = io::stderr();
        let mut err = io::BufWriter::new(stderr.lock());
        for (name, ms) in &self.events {
            let _ = writeln!(err, "profile: {name}={ms:.3}ms");
        }
        let _ = err.flush();
    }
}

fn main() {
    let args: Vec<String> = env::args().skip(1).collect();
    match run(&args) {
        Ok(code) => std::process::exit(code),
        Err(message) => {
            eprintln!("is: {message}");
            std::process::exit(2);
        }
    }
}

fn run(args: &[String]) -> Result<i32, String> {
    if should_delegate(args) {
        return run_backend(args);
    }
    let total_timer = Instant::now();
    let mut profile = FrontendProfile::new(args);
    let prepare_timer = Instant::now();
    let daemon_args = search_daemon_args(args);
    let search_args = strip_search_command(args);
    profile.record("frontend_prepare_args", prepare_timer);
    if search_args.iter().any(|arg| {
        matches!(
            arg.as_str(),
            "--no-daemon" | "--auto-update" | "--auto-update-untracked"
        )
    }) {
        return run_backend(args);
    }
    let start_timer = Instant::now();
    let Some(start) = search_start_path(&search_args) else {
        return run_backend(args);
    };
    profile.record("frontend_resolve_start_path", start_timer);
    if search_args.iter().any(|arg| arg == "--no-auto-index") {
        return run_backend(args);
    }
    let root_timer = Instant::now();
    let Some(mut root) = find_project_root(&start) else {
        return handle_missing_project(args);
    };
    profile.record("frontend_find_project_root", root_timer);
    let ready_record;
    let ensure_timer = Instant::now();
    (root, ready_record) = ensure_watch(&root)?;
    profile.record("frontend_ensure_project_service", ensure_timer);
    let record_timer = Instant::now();
    if let Some(record) = ready_record.or_else(|| read_valid_record(&root)) {
        profile.record("frontend_read_daemon_record", record_timer);
        if let Ok(code) = request_daemon(&record, &daemon_args, &mut profile) {
            profile.record("frontend_total", total_timer);
            profile.print();
            return Ok(code);
        }
        let _ = fs::remove_file(record_path(&root));
    } else {
        profile.record("frontend_read_daemon_record", record_timer);
    }
    let start_daemon_timer = Instant::now();
    start_daemon(&root)?;
    profile.record("frontend_start_daemon", start_daemon_timer);
    let deadline = Instant::now() + START_TIMEOUT;
    while Instant::now() < deadline {
        let record_timer = Instant::now();
        if let Some(record) = read_valid_record(&root) {
            profile.record("frontend_read_daemon_record", record_timer);
            if let Ok(code) = request_daemon(&record, &daemon_args, &mut profile) {
                profile.record("frontend_total", total_timer);
                profile.print();
                return Ok(code);
            }
        } else {
            profile.record("frontend_read_daemon_record", record_timer);
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    run_backend(args)
}

fn should_delegate(args: &[String]) -> bool {
    if args.is_empty() {
        return true;
    }
    let first = args[0].as_str();
    if matches!(first, "-h" | "--help" | "-V" | "--version") {
        return true;
    }
    matches!(
        first,
        "index"
            | "update"
            | "compact"
            | "clean"
            | "watch"
            | "watch-daemon"
            | "search-daemon"
            | "list-watches"
            | "watch-list"
            | "unwatch"
            | "watch-log"
            | "install"
            | "install-skills"
            | "status"
    )
}

fn strip_search_command(args: &[String]) -> Vec<String> {
    if args.first().is_some_and(|arg| arg == "search") {
        args[1..].to_vec()
    } else {
        args.to_vec()
    }
}

fn search_daemon_args(args: &[String]) -> Vec<String> {
    let args = strip_search_command(args);
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

fn search_start_path(args: &[String]) -> Option<PathBuf> {
    let mut after_double_dash = false;
    let mut saw_pattern = false;
    let mut files_mode = false;
    let mut paths = Vec::new();
    let mut i = 0usize;
    while i < args.len() {
        let arg = &args[i];
        if after_double_dash {
            if saw_pattern {
                paths.push(arg.clone());
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
            paths.push(arg.clone());
        } else {
            saw_pattern = true;
        }
        i += 1;
    }
    let cwd = env::current_dir().ok()?;
    let start = paths
        .first()
        .map(PathBuf::from)
        .unwrap_or_else(|| cwd.clone());
    Some(if start.is_absolute() {
        start
    } else {
        cwd.join(start)
    })
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
            | "--sort"
            | "--sortr"
    )
}

fn short_option_with_attached_value(arg: &str) -> bool {
    (arg.starts_with("-A") || arg.starts_with("-B") || arg.starts_with("-C")) && arg.len() > 2
}

fn find_project_root(start: &Path) -> Option<PathBuf> {
    start
        .ancestors()
        .find(|ancestor| {
            index_path(ancestor).is_file()
                || ancestor.join(INDEX_DIR).is_dir()
                || ancestor.join(PROJECT_FILE).is_file()
        })
        .map(Path::to_path_buf)
}

fn handle_missing_project(args: &[String]) -> Result<i32, String> {
    if io::stdin().is_terminal() && io::stderr().is_terminal() {
        let cwd = env::current_dir().map_err(|err| err.to_string())?;
        eprint!(
            "is: no IndexSearch project found above {}. Create one here? [Y/n] ",
            display_path(&cwd)
        );
        let _ = io::stderr().flush();
        let mut answer = String::new();
        if io::stdin().read_line(&mut answer).is_ok() {
            match answer.trim() {
                "" | "y" | "Y" | "yes" | "YES" => {
                    let (root, _) = ensure_watch(&cwd)?;
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
        "is: no IndexSearch project found; run `indexsearch index .` or `is watch .` at the project root"
    );
    Ok(2)
}

fn ensure_watch(root: &Path) -> Result<(PathBuf, Option<DaemonRecord>), String> {
    if let Some(covering_root) = watch_covering_root(root) {
        if let Some(record) = ready_watch_record(&covering_root) {
            return Ok((covering_root, Some(record)));
        }
        stop_watch_for_root(&covering_root);
    }
    start_watch(root)?;
    Ok((
        watch_covering_root(root).unwrap_or_else(|| root.to_path_buf()),
        None,
    ))
}

fn ready_watch_record(root: &Path) -> Option<DaemonRecord> {
    if !index_path(root).is_file() {
        return None;
    }
    read_valid_record(root)
}

fn stop_watch_for_root(root: &Path) {
    let requested_root = normalized_existing_path(root);
    let registry = watch_registry_dir();
    let Ok(entries) = fs::read_dir(registry) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.extension().is_some_and(|ext| ext == "watch") {
            continue;
        }
        let Ok(record) = read_watch_record(&path) else {
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

fn watch_covering_root(root: &Path) -> Option<PathBuf> {
    let requested_root = normalized_existing_path(root);
    let registry = watch_registry_dir();
    let entries = fs::read_dir(registry).ok()?;
    for entry in entries.flatten() {
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
        let record_root = normalized_existing_path(&record.root);
        if path_is_ancestor(&record_root, &requested_root) {
            return Some(record.root);
        }
    }
    None
}

struct ProgressLine {
    done: Arc<AtomicBool>,
    started: Instant,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl ProgressLine {
    fn start(label: &'static str) -> Self {
        if !stderr_supports_progress() {
            return Self {
                done: Arc::new(AtomicBool::new(true)),
                started: Instant::now(),
                handle: None,
            };
        }
        let done = Arc::new(AtomicBool::new(false));
        let thread_done = Arc::clone(&done);
        let started = Instant::now();
        let handle = std::thread::spawn(move || {
            let frames = ["|", "/", "-", "\\"];
            let width = 28usize;
            let mut tick = 0usize;
            while !thread_done.load(AtomicOrdering::Relaxed) {
                eprint!(
                    "\r  {} {:<18} {} {:>5.1}s",
                    color_progress(frames[tick % frames.len()]),
                    label,
                    progress_bar(tick, width),
                    started.elapsed().as_secs_f32()
                );
                let _ = io::stderr().flush();
                tick = tick.wrapping_add(1);
                std::thread::sleep(Duration::from_millis(80));
            }
            clear_progress_line();
        });
        Self {
            done,
            started,
            handle: Some(handle),
        }
    }

    fn finish(mut self, label: &str) {
        self.stop();
        if stderr_supports_progress() {
            eprintln!(
                "│");
            eprintln!(
                "{} {} — done ({:.1}s)",
                color_success("◆"),
                label,
                self.started.elapsed().as_secs_f32()
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

fn stderr_supports_progress() -> bool {
    io::stderr().is_terminal()
        && env::var("TERM").map(|term| term != "dumb").unwrap_or(true)
        && env::var_os("INDEXSEARCH_NO_PROGRESS").is_none()
}

fn clear_progress_line() {
    eprint!("\r{}\r", " ".repeat(80));
    let _ = io::stderr().flush();
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

fn stderr_supports_color() -> bool {
    if env::var_os("NO_COLOR").is_some() {
        return false;
    }
    if env::var("CLICOLOR_FORCE").is_ok_and(|value| value != "0") {
        return true;
    }
    io::stderr().is_terminal() && stdout_supports_ansi()
}

fn start_watch(root: &Path) -> Result<(), String> {
    let backend = backend_path()?;
    let mut command = Command::new(backend);
    command.arg("watch");
    command.arg(root).stdin(Stdio::null()).stdout(Stdio::null());
    if stderr_supports_progress() {
        command.stderr(Stdio::inherit());
    } else {
        command.stderr(Stdio::null());
    }
    let status = command.status().map_err(|err| err.to_string())?;
    if status.success() {
        Ok(())
    } else {
        Err(format!(
            "failed to start watcher for {}",
            display_path(root)
        ))
    }
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
    let backend = backend_path().ok();
    let Some(backend) = backend.as_ref() else {
        return false;
    };
    let Ok(backend_meta) = fs::metadata(backend) else {
        return false;
    };
    let Ok(index_meta) = fs::metadata(index_path(&record.root)) else {
        return false;
    };
    let state_meta = fs::metadata(state_path(&record.root)).ok();
    let state_size = state_meta.as_ref().map(|meta| meta.len()).unwrap_or(0);
    let state_mtime = state_meta.as_ref().map(mtime_ns).unwrap_or(0);
    same_fileish(backend, &record.exe_path)
        && backend_meta.len() == record.exe_size
        && mtime_ns(&backend_meta) == record.exe_mtime
        && index_meta.len() == record.index_size
        && mtime_ns(&index_meta) == record.index_mtime
        && state_size == record.state_size
        && state_mtime == record.state_mtime
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
            STDOUT_FRAME => copy_frame(&mut stream, &mut out)?,
            STDERR_FRAME => copy_frame(&mut stream, &mut err)?,
            DONE_FRAME => {
                let code = read_u32(&mut stream)? as i32;
                out.flush()?;
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
fn send_stdout_handle(stream: &mut TcpStream, record: &DaemonRecord) -> io::Result<()> {
    let handle = if windows_direct_stdout_enabled() {
        duplicate_stdout_for_process(record.pid)
            .map(|handle| handle as usize as u64)
            .unwrap_or(0)
    } else {
        0
    };
    stream.write_all(&handle.to_le_bytes())
}

#[cfg(not(windows))]
fn send_stdout_handle(_stream: &mut TcpStream, _record: &DaemonRecord) -> io::Result<()> {
    Ok(())
}

#[cfg(windows)]
fn duplicate_stdout_for_process(pid: u32) -> io::Result<RawHandle> {
    use std::ptr;
    use windows_sys::Win32::Foundation::{
        CloseHandle, DUPLICATE_SAME_ACCESS, DuplicateHandle, HANDLE,
    };
    use windows_sys::Win32::System::Threading::{
        GetCurrentProcess, OpenProcess, PROCESS_DUP_HANDLE,
    };

    unsafe {
        let target_process = OpenProcess(PROCESS_DUP_HANDLE, 0, pid);
        if target_process.is_null() {
            return Err(io::Error::last_os_error());
        }
        let mut target_handle: HANDLE = ptr::null_mut();
        let ok = DuplicateHandle(
            GetCurrentProcess(),
            io::stdout().as_raw_handle() as HANDLE,
            target_process,
            &mut target_handle,
            0,
            0,
            DUPLICATE_SAME_ACCESS,
        );
        let result = if ok == 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(target_handle as RawHandle)
        };
        CloseHandle(target_process);
        result
    }
}

#[cfg(windows)]
fn windows_direct_stdout_enabled() -> bool {
    matches!(
        env::var("INDEXSEARCH_WINDOWS_DIRECT_STDOUT").as_deref(),
        Ok("1") | Ok("true") | Ok("TRUE") | Ok("yes") | Ok("YES")
    )
}

fn start_daemon(root: &Path) -> Result<(), String> {
    let backend = backend_path()?;
    let progress = ProgressLine::start("Starting service");
    let mut command = Command::new(backend);
    command
        .arg("search-daemon")
        .arg("--watch")
        .arg("--detach")
        .arg(root)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    detach_background(&mut command);
    command.spawn().map_err(|err| err.to_string())?;
    progress.finish("Project service started");
    Ok(())
}

fn run_backend(args: &[String]) -> Result<i32, String> {
    run_backend_owned(args.iter().map(String::as_str))
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
    let dir = exe
        .parent()
        .ok_or_else(|| "current executable has no parent".to_string())?;
    let versioned_daemon = format!("is-daemon-{}", env!("CARGO_PKG_VERSION"));
    for name in [versioned_daemon.as_str(), "is-daemon", "indexsearch"] {
        let sibling = dir.join(executable_name(name));
        if sibling.is_file() && fs::canonicalize(&sibling).ok() != fs::canonicalize(&exe).ok() {
            return Ok(sibling);
        }
    }
    if exe
        .file_stem()
        .and_then(|stem| stem.to_str())
        .is_some_and(|stem| stem == "is-daemon")
    {
        return Ok(exe);
    }
    Err(format!(
        "cannot find `{}` next to {}",
        executable_name("is-daemon"),
        display_path(&exe)
    ))
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

#[cfg(unix)]
fn detach_background(command: &mut Command) {
    use std::os::unix::process::CommandExt;
    unsafe {
        command.pre_exec(|| {
            libc_setsid();
            Ok(())
        });
    }
}

#[cfg(unix)]
fn libc_setsid() {
    unsafe extern "C" {
        fn setsid() -> i32;
    }
    unsafe {
        let _ = setsid();
    }
}

#[cfg(windows)]
fn detach_background(command: &mut Command) {
    use std::os::windows::process::CommandExt;
    command.creation_flags(0x0800_0000);
}

#[cfg(not(any(unix, windows)))]
fn detach_background(_command: &mut Command) {}

fn index_path(root: &Path) -> PathBuf {
    root.join(INDEX_DIR).join(INDEX_FILE)
}

fn state_path(root: &Path) -> PathBuf {
    root.join(INDEX_DIR).join(STATE_FILE)
}

fn record_path(root: &Path) -> PathBuf {
    root.join(INDEX_DIR).join(SEARCH_DAEMON_FILE)
}

fn watch_registry_dir() -> PathBuf {
    env::var_os("HOME")
        .or_else(|| env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".indexsearch")
        .join(WATCH_DIR)
}

fn normalized_existing_path(path: &Path) -> PathBuf {
    fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

fn path_is_ancestor(parent: &Path, child: &Path) -> bool {
    child == parent || child.starts_with(parent)
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
    state_size: u64,
    state_mtime: i64,
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
            "state_size" => record.state_size = value.parse().map_err(|_| "invalid state_size")?,
            "state_mtime" => {
                record.state_mtime = value.parse().map_err(|_| "invalid state_mtime")?
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
struct WatchRecord {
    pid: u32,
    root: PathBuf,
}

fn read_watch_record(path: &Path) -> Result<WatchRecord, String> {
    let text = fs::read_to_string(path).map_err(|err| err.to_string())?;
    let mut record = WatchRecord::default();
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
        return Err("invalid watch record".to_string());
    }
    Ok(record)
}

fn process_alive(pid: u32) -> bool {
    #[cfg(unix)]
    {
        unsafe { libc::kill(pid as i32, 0) == 0 }
    }
    #[cfg(windows)]
    {
        use windows_sys::Win32::Foundation::{CloseHandle, STILL_ACTIVE};
        use windows_sys::Win32::System::Threading::{
            GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
        };

        unsafe {
            let process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
            if process.is_null() {
                return false;
            }
            let mut code = 0u32;
            let ok = GetExitCodeProcess(process, &mut code);
            CloseHandle(process);
            ok != 0 && code == STILL_ACTIVE as u32
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
            .status();
    }
}

fn write_frame(writer: &mut impl Write, bytes: &[u8]) -> io::Result<()> {
    writer.write_all(&(bytes.len() as u64).to_le_bytes())?;
    writer.write_all(bytes)
}

fn copy_frame(reader: &mut impl Read, writer: &mut impl Write) -> io::Result<()> {
    let mut remaining = read_u64(reader)?;
    let mut buffer = [0u8; 64 * 1024];
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
