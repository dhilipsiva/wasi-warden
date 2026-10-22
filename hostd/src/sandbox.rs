//! The rules the host enforces on both of its executors: the planner's JSON actions
//! (`actions.rs`) and the WIT capabilities (`capabilities.rs`).
//!
//! - Paths stay inside the workspace, through symlinks too ([`confine`]).
//! - A program runs only as the allowlist resolved it at config load, never from inside the
//!   workspace ([`resolve_allowed`], [`admit`]).
//! - The plan sets only the environment variables the operator allows, never the loader's, `PATH`
//!   or shell hooks; the host sets the child's `PATH` ([`check_env_key`], [`child_path`]).
//! - A process is bounded in time and output, and when it ends (or times out) its whole process
//!   group ends with it ([`run_bounded`]).
//! - Reads are capped ([`cap_read`]).
//! - The browser visits only allowed hosts over http(s) ([`check_navigation`]).

use std::ffi::{OsStr, OsString};
use std::fmt;
use std::fs;
use std::io::{self, Read};
use std::os::unix::process::CommandExt;
use std::path::{Component, Path, PathBuf};
use std::process::{ChildStderr, ChildStdout, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use rustix::event::{PollFd, PollFlags};
use rustix::process::{Pid, Signal, WaitId, WaitIdOptions};

use camino::{Utf8Path, Utf8PathBuf};

/// The most bytes one read returns, whatever the caller asks for.
pub const MAX_READ_BYTES: u64 = 1 << 20;
/// The most bytes kept of each of a process's output streams.
pub const MAX_OUTPUT_BYTES: usize = 1 << 20;
/// How long a process may run when the caller names no timeout.
pub const DEFAULT_PROC_TIMEOUT: Duration = Duration::from_secs(30);
/// The longest timeout a caller may ask for.
pub const MAX_PROC_TIMEOUT: Duration = Duration::from_secs(300);

/// After a process ends, how long to keep reading what it already wrote. A process that left the
/// group (`setsid`) and still holds the output cannot hold the caller past this.
const DRAIN_GRACE: Duration = Duration::from_millis(200);
/// How often the runner looks at the process while it waits for output.
const POLL_INTERVAL: Duration = Duration::from_millis(10);

/// Why a path was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PathError {
    Absolute,
    Parent,
    NotUtf8,
    /// The path (as the caller wrote it) resolves outside the workspace.
    Escapes(String),
    /// The path runs through a symlink whose target does not exist, so it cannot be checked.
    Dangling(String),
    Io(String),
}

impl fmt::Display for PathError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Absolute => f.write_str("absolute paths are not allowed"),
            Self::Parent => f.write_str("path traversal segments are not allowed"),
            Self::NotUtf8 => f.write_str("path is not valid UTF-8"),
            Self::Escapes(p) => write!(f, "path `{p}` escapes the workspace"),
            Self::Dangling(p) => write!(f, "path `{p}` runs through a dangling symlink"),
            Self::Io(e) => f.write_str(e),
        }
    }
}

impl std::error::Error for PathError {}

/// Whether `candidate` lies inside `root`, comparing whole path components (so `/w/sandbox2` is
/// not inside `/w/sandbox`).
pub fn within(root: &Path, candidate: &Path) -> bool {
    candidate.starts_with(root)
}

