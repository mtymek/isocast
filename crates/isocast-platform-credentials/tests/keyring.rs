//! The Linux secret store: the desktop keyring through the Secret Service,
//! and the plaintext files it falls back to.
//!
//! Each case runs this test binary again as a child with an environment of
//! its own (a scratch `HOME` and `XDG_CONFIG_HOME`, and a private or missing
//! session bus), since changing the environment of a threaded test process
//! is unsafe. The child does the work in `child_entry` and panics on a
//! failed check.
//!
//! The cases marked `#[ignore]` start their own D-Bus session with
//! `dbus-run-session`, and a throwaway `gnome-keyring-daemon` or
//! `dbus-test-tool`; they never touch the person's own keyring. Run them
//! with `cargo test -p isocast-platform-credentials --test keyring --
//! --include-ignored`.
#![cfg(target_os = "linux")]

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use isocast_platform_credentials::{delete_secret, has_secret, secret, set_secret};

const ROLE: &str = "ISOCAST_KEYRING_TEST_ROLE";
const ACCOUNT: &str = "test-account";
const VALUE: &str = "live_0123456789abcdef";

/// The service name every case uses: unique to this run, so a leftover
/// item can never be mistaken for a real one.
fn service() -> String {
    std::env::var("ISOCAST_KEYRING_TEST_SERVICE").expect("set by the parent")
}

fn files(config: &Path) -> Vec<PathBuf> {
    match std::fs::read_dir(config.join("isocast/credentials")) {
        Ok(dir) => dir
            .filter_map(|entry| entry.ok().map(|entry| entry.path()))
            .filter(|path| path.is_file())
            .collect(),
        Err(_) => Vec::new(),
    }
}

fn config() -> PathBuf {
    PathBuf::from(std::env::var_os("XDG_CONFIG_HOME").expect("set by the parent"))
}

