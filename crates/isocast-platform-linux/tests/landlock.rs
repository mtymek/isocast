//! What a sandboxed plugin process can and cannot do. Each case starts this
//! test binary again as the "plugin", inside a Landlock sandbox, and reads
//! its exit code: 0 when the action worked, 3 when it was refused.
//!
//! Needs Landlock ABI 10 (Linux 7.0); on an older kernel every case prints
//! why it was skipped and passes.
#![cfg(target_os = "linux")]

use std::io::Write;
use std::net::{TcpListener, UdpSocket};
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};

use isocast_platform_linux::plugin::{self, Sandbox, Spawn};

const ROLE: &str = "ISOCAST_LANDLOCK_PROBE";
const TARGET: &str = "ISOCAST_LANDLOCK_TARGET";
const ALLOWED: i32 = 0;
const DENIED: i32 = 3;

/// The "plugin" side: does one action and exits. Passes trivially when this
/// binary runs as an ordinary test.
#[test]
fn probe_entry() {
    let Ok(role) = std::env::var(ROLE) else {
        return;
    };
    let target = std::env::var(TARGET).unwrap_or_default();
    let result: std::io::Result<()> = match role.as_str() {
        "read" => std::fs::read(&target).map(drop),
        "write" => std::fs::write(&target, b"probe"),
        "unix" => std::os::unix::net::UnixStream::connect(&target).map(drop),
        "tcp" => std::net::TcpStream::connect(&target).map(drop),
        "udp" => {
            UdpSocket::bind("127.0.0.1:0").and_then(|s| s.send_to(b"probe", &target).map(drop))
        }
        "signal" => {
            // SAFETY: SIGCONT to the test process that started us is harmless.
            let code = unsafe { libc::kill(libc::getppid(), libc::SIGCONT) };
            if code == 0 {
                Ok(())
            } else {
                Err(std::io::Error::last_os_error())
            }
        }
        other => panic!("unknown probe {other}"),
    };
    let _ = writeln!(std::io::stdout(), "probe {role} {target}: {result:?}");
    std::process::exit(if result.is_ok() { ALLOWED } else { DENIED });
}

fn ready() -> bool {
    let abi = plugin::abi();
    if abi < plugin::MIN_ABI {
        eprintln!(
            "skipped: Landlock ABI {abi}, the sandbox needs {}",
            plugin::MIN_ABI
        );
        return false;
    }
    true
}

/// A scratch layout outside every sandbox rule: /tmp is not allowed.
struct Scratch {
    root: PathBuf,
}