/// `relative` resolved under `base`, a directory inside the workspace `root`. Absolute paths and
/// `..` are refused, and so is any path whose existing part resolves, through symlinks, outside
/// `root`, or through a dangling symlink. Symlinks along the way are resolved in the result; a tail
/// that does not exist yet is kept as written (it holds no symlink).
///
/// Known limit: the check and the use are separate system calls, so another process swapping a
/// directory for a symlink in between could still escape. Only an `openat2(RESOLVE_BENEATH)`-style
/// open closes that window.
pub fn confine(root: &Utf8Path, base: &Utf8Path, relative: &str) -> Result<Utf8PathBuf, PathError> {
    let rel = Path::new(relative);
    if rel.is_absolute() {
        return Err(PathError::Absolute);
    }
    let root = fs::canonicalize(root).map_err(|e| PathError::Io(format!("workspace root: {e}")))?;
    let mut current = fs::canonicalize(base).map_err(|e| PathError::Io(format!("{base}: {e}")))?;
    if !within(&root, &current) {
        return Err(PathError::Escapes(base.to_string()));
    }
    let mut missing = false;
    for component in rel.components() {
        let segment = match component {
            Component::CurDir => continue,
            Component::Normal(segment) => segment,
            Component::ParentDir => return Err(PathError::Parent),
            Component::Prefix(_) | Component::RootDir => return Err(PathError::Absolute),
        };
        let next = current.join(segment);
        if missing {
            current = next;
            continue;
        }
        match fs::symlink_metadata(&next) {
            Ok(meta) if meta.file_type().is_symlink() => {
                let target = fs::canonicalize(&next)
                    .map_err(|_| PathError::Dangling(relative.to_string()))?;
                if !within(&root, &target) {
                    return Err(PathError::Escapes(relative.to_string()));
                }
                current = target;
            }
            Ok(_) => current = next,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                missing = true;
                current = next;
            }
            Err(e) => return Err(PathError::Io(format!("{relative}: {e}"))),
        }
    }
    Utf8PathBuf::from_path_buf(current).map_err(|_| PathError::NotUtf8)
}

/// Like [`confine`], but the last component is not followed: the parent is confined and the name
/// joined to it, so removing or renaming a symlink acts on the link, never its target.
pub fn confine_entry(
    root: &Utf8Path,
    base: &Utf8Path,
    relative: &str,
) -> Result<Utf8PathBuf, PathError> {
    let rel = Utf8Path::new(relative);
    let Some(name) = rel.file_name() else {
        return Err(PathError::Parent);
    };
    if matches!(
        rel.components().next_back(),
        Some(camino::Utf8Component::ParentDir)
    ) {
        return Err(PathError::Parent);
    }
    let parent = rel.parent().map(Utf8Path::as_str).unwrap_or("");
    Ok(confine(root, base, parent)?.join(name))
}

/// A program the allowlist admits: the name it was configured as, and the absolute path it runs as.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AllowedProgram {
    pub name: String,
    pub path: PathBuf,
}

/// `dir` canonicalised, joined with `name`. The file name is kept rather than followed, so a
/// multi-call binary (busybox, a single-binary coreutils) keeps the name it dispatches on.
fn resolved(dir: &Path, name: &OsStr) -> Option<PathBuf> {
    let path = fs::canonicalize(dir).ok()?.join(name);
    path.is_file().then_some(path)
}

/// Resolve one allowlist entry: a bare name through `path_var` (the host's `PATH` when the config
/// loads; relative entries skipped), an absolute path as written. Anything else, a program that does
/// not exist, or one that resolves (through any symlink) inside the workspace `root`, where the plan
/// could rewrite it, is a configuration error.
pub fn resolve_allowed(
    entry: &str,
    path_var: Option<&OsStr>,
    root: &Utf8Path,
) -> Result<AllowedProgram, String> {
    let program = Path::new(entry);
    let path = if entry.is_empty() {
        None
    } else if entry.contains('/') {
        if !program.is_absolute() {
            return Err(format!(
                "allow_proc entry `{entry}` must be a bare name or an absolute path"
            ));
        }
        match (program.parent(), program.file_name()) {
            (Some(dir), Some(name)) => resolved(dir, name),
            _ => None,
        }
    } else {
        path_var.and_then(|paths| {
            std::env::split_paths(paths)
                .filter(|dir| dir.is_absolute())
                .find_map(|dir| {
                    dir.join(entry)
                        .is_file()
                        .then(|| resolved(&dir, OsStr::new(entry)))
                        .flatten()
                })
        })
    };
    let path =
        path.ok_or_else(|| format!("allow_proc entry `{entry}` is not a program on this host"))?;
    if inside_workspace(&path, root) {
        return Err(format!(
            "allow_proc entry `{entry}` resolves inside the workspace, where the plan could rewrite it"
        ));
    }
    Ok(AllowedProgram {
        name: entry.to_string(),
        path,
    })
}

