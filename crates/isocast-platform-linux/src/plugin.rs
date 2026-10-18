//! Running process plugins: spawning them in a Landlock sandbox, and
//! telling which process is at the other end of a socket connection.
//!
//! The sandbox needs Landlock ABI 10 (Linux 7.0): ABI 9 is the first that
//! controls connecting to pathname Unix sockets (PipeWire, the D-Bus session
//! bus, Wayland, X11), and ABI 10 the first that controls UDP. On an older
//! kernel, or with Landlock off, plugins run unsandboxed and `doctor` says so.
//!
//! The ruleset is built in isocastd, before the fork; the child only sets
//! `no_new_privs` and restricts itself, which are async-signal-safe.

use std::ffi::c_long;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// The Landlock ABI the sandbox needs.
pub const MIN_ABI: i64 = 10;

const ACCESS_FS_EXECUTE: u64 = 1 << 0;
const ACCESS_FS_WRITE_FILE: u64 = 1 << 1;
const ACCESS_FS_READ_FILE: u64 = 1 << 2;
const ACCESS_FS_READ_DIR: u64 = 1 << 3;
const ACCESS_FS_REMOVE_DIR: u64 = 1 << 4;
const ACCESS_FS_REMOVE_FILE: u64 = 1 << 5;
const ACCESS_FS_MAKE_DIR: u64 = 1 << 7;
const ACCESS_FS_MAKE_REG: u64 = 1 << 8;
const ACCESS_FS_MAKE_SOCK: u64 = 1 << 9;
const ACCESS_FS_MAKE_FIFO: u64 = 1 << 10;
const ACCESS_FS_MAKE_SYM: u64 = 1 << 12;
const ACCESS_FS_REFER: u64 = 1 << 13;
const ACCESS_FS_TRUNCATE: u64 = 1 << 14;
const ACCESS_FS_IOCTL_DEV: u64 = 1 << 15;
const ACCESS_FS_RESOLVE_UNIX: u64 = 1 << 16;
/// Every filesystem right up to ABI 10: all are denied unless a rule
/// allows them.
const ACCESS_FS_ALL: u64 = (1 << 17) - 1;
/// The rights a rule on a file (not a folder) may carry.
const ACCESS_FS_FILE: u64 = ACCESS_FS_EXECUTE
    | ACCESS_FS_WRITE_FILE
    | ACCESS_FS_READ_FILE
    | ACCESS_FS_TRUNCATE
    | ACCESS_FS_IOCTL_DEV;
const ACCESS_FS_READ: u64 = ACCESS_FS_READ_FILE | ACCESS_FS_READ_DIR;
const ACCESS_FS_WRITE: u64 = ACCESS_FS_READ
    | ACCESS_FS_WRITE_FILE
    | ACCESS_FS_REMOVE_DIR
    | ACCESS_FS_REMOVE_FILE
    | ACCESS_FS_MAKE_DIR
    | ACCESS_FS_MAKE_REG
    | ACCESS_FS_MAKE_SOCK
    | ACCESS_FS_MAKE_FIFO
    | ACCESS_FS_MAKE_SYM
    | ACCESS_FS_REFER
    | ACCESS_FS_TRUNCATE;

const ACCESS_NET_CONNECT_TCP: u64 = 1 << 1;
const ACCESS_NET_CONNECT_SEND_UDP: u64 = 1 << 3;

const SCOPE_ABSTRACT_UNIX_SOCKET: u64 = 1 << 0;
const SCOPE_SIGNAL: u64 = 1 << 1;

const CREATE_RULESET_VERSION: u32 = 1 << 0;
const RULE_PATH_BENEATH: u32 = 1;

#[repr(C)]
struct RulesetAttr {
    handled_access_fs: u64,
    handled_access_net: u64,
    scoped: u64,
}

#[repr(C, packed)]
struct PathBeneathAttr {
    allowed_access: u64,
    parent_fd: i32,
}

/// Folders every program needs to read and run: the system's programs,
/// libraries and settings, and process and device information.
const SYSTEM_READ_EXEC: &[&str] = &[
    "/usr", "/bin", "/sbin", "/lib", "/lib32", "/lib64", "/opt", "/nix", "/gnu",
];
const SYSTEM_READ: &[&str] = &["/etc", "/proc", "/sys", "/run/systemd/resolve"];
/// Device files a plugin may use. Not cameras (`/dev/video*`), sound
/// (`/dev/snd`) or input devices.
const DEVICES_READ: &[&str] = &["/dev/zero", "/dev/random", "/dev/urandom"];
const DEVICES_WRITE: &[&str] = &["/dev/null"];

/// Whether plugins can be sandboxed on this machine.
pub fn sandbox_available() -> bool {
    abi() >= MIN_ABI
}