impl Scratch {
    fn new(name: &str) -> Self {
        let root =
            std::env::temp_dir().join(format!("isocast-landlock-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        for dir in ["home", "plugin", "data", "run"] {
            std::fs::create_dir_all(root.join(dir)).unwrap();
        }
        std::fs::write(root.join("home/secret"), b"secret").unwrap();
        std::fs::write(root.join("plugin/readme"), b"mine").unwrap();
        Self { root }
    }

    fn path(&self, rest: &str) -> PathBuf {
        self.root.join(rest)
    }

    fn sandbox(&self, network: bool) -> Sandbox {
        Sandbox {
            readable: vec![self.path("plugin")],
            writable: vec![self.path("data")],
            programs: vec![std::env::current_exe().unwrap()],
            program_folders: Vec::new(),
            socket: self.path("run/socket"),
            network,
            recordings: Vec::new(),
            recordings_read: false,
        }
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// Runs one probe in `sandbox` and returns its exit code.
fn probe(scratch: &Scratch, sandbox: &Sandbox, role: &str, target: &str) -> i32 {
    run(scratch, Some(sandbox), role, target)
}

/// Runs one probe, sandboxed or not, and returns its exit code.
fn run(scratch: &Scratch, sandbox: Option<&Sandbox>, role: &str, target: &str) -> i32 {
    let program = std::env::current_exe().unwrap();
    let args = [
        "--exact".to_string(),
        "probe_entry".into(),
        "--nocapture".into(),
        "--test-threads=1".into(),
    ];
    let env = [
        (ROLE.to_string(), role.to_string()),
        (TARGET.to_string(), target.to_string()),
    ];
    let log = scratch.path(&format!("{role}.log"));
    let pid = plugin::spawn(&Spawn {
        program: &program,
        args: &args,
        env: &env,
        cwd: &scratch.path("data"),
        log: &log,
        sandbox,
    })
    .expect("spawn the probe");
    let mut status = 0;
    // SAFETY: waits for the child we just started.
    let waited = unsafe { libc::waitpid(pid as i32, &mut status, 0) };
    assert_eq!(
        waited,
        pid as i32,
        "waitpid: {}",
        std::io::Error::last_os_error()
    );
    assert!(
        libc::WIFEXITED(status),
        "probe {role} did not exit normally: {status}"
    );
    libc::WEXITSTATUS(status)
}

fn text(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

#[test]
fn files_outside_the_sandbox_are_unreadable() {
    if !ready() {
        return;
    }
    let s = Scratch::new("files");
    let sandbox = s.sandbox(false);
    assert_eq!(
        probe(&s, &sandbox, "read", &text(&s.path("home/secret"))),
        DENIED
    );
    assert_eq!(
        probe(&s, &sandbox, "read", &text(&s.path("plugin/readme"))),
        ALLOWED
    );
    assert_eq!(
        probe(&s, &sandbox, "write", &text(&s.path("data/out"))),
        ALLOWED
    );
    assert_eq!(
        probe(&s, &sandbox, "write", &text(&s.path("plugin/out"))),
        DENIED
    );
    assert_eq!(
        probe(&s, &sandbox, "write", &text(&s.path("home/out"))),
        DENIED
    );
}

/// The probe itself works: without a sandbox, the same actions succeed.
#[test]
fn without_the_sandbox_everything_is_reachable() {
    if !ready() {
        return;
    }
    let s = Scratch::new("control");
    assert_eq!(
        run(&s, None, "read", &text(&s.path("home/secret"))),
        ALLOWED
    );
    assert_eq!(run(&s, None, "signal", ""), ALLOWED);
}

#[test]
fn only_its_own_socket_can_be_reached() {
    if !ready() {
        return;
    }
    let s = Scratch::new("sockets");
    let _own = UnixListener::bind(s.path("run/socket")).unwrap();
    let _other = UnixListener::bind(s.path("home/other.sock")).unwrap();
    let sandbox = s.sandbox(false);
    assert_eq!(
        probe(&s, &sandbox, "unix", &text(&s.path("run/socket"))),
        ALLOWED
    );
    assert_eq!(
        probe(&s, &sandbox, "unix", &text(&s.path("home/other.sock"))),
        DENIED
    );
}

#[test]
fn network_needs_a_grant() {
    if !ready() {
        return;
    }
    let s = Scratch::new("network");
    let tcp = TcpListener::bind("127.0.0.1:0").unwrap();
    let udp = UdpSocket::bind("127.0.0.1:0").unwrap();
    let tcp_addr = tcp.local_addr().unwrap().to_string();
    let udp_addr = udp.local_addr().unwrap().to_string();
    let closed = s.sandbox(false);
    assert_eq!(probe(&s, &closed, "tcp", &tcp_addr), DENIED);
    assert_eq!(probe(&s, &closed, "udp", &udp_addr), DENIED);
    let open = s.sandbox(true);
    assert_eq!(probe(&s, &open, "tcp", &tcp_addr), ALLOWED);
    assert_eq!(probe(&s, &open, "udp", &udp_addr), ALLOWED);
}

#[test]
fn processes_outside_the_sandbox_get_no_signals() {
    if !ready() {
        return;
    }
    let s = Scratch::new("signals");
    assert_eq!(probe(&s, &s.sandbox(false), "signal", ""), DENIED);
}