/// Whether `path`, every symlink followed, lies inside the workspace `root`.
fn inside_workspace(path: &Path, root: &Utf8Path) -> bool {
    match (fs::canonicalize(path), fs::canonicalize(root)) {
        (Ok(path), Ok(root)) => within(&root, &path),
        // An unresolvable root or program cannot be shown to be outside: treat it as inside.
        _ => true,
    }
}

/// The `PATH` the host gives a spawned process: its own `path_var`, keeping only absolute
/// directories outside the workspace `root` (so a plan cannot plant a program a script calls).
pub fn child_path(path_var: Option<&OsStr>, root: &Utf8Path) -> OsString {
    let dirs = path_var
        .map(|paths| {
            std::env::split_paths(paths)
                .filter(|dir| dir.is_absolute() && !inside_workspace(dir, root))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    std::env::join_paths(dirs).unwrap_or_default()
}

/// The program to run for `requested`, if the allowlist admits it. A bare name matches an entry
/// configured by that name; an absolute path matches an entry that resolves to the same file. A
/// planted program that merely shares an allowed base name matches nothing.
pub fn admit<'a>(allowed: &'a [AllowedProgram], requested: &str) -> Option<&'a Path> {
    if requested.is_empty() {
        return None;
    }
    if !requested.contains('/') {
        return allowed
            .iter()
            .find(|a| a.name == requested)
            .map(|a| a.path.as_path());
    }
    let program = Path::new(requested);
    if !program.is_absolute() {
        return None;
    }
    let path = resolved(program.parent()?, program.file_name()?)?;
    allowed
        .iter()
        .find(|a| a.path == path)
        .map(|a| a.path.as_path())
}

/// Variables no plan may set even when the operator lists them: the dynamic loader's, the child's
/// `PATH` (the host sets it), and the hooks shells and interpreters run code from.
const NEVER_FROM_THE_PLAN: [&str; 16] = [
    "PATH",
    "BASH_ENV",
    "ENV",
    "IFS",
    "SHELLOPTS",
    "PS4",
    "GCONV_PATH",
    "GLIBC_TUNABLES",
    "LOCPATH",
    "NODE_OPTIONS",
    "PYTHONPATH",
    "PYTHONHOME",
    "PYTHONSTARTUP",
    "PERL5LIB",
    "PERL5OPT",
    "RUBYOPT",
];

/// Whether the plan may set `key` in a spawned process's environment: only names on the operator's
/// `allowed` list, and never the loader's (`LD_*`, `DYLD_*`), exported shell functions
/// (`BASH_FUNC_*`), git's config injection (`GIT_CONFIG*`, `GIT_SSH*`, `GIT_ASKPASS`,
/// `GIT_EXEC_PATH`) or [`NEVER_FROM_THE_PLAN`]. An empty name, or one holding `=` or NUL, is
/// malformed.
pub fn check_env_key(key: &str, allowed: &[String]) -> Result<(), String> {
    if key.is_empty() || key.contains('=') || key.contains('\0') {
        return Err(format!("environment variable name `{key}` is malformed"));
    }
    let never = NEVER_FROM_THE_PLAN.contains(&key)
        || ["LD_", "DYLD_", "BASH_FUNC_", "GIT_CONFIG", "GIT_SSH"]
            .iter()
            .any(|prefix| key.starts_with(prefix))
        || matches!(key, "GIT_ASKPASS" | "GIT_EXEC_PATH");
    if never {
        return Err(format!("environment variable `{key}` is not allowed"));
    }
    if !allowed.iter().any(|a| a == key) {
        return Err(format!(
            "environment variable `{key}` is not in the host's allow_env"
        ));
    }
    Ok(())
}