/// The kernel's Landlock ABI version, or 0 when Landlock is missing or off.
pub fn abi() -> i64 {
    static ABI: OnceLock<i64> = OnceLock::new();
    *ABI.get_or_init(|| {
        // SAFETY: with a null attribute and size 0, this only asks for the
        // version.
        let version = unsafe {
            libc::syscall(
                libc::SYS_landlock_create_ruleset,
                std::ptr::null::<RulesetAttr>(),
                0usize,
                CREATE_RULESET_VERSION,
            )
        };
        version.max(0)
    })
}

/// What a plugin's process may touch. Everything not listed is denied:
/// the home folder, other users' files, `/tmp`, cameras, microphones and
/// sound devices, and every Unix socket but `socket` (so no PipeWire, D-Bus
/// session bus, Wayland or X11: no screen, audio, portals, secrets or
/// clipboard).
#[derive(Debug, Clone, Default)]
pub struct Sandbox {
    /// Folders and files it may read: its own folders and the files it was
    /// granted.
    pub readable: Vec<PathBuf>,
    /// Folders it may write in (its data folder).
    pub writable: Vec<PathBuf>,
    /// Programs it may run, and folders whose programs it may run, besides
    /// the system's.
    pub programs: Vec<PathBuf>,
    pub program_folders: Vec<PathBuf>,
    /// The socket it reaches isocastd on: the only one it may connect to.
    /// Allowed by its folder, which holds only the plugin's own files.
    pub socket: PathBuf,
    /// It may connect anywhere on the network. Without it, it can only
    /// serve (bind and accept TCP), and reach isocastd on its socket.
    pub network: bool,
    /// The recordings folders, readable when `recordings_read`. Landlock
    /// can't allow creating files without allowing changing existing ones,
    /// so a plugin granted writing gets reading only.
    pub recordings: Vec<PathBuf>,
    pub recordings_read: bool,
}

/// A Landlock ruleset ready for a child process to restrict itself with.
pub struct Ruleset(OwnedFd);

impl Sandbox {
    /// Build the ruleset. Paths that don't exist are left out.
    pub fn ruleset(&self) -> io::Result<Ruleset> {
        let attr = RulesetAttr {
            handled_access_fs: ACCESS_FS_ALL,
            handled_access_net: if self.network {
                0
            } else {
                ACCESS_NET_CONNECT_TCP | ACCESS_NET_CONNECT_SEND_UDP
            },
            scoped: SCOPE_ABSTRACT_UNIX_SOCKET | SCOPE_SIGNAL,
        };
        // SAFETY: `attr` is a valid ruleset attribute of the size given.
        let fd = unsafe {
            libc::syscall(
                libc::SYS_landlock_create_ruleset,
                &attr as *const RulesetAttr,
                std::mem::size_of::<RulesetAttr>(),
                0u32,
            )
        };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: a new descriptor (close-on-exec) that only we own.
        let ruleset = Ruleset(unsafe { OwnedFd::from_raw_fd(fd as RawFd) });
        let exec = ACCESS_FS_READ | ACCESS_FS_EXECUTE;
        for path in SYSTEM_READ_EXEC {
            ruleset.allow(Path::new(path), exec)?;
        }
        for path in SYSTEM_READ {
            ruleset.allow(Path::new(path), ACCESS_FS_READ)?;
        }
        for path in DEVICES_READ {
            ruleset.allow(Path::new(path), ACCESS_FS_READ_FILE)?;
        }
        for path in DEVICES_WRITE {
            ruleset.allow(Path::new(path), ACCESS_FS_READ_FILE | ACCESS_FS_WRITE_FILE)?;
        }
        for path in &self.readable {
            ruleset.allow(path, ACCESS_FS_READ)?;
        }
        for path in &self.writable {
            ruleset.allow(path, ACCESS_FS_WRITE)?;
        }
        for path in self.programs.iter().chain(&self.program_folders) {
            ruleset.allow(path, exec)?;
        }
        if self.recordings_read {
            for path in &self.recordings {
                ruleset.allow(path, ACCESS_FS_READ)?;
            }
        }
        if let Some(folder) = self.socket.parent() {
            ruleset.allow(folder, ACCESS_FS_READ | ACCESS_FS_RESOLVE_UNIX)?;
        }
        Ok(ruleset)
    }
}

