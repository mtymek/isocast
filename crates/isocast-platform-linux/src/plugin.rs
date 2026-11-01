//! Running process plugins: telling which process is at the other end of a
//! socket connection, so isocastd can hold a plugin's connections to its
//! grant.

use std::os::fd::RawFd;

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stat_survives_odd_command_names() {
        let stat =
            b"4242 (a) b (c) S 17 4242 4242 0 -1 4194560 100 0 0 0 1 2 0 0 20 0 1 0 987654 1000 10";
        assert_eq!(parse_stat(stat), Some((17, (987654, 0))));
    }

    #[test]
    fn this_process_has_an_identity() {
        let (parent, (started, _)) = process_info(std::process::id()).expect("own /proc stat");
        assert!(parent > 0);
        assert!(started > 0);
    }

    #[test]
    fn a_socket_pair_reports_this_process() {
        use std::os::fd::AsRawFd;
        let (a, _b) = std::os::unix::net::UnixStream::pair().unwrap();
        assert_eq!(peer_pid(a.as_raw_fd()), Some(std::process::id()));
    }
}