/// The timeout for a request of `requested_ms`: the default when unset, never above the maximum.
pub fn proc_timeout(requested_ms: Option<u64>) -> Duration {
    requested_ms
        .map(Duration::from_millis)
        .unwrap_or(DEFAULT_PROC_TIMEOUT)
        .min(MAX_PROC_TIMEOUT)
}

/// The read size for a request of `requested` bytes.
pub fn cap_read(requested: u64) -> u64 {
    requested.min(MAX_READ_BYTES)
}

/// A bounded process's result.
#[derive(Debug, Default)]
pub struct Bounded {
    /// The exit code; `None` when a signal ended it (a timeout included).
    pub status: Option<i32>,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub stdout_truncated: bool,
    pub stderr_truncated: bool,
    pub timed_out: bool,
}

/// One of a process's output streams, read as it comes, keeping at most [`MAX_OUTPUT_BYTES`].
struct Stream<R> {
    pipe: Option<R>,
    kept: Vec<u8>,
    truncated: bool,
}

impl<R: Read> Stream<R> {
    fn new(pipe: Option<R>) -> Self {
        Self {
            pipe,
            kept: Vec::new(),
            truncated: false,
        }
    }

    /// Read what is ready (the caller polled first); the stream closes at its end or on an error.
    fn take_ready(&mut self) {
        let Some(pipe) = self.pipe.as_mut() else {
            return;
        };
        let mut chunk = [0u8; 16 * 1024];
        match pipe.read(&mut chunk) {
            Ok(0) | Err(_) => self.pipe = None,
            Ok(n) => {
                let room = MAX_OUTPUT_BYTES.saturating_sub(self.kept.len());
                self.kept.extend_from_slice(&chunk[..n.min(room)]);
                self.truncated |= n > room;
            }
        }
    }
}

/// Wait up to `wait` for either stream to have output or close, then read what is ready. With both
/// streams closed it just sleeps.
fn pump(out: &mut Stream<ChildStdout>, err: &mut Stream<ChildStderr>, wait: Duration) {
    let ready = {
        let mut fds = Vec::with_capacity(2);
        let mut which = Vec::with_capacity(2);
        if let Some(p) = &out.pipe {
            fds.push(PollFd::new(p, PollFlags::IN));
            which.push(0);
        }
        if let Some(p) = &err.pipe {
            fds.push(PollFd::new(p, PollFlags::IN));
            which.push(1);
        }
        if fds.is_empty() {
            thread::sleep(wait);
            return;
        }
        let timeout = rustix::time::Timespec::try_from(wait).ok();
        if rustix::event::poll(&mut fds, timeout.as_ref()).is_err() {
            return; // interrupted; the caller loops
        }
        fds.iter()
            .zip(which)
            .filter(|(fd, _)| !fd.revents().is_empty())
            .map(|(_, w)| w)
            .collect::<Vec<_>>()
    };
    for w in ready {
        if w == 0 {
            out.take_ready();
        } else {
            err.take_ready();
        }
    }
}

/// Whether the process `pid` has exited, without reaping it: while it is unreaped its pid still
/// names its process group, so a group kill cannot reach any other process.
fn exited(pid: Pid) -> io::Result<bool> {
    let options = WaitIdOptions::EXITED | WaitIdOptions::NOHANG | WaitIdOptions::NOWAIT;
    Ok(rustix::process::waitid(WaitId::Pid(pid), options)?.is_some())
}