impl Ruleset {
    /// Allow `access` beneath `path` (on a file, only the rights a file can
    /// have). Nothing for a path that doesn't exist.
    fn allow(&self, path: &Path, access: u64) -> io::Result<()> {
        let Ok(file) = std::fs::File::options()
            .read(true)
            .custom_flags(libc::O_PATH | libc::O_CLOEXEC)
            .open(path)
        else {
            return Ok(());
        };
        let access = if file.metadata()?.is_dir() {
            access
        } else {
            access & ACCESS_FS_FILE
        };
        let rule = PathBeneathAttr {
            allowed_access: access,
            parent_fd: file.as_raw_fd(),
        };
        // SAFETY: a valid path-beneath rule; both descriptors are open.
        let code = unsafe {
            libc::syscall(
                libc::SYS_landlock_add_rule,
                self.0.as_raw_fd(),
                RULE_PATH_BENEATH,
                &rule as *const PathBeneathAttr,
                0u32,
            )
        };
        if code < 0 {
            let err = io::Error::last_os_error();
            return Err(io::Error::new(
                err.kind(),
                format!("landlock rule for {}: {err}", path.display()),
            ));
        }
        Ok(())
    }
}

/// How to start a plugin's process.
pub struct Spawn<'a> {
    /// The program, as an absolute path.
    pub program: &'a Path,
    /// Its arguments (not the program itself).
    pub args: &'a [String],
    /// Its whole environment.
    pub env: &'a [(String, String)],
    /// The folder it runs in.
    pub cwd: &'a Path,
    /// Where its output goes (appended).
    pub log: &'a Path,
    /// Run it in this sandbox; unsandboxed if absent.
    pub sandbox: Option<&'a Sandbox>,
}

/// Start a plugin's process: in its own process group (so stopping it
/// stops what it started), with only stdin (/dev/null), stdout and stderr
/// (the log) open, and, when sandboxed, `no_new_privs` and the Landlock
/// ruleset applied before it runs. Returns its process id.
pub fn spawn(spawn: &Spawn) -> io::Result<u32> {
    let log = isocast_diagnostics::output::plugin_pipe(spawn.log)?;
    let ruleset = spawn.sandbox.map(Sandbox::ruleset).transpose()?;
    let mut command = std::process::Command::new(spawn.program);
    command
        .args(spawn.args)
        .env_clear()
        .envs(spawn.env.iter().map(|(k, v)| (k, v)))
        .current_dir(spawn.cwd)
        .stdin(std::process::Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log)
        .process_group(0);
    if let Some(ruleset) = &ruleset {
        let fd = ruleset.0.as_raw_fd();
        // SAFETY: the closure makes only two system calls, both
        // async-signal-safe, and touches no memory but its captured fd.
        unsafe {
            command.pre_exec(move || {
                if libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1 as c_long, 0, 0, 0) != 0 {
                    return Err(io::Error::last_os_error());
                }
                if libc::syscall(libc::SYS_landlock_restrict_self, fd, 0u32) != 0 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
    }
    let child = command.spawn()?;
    Ok(child.id())
}

/// The process at the other end of a Unix socket connection.
pub fn peer_pid(fd: RawFd) -> Option<u32> {
    let mut cred = libc::ucred {
        pid: 0,
        uid: 0,
        gid: 0,
    };
    let mut size = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    // SAFETY: getsockopt writes at most `size` bytes into `cred`.
    let code = unsafe {
        libc::getsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&mut cred as *mut libc::ucred).cast(),
            &mut size,
        )
    };
    (code == 0 && cred.pid > 0).then_some(cred.pid as u32)
}

/// A process's parent, and when it started (clock ticks since boot, and
/// 0), from `/proc/<pid>/stat`: together with the id, the process's
/// identity.
pub fn process_info(pid: u32) -> Option<(u32, (u64, u64))> {
    let stat = std::fs::read(format!("/proc/{pid}/stat")).ok()?;
    parse_stat(&stat)
}

/// Fields after the command name, which may itself hold `)` and spaces:
/// state, parent, … start time (the 20th).
fn parse_stat(stat: &[u8]) -> Option<(u32, (u64, u64))> {
    let close = stat.iter().rposition(|&b| b == b')')?;
    let rest = std::str::from_utf8(&stat[close + 1..]).ok()?;
    let fields: Vec<&str> = rest.split_whitespace().collect();
    let parent = fields.get(1)?.parse().ok()?;
    let started = fields.get(19)?.parse().ok()?;
    Some((parent, (started, 0)))
}

/// A path with `~/` resolved against `home`, as a plugin's granted files
/// are written.
pub fn expand_home(file: &str, home: &Path) -> PathBuf {
    match file.strip_prefix("~/") {
        Some(rest) => home.join(rest),
        None => PathBuf::from(file),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stat_survives_odd_command_names() {
        let stat =
            b"4242 (a) b (c) S 17 4242 4242 0 -1 4194560 100 0 0 0 1 2 0 0 20 0 1 0 987654 1000 10";
        assert_eq!(parse_stat(stat), Some((17, (987654, 0))));
    }
}