fn lookup(service: &str) -> String {
    let out = Command::new("secret-tool")
        .args(["lookup", "application", "isocast", "service", service])
        .args(["account", ACCOUNT])
        .output()
        .expect("secret-tool");
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// The child side. Passes trivially when this binary runs as an ordinary
/// test.
#[test]
fn child_entry() {
    let Ok(role) = std::env::var(ROLE) else {
        return;
    };
    let service = service();
    match role.as_str() {
        "files" => {
            set_secret(&service, ACCOUNT, VALUE).unwrap();
            let kept = files(&config());
            assert_eq!(kept.len(), 1, "one plaintext file: {kept:?}");
            let mode = std::fs::metadata(&kept[0]).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
            assert_eq!(secret(&service, ACCOUNT).unwrap().as_deref(), Some(VALUE));
            assert!(has_secret(&service, ACCOUNT).unwrap());
            assert!(delete_secret(&service, ACCOUNT).unwrap());
            assert!(!has_secret(&service, ACCOUNT).unwrap());
            assert!(files(&config()).is_empty());
        }
        "keyring" => {
            set_secret(&service, ACCOUNT, VALUE).unwrap();
            assert!(
                files(&config()).is_empty(),
                "no plaintext file with a keyring"
            );
            assert_eq!(lookup(&service), VALUE, "secret-tool sees the item");
            assert_eq!(secret(&service, ACCOUNT).unwrap().as_deref(), Some(VALUE));
            assert!(has_secret(&service, ACCOUNT).unwrap());
            assert!(delete_secret(&service, ACCOUNT).unwrap());
            assert_eq!(lookup(&service), "");
            assert_eq!(secret(&service, ACCOUNT).unwrap(), None);
        }
        "write-file" => {
            set_secret(&service, ACCOUNT, VALUE).unwrap();
            assert_eq!(files(&config()).len(), 1);
        }
        "adopt" => {
            assert_eq!(
                files(&config()).len(),
                1,
                "the file from before the keyring"
            );
            assert_eq!(secret(&service, ACCOUNT).unwrap().as_deref(), Some(VALUE));
            assert!(files(&config()).is_empty(), "the file is gone once adopted");
            assert_eq!(lookup(&service), VALUE);
            assert!(delete_secret(&service, ACCOUNT).unwrap());
        }
        "hung" => {
            let first = Instant::now();
            assert!(!has_secret(&service, ACCOUNT).unwrap());
            let first = first.elapsed();
            assert!(
                first >= Duration::from_secs(4) && first < Duration::from_secs(12),
                "the first call waits for the time limit: {first:?}"
            );
            let second = Instant::now();
            set_secret(&service, ACCOUNT, VALUE).unwrap();
            assert_eq!(secret(&service, ACCOUNT).unwrap().as_deref(), Some(VALUE));
            let second = second.elapsed();
            assert!(
                second < Duration::from_secs(1),
                "later calls skip the keyring: {second:?}"
            );
            assert_eq!(files(&config()).len(), 1, "kept in a file meanwhile");
            println!("hung keyring: first call {first:?}, next two {second:?}");
        }
        other => panic!("unknown role {other}"),
    }
}

struct Scratch(PathBuf);

impl Scratch {
    fn new(name: &str) -> Self {
        let root =
            std::env::temp_dir().join(format!("isocast-keyring-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        for dir in ["home", "config", "data", "runtime"] {
            std::fs::create_dir_all(root.join(dir)).unwrap();
        }
        std::fs::set_permissions(root.join("runtime"), std::fs::Permissions::from_mode(0o700))
            .unwrap();
        Self(root)
    }

    /// A command that runs `role` in the child, with this scratch as its
    /// world. `wrap` is a shell line that ends by running "$@".
    fn child(&self, role: &str, wrap: Option<&str>, bus: Option<&str>) -> Command {
        let exe = std::env::current_exe().unwrap();
        let mut command = match wrap {
            Some(line) => {
                let mut c = Command::new("dbus-run-session");
                c.args(["--", "sh", "-c", line, "sh"]).arg(&exe);
                c
            }
            None => Command::new(&exe),
        };
        command
            .args(["--exact", "child_entry", "--nocapture", "--test-threads=1"])
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("HOME", self.0.join("home"))
            .env("XDG_CONFIG_HOME", self.0.join("config"))
            .env("XDG_DATA_HOME", self.0.join("data"))
            .env("XDG_RUNTIME_DIR", self.0.join("runtime"))
            .env(
                "ISOCAST_KEYRING_TEST_SERVICE",
                format!("isocast-test-{}", std::process::id()),
            )
            .env(ROLE, role);
        if let Some(bus) = bus {
            command.env("DBUS_SESSION_BUS_ADDRESS", bus);
        }
        command
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn run(mut command: Command) {
    let out = command.output().expect("start the child");
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    println!("{text}");
    assert!(out.status.success(), "child failed: {}\n{text}", out.status);
}

/// A throwaway keyring on the private bus, unlocked with a fixed password.
const WITH_KEYRING: &str = "printf test | gnome-keyring-daemon --unlock --components=secrets \
                            >/dev/null && exec \"$@\"";
/// A name owner on the private bus that takes every call and never answers.
const WITH_HUNG_KEYRING: &str = "dbus-test-tool black-hole --session \
                                 --name=org.freedesktop.secrets & sleep 0.5; exec \"$@\"";

#[test]
fn without_a_session_bus_secrets_are_owner_only_files() {
    let scratch = Scratch::new("files");
    run(scratch.child("files", None, Some("unix:path=/nonexistent")));
}

#[test]
#[ignore = "starts dbus-run-session and gnome-keyring-daemon"]
fn with_a_keyring_secrets_never_reach_a_file() {
    let scratch = Scratch::new("keyring");
    run(scratch.child("keyring", Some(WITH_KEYRING), None));
}

#[test]
#[ignore = "starts dbus-run-session and gnome-keyring-daemon"]
fn a_secret_kept_in_a_file_moves_into_the_keyring() {
    let scratch = Scratch::new("adopt");
    run(scratch.child("write-file", None, Some("unix:path=/nonexistent")));
    run(scratch.child("adopt", Some(WITH_KEYRING), None));
}

#[test]
#[ignore = "starts dbus-run-session and dbus-test-tool; takes about 5 s"]
fn a_hung_keyring_is_skipped_after_its_time_limit() {
    let scratch = Scratch::new("hung");
    run(scratch.child("hung", Some(WITH_HUNG_KEYRING), None));
}