/// Run `cmd` with stdin closed and its output piped, in a process group of its own, keeping at most
/// [`MAX_OUTPUT_BYTES`] of each stream. When the command exits, or `timeout` passes, the whole group
/// is killed (the command's background children included) before the command is reaped, and what
/// was already written is read for a moment longer. The command also dies if the host does.
///
/// Known limit: a process that leaves the group (`setsid`, a daemon) is not killed; it can only
/// come from an allowlisted program, and the caller stops reading it after [`DRAIN_GRACE`].
pub fn run_bounded(mut cmd: Command, timeout: Duration) -> io::Result<Bounded> {
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0);
    // SAFETY: the closure runs in the forked child before `exec`, where only async-signal-safe calls
    // are sound; it makes one `prctl` system call and touches no memory shared with the parent.
    unsafe {
        cmd.pre_exec(|| {
            rustix::process::set_parent_process_death_signal(Some(Signal::KILL))
                .map_err(io::Error::from)
        });
    }
    let mut child = cmd.spawn()?;
    let pid = Pid::from_child(&child);
    let mut out = Stream::new(child.stdout.take());
    let mut err = Stream::new(child.stderr.take());
    let deadline = Instant::now() + timeout;
    let mut timed_out = false;
    loop {
        if exited(pid)? {
            break;
        }
        if Instant::now() >= deadline {
            timed_out = true;
            break;
        }
        pump(&mut out, &mut err, POLL_INTERVAL);
    }
    // The command is still unreaped here, so its pid names its group: end the whole group, then reap.
    let _ = rustix::process::kill_process_group(pid, Signal::KILL);
    let status = child.wait()?;
    let drain_until = Instant::now() + DRAIN_GRACE;
    while (out.pipe.is_some() || err.pipe.is_some()) && Instant::now() < drain_until {
        pump(&mut out, &mut err, POLL_INTERVAL);
    }
    Ok(Bounded {
        status: if timed_out { None } else { status.code() },
        stdout: out.kept,
        stderr: err.kept,
        stdout_truncated: out.truncated,
        stderr_truncated: err.truncated,
        timed_out,
    })
}

/// Whether the browser may go to `url`: http or https only, to a host on `allowed` (exactly, or
/// `*.example.com` for its subdomains), compared without case. An empty list allows nothing. Returns
/// the URL as parsed, which is what the browser should be sent. Ports are not constrained.
pub fn check_navigation(url: &str, allowed: &[String]) -> Result<String, String> {
    let parsed = url::Url::parse(url).map_err(|e| format!("`{url}` is not a URL: {e}"))?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return Err(format!("`{url}`: only http and https pages may be visited"));
    }
    let host = parsed
        .host_str()
        .map(|h| h.trim_end_matches('.').to_ascii_lowercase())
        .unwrap_or_default();
    let allowed_host = allowed.iter().any(|entry| {
        let entry = entry.trim().trim_end_matches('.').to_ascii_lowercase();
        match entry.strip_prefix("*.") {
            Some(suffix) => host.ends_with(&format!(".{suffix}")),
            None => !entry.is_empty() && host == entry,
        }
    });
    if allowed_host {
        Ok(parsed.to_string())
    } else {
        Err(format!(
            "host `{host}` is not in the browser's allowed_hosts"
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> Utf8PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "hostd-sandbox-{name}-{}-{nanos}",
            std::process::id()
        ));
        fs::create_dir_all(&dir).expect("scratch dir");
        Utf8PathBuf::from_path_buf(fs::canonicalize(dir).expect("canonical")).expect("UTF-8")
    }

    // ── Paths ───────────────────────────────────────────────────────────────

    #[test]
    fn an_empty_path_is_the_base() {
        let root = scratch("empty");
        assert_eq!(confine(&root, &root, "").unwrap(), root);
    }

    #[test]
    fn a_relative_path_resolves_under_the_base_existing_or_not() {
        let root = scratch("relative");
        fs::create_dir(root.join("src")).unwrap();
        assert_eq!(
            confine(&root, &root, "src/main.rs").unwrap(),
            root.join("src/main.rs")
        );
        assert_eq!(
            confine(&root, &root, "./new/./dir").unwrap(),
            root.join("new/dir")
        );
    }

    #[test]
    fn a_parent_segment_or_an_absolute_path_is_refused() {
        let root = scratch("parent");
        for path in ["../etc/passwd", "a/../../etc", "a/.."] {
            assert_eq!(
                confine(&root, &root, path),
                Err(PathError::Parent),
                "{path}"
            );
        }
        assert_eq!(
            confine(&root, &root, "/etc/passwd"),
            Err(PathError::Absolute)
        );
    }

    /// `/w/sandbox2` shares a string prefix with `/w/sandbox` but is not inside it.
    #[test]
    fn a_sibling_sharing_the_roots_prefix_is_outside() {
        assert!(!within(
            Path::new("/w/sandbox"),
            Path::new("/w/sandbox2/secret")
        ));
        assert!(within(
            Path::new("/w/sandbox"),
            Path::new("/w/sandbox/inner")
        ));
    }

    #[test]
    fn symlinks_are_followed_only_inside_the_workspace() {
        let root = scratch("links");
        let outside = scratch("links-outside");
        fs::create_dir(root.join("real")).unwrap();
        std::os::unix::fs::symlink(&outside, root.join("out")).unwrap();
        std::os::unix::fs::symlink(root.join("real"), root.join("in")).unwrap();
        std::os::unix::fs::symlink(outside.join("missing"), root.join("dangling")).unwrap();
        assert!(matches!(
            confine(&root, &root, "out"),
            Err(PathError::Escapes(_))
        ));
        assert!(matches!(
            confine(&root, &root, "out/new-file"),
            Err(PathError::Escapes(_))
        ));
        assert_eq!(confine(&root, &root, "in/x").unwrap(), root.join("real/x"));
        assert!(matches!(
            confine(&root, &root, "dangling"),
            Err(PathError::Dangling(_))
        ));
    }

    #[test]
    fn a_base_outside_the_root_is_refused() {
        let root = scratch("base-root");
        let other = scratch("base-other");
        assert!(matches!(
            confine(&root, &other, "x"),
            Err(PathError::Escapes(_))
        ));
    }

    #[test]
    fn an_entry_is_not_followed_but_its_parent_is_checked() {
        let root = scratch("entry");
        let outside = scratch("entry-outside");
        std::os::unix::fs::symlink(&outside, root.join("link")).unwrap();
        assert_eq!(
            confine_entry(&root, &root, "link").unwrap(),
            root.join("link")
        );
        assert!(matches!(
            confine_entry(&root, &root, "link/inner"),
            Err(PathError::Escapes(_))
        ));
        assert_eq!(confine_entry(&root, &root, "a/.."), Err(PathError::Parent));
    }

    // ── Programs and environment ────────────────────────────────────────────

    /// A workspace somewhere else, for allowlist entries that must resolve outside it.
    fn elsewhere() -> Utf8PathBuf {
        scratch("not-here")
    }

    #[test]
    fn an_allowlist_entry_resolves_through_path_and_keeps_its_name() {
        let path_var = std::env::var_os("PATH");
        let ws = elsewhere();
        let echo = resolve_allowed("echo", path_var.as_deref(), &ws).unwrap();
        assert!(echo.path.is_absolute() && echo.path.ends_with("echo"));
        let again = resolve_allowed(echo.path.to_str().unwrap(), None, &ws).unwrap();
        assert_eq!(
            again.path, echo.path,
            "an absolute entry resolves to the same file"
        );
        assert!(resolve_allowed("no-such-program-xyz", path_var.as_deref(), &ws).is_err());
        assert!(resolve_allowed("bin/echo", path_var.as_deref(), &ws).is_err());
        assert!(resolve_allowed("", path_var.as_deref(), &ws).is_err());
    }

    /// A program the plan could rewrite (inside the workspace, directly, through a PATH entry or
    /// through a symlink) is never allowed; relative PATH entries are not searched.
    #[test]
    fn an_allowlist_entry_never_resolves_inside_the_workspace() {
        let ws = scratch("ws-programs");
        fs::create_dir(ws.join("bin")).unwrap();
        fs::write(ws.join("bin/tool"), "#!/bin/sh\n").unwrap();
        let bin = OsString::from(ws.join("bin").as_str());
        assert!(
            resolve_allowed("tool", Some(&bin), &ws).is_err(),
            "through PATH"
        );
        assert!(
            resolve_allowed(ws.join("bin/tool").as_str(), None, &ws).is_err(),
            "absolute"
        );
        let outside = scratch("ws-programs-out");
        std::os::unix::fs::symlink(ws.join("bin/tool"), outside.join("tool")).unwrap();
        assert!(
            resolve_allowed(outside.join("tool").as_str(), None, &ws).is_err(),
            "a link from outside into the workspace"
        );
        let relative = OsString::from("bin");
        assert!(
            resolve_allowed("tool", Some(&relative), &ws).is_err(),
            "a relative PATH entry"
        );
        // A relative entry that does resolve from this process's directory to a real program
        // outside the workspace is still not searched.
        let tools = scratch("relative-tools");
        fs::write(tools.join("tool"), "#!/bin/sh\n").unwrap();
        let cwd = std::env::current_dir().unwrap();
        let up = "../".repeat(cwd.components().count().saturating_sub(1));
        let relative = OsString::from(format!("{up}{}", tools.as_str().trim_start_matches('/')));
        assert!(
            Path::new(&relative).join("tool").is_file(),
            "the relative entry does resolve"
        );
        assert!(
            resolve_allowed("tool", Some(&relative), &elsewhere()).is_err(),
            "but relative entries are never searched"
        );
    }

    #[test]
    fn admission_is_by_name_or_resolved_path_never_by_base_name() {
        let path_var = std::env::var_os("PATH");
        let allowed = [resolve_allowed("echo", path_var.as_deref(), &elsewhere()).unwrap()];
        let echo = allowed[0].path.clone();
        assert_eq!(admit(&allowed, "echo"), Some(echo.as_path()));
        assert_eq!(
            admit(&allowed, echo.to_str().unwrap()),
            Some(echo.as_path())
        );
        let planted_dir = scratch("planted");
        fs::write(planted_dir.join("echo"), "#!/bin/sh\n").unwrap();
        assert_eq!(admit(&allowed, planted_dir.join("echo").as_str()), None);
        assert_eq!(admit(&allowed, "./echo"), None);
        assert_eq!(admit(&allowed, "cat"), None);
    }

    #[test]
    fn the_plan_sets_only_allowed_variables_and_never_dangerous_ones() {
        let allowed: Vec<String> = [
            "HOME",
            "RUST_LOG",
            "PATH",
            "LD_PRELOAD",
            "BASH_FUNC_ls%%",
            "PYTHONUSERBASE",
            "PERLLIB",
            "RUBYLIB",
            "NODE_PATH",
            "BASHOPTS",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        for key in ["HOME", "RUST_LOG"] {
            assert!(check_env_key(key, &allowed).is_ok(), "{key}");
        }
        assert!(check_env_key("TERM", &allowed).is_err(), "not on the list");
        for key in [
            "PATH",
            "LD_PRELOAD",
            "LD_LIBRARY_PATH",
            "DYLD_INSERT_LIBRARIES",
            "BASH_FUNC_ls%%",
            "BASH_ENV",
            "GCONV_PATH",
            "GIT_CONFIG_COUNT",
            "GIT_SSH_COMMAND",
            "PYTHONUSERBASE",
            "PERLLIB",
            "RUBYLIB",
            "NODE_PATH",
            "BASHOPTS",
            "",
            "A=B",
            "A\0B",
        ] {
            assert!(
                check_env_key(key, &allowed).is_err(),
                "{key:?} even when listed"
            );
        }
    }

    #[test]
    fn a_childs_path_keeps_only_absolute_directories_outside_the_workspace() {
        let ws = scratch("childpath");
        fs::create_dir(ws.join("bin")).unwrap();
        let outside = scratch("childpath-out");
        let path_var = std::env::join_paths([
            PathBuf::from(outside.as_str()),
            PathBuf::from(ws.join("bin").as_str()),
            PathBuf::from("relative/bin"),
        ])
        .unwrap();
        assert_eq!(
            child_path(Some(&path_var), &ws),
            OsString::from(outside.as_str())
        );
    }

    #[test]
    fn timeouts_and_reads_are_bounded() {
        assert_eq!(proc_timeout(None), DEFAULT_PROC_TIMEOUT);
        assert_eq!(proc_timeout(Some(250)), Duration::from_millis(250));
        assert_eq!(proc_timeout(Some(u64::MAX)), MAX_PROC_TIMEOUT);
        assert_eq!(cap_read(10), 10);
        assert_eq!(cap_read(u64::MAX), MAX_READ_BYTES);
    }

    // ── Bounded processes ───────────────────────────────────────────────────

    fn sh(script: &str) -> Command {
        let mut cmd = Command::new("/bin/sh");
        cmd.args(["-c", script]);
        cmd
    }

    #[test]
    fn a_bounded_process_reports_its_exit_and_output() {
        let out =
            run_bounded(sh("echo out; echo err >&2; exit 3"), Duration::from_secs(5)).unwrap();
        assert_eq!(out.status, Some(3));
        assert_eq!(out.stdout, b"out\n");
        assert_eq!(out.stderr, b"err\n");
        assert!(!out.timed_out && !out.stdout_truncated);
    }

    /// Whether the process `pid` is gone (or a zombie) within two seconds.
    fn gone(pid: &str) -> bool {
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            match fs::read_to_string(format!("/proc/{}/stat", pid.trim())) {
                Err(_) => return true,
                Ok(stat) => {
                    let state = stat.rsplit_once(')').map(|(_, rest)| rest.trim_start());
                    if state.is_some_and(|r| r.starts_with('Z')) {
                        return true;
                    }
                }
            }
            thread::sleep(Duration::from_millis(20));
        }
        false
    }

    #[test]
    fn a_child_left_holding_the_output_does_not_hold_the_caller() {
        let started = Instant::now();
        let out = run_bounded(sh("sleep 30 & echo $!"), Duration::from_secs(10)).unwrap();
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "{:?}",
            started.elapsed()
        );
        assert_eq!(out.status, Some(0));
        assert!(
            gone(&String::from_utf8_lossy(&out.stdout)),
            "and it is dead"
        );
    }

    #[test]
    fn output_exactly_at_the_cap_is_not_truncated() {
        let exact = run_bounded(sh("head -c 1048576 /dev/zero"), Duration::from_secs(10)).unwrap();
        assert_eq!(exact.stdout.len(), MAX_OUTPUT_BYTES);
        assert!(!exact.stdout_truncated);
        let over = run_bounded(sh("head -c 1048577 /dev/zero"), Duration::from_secs(10)).unwrap();
        assert_eq!(over.stdout.len(), MAX_OUTPUT_BYTES);
        assert!(over.stdout_truncated);
    }

    // ── Navigation ──────────────────────────────────────────────────────────

    #[test]
    fn navigation_needs_http_and_an_allowed_host() {
        let allowed = vec!["login.example.test".to_string(), "*.docs.test".to_string()];
        assert!(check_navigation("http://login.example.test/", &allowed).is_ok());
        assert!(check_navigation("https://LOGIN.example.test:8443/a?b", &allowed).is_ok());
        assert!(check_navigation("https://api.docs.test/", &allowed).is_ok());
        assert!(
            check_navigation("https://docs.test/", &allowed).is_err(),
            "the apex of a wildcard"
        );
        assert!(check_navigation("http://evil.example.test/", &allowed).is_err());
        assert!(check_navigation("http://login.example.test.evil.test/", &allowed).is_err());
        assert!(check_navigation("file:///etc/passwd", &allowed).is_err());
        assert!(check_navigation("javascript:alert(1)", &allowed).is_err());
        assert!(check_navigation("not a url", &allowed).is_err());
        assert!(
            check_navigation("http://login.example.test/", &[]).is_err(),
            "deny by default"
        );
    }
}
