//! Facts about this machine and its disks, from `/proc`, `/etc` and the
//! kernel.

use std::ffi::{CStr, CString};
use std::mem::MaybeUninit;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

use isocast_engine::Volume;
use isocast_protocol::api::Machine;

/// The CPU, memory, OS and hardware encoders.
pub fn machine() -> Machine {
    let cpuinfo = std::fs::read_to_string("/proc/cpuinfo").unwrap_or_default();
    let meminfo = std::fs::read_to_string("/proc/meminfo").unwrap_or_default();
    let os_release = std::fs::read_to_string("/etc/os-release").unwrap_or_default();
    Machine {
        chip: cpu_name(&cpuinfo)
            .unwrap_or_else(|| format!("{} (unknown CPU)", std::env::consts::ARCH)),
        // 0 means unknown.
        memory_gb: memory_kib(&meminfo).map_or(0.0, |kib| kib as f64 / (1024.0 * 1024.0)),
        cores: std::thread::available_parallelism()
            .map_or(0, |cores| u32::try_from(cores.get()).unwrap_or(u32::MAX)),
        os: format!(
            "{}, Linux {}",
            os_name(&os_release).unwrap_or_else(|| "Linux".into()),
            kernel_release().unwrap_or_default()
        ),
        // Found once hardware encoding exists (L.3).
        hardware_encoders: Vec::new(),
    }
}

/// `model name` in `/proc/cpuinfo` (x86), else `Model` (most ARM boards).
fn cpu_name(cpuinfo: &str) -> Option<String> {
    ["model name", "Model"].iter().find_map(|key| {
        cpuinfo.lines().find_map(|line| {
            let (name, value) = line.split_once(':')?;
            (name.trim() == *key && !value.trim().is_empty()).then(|| value.trim().to_string())
        })
    })
}

/// `MemTotal` in `/proc/meminfo`, in KiB.
fn memory_kib(meminfo: &str) -> Option<u64> {
    let line = meminfo.lines().find(|line| line.starts_with("MemTotal:"))?;
    line.split_whitespace().nth(1)?.parse().ok()
}

/// `PRETTY_NAME` in `/etc/os-release`.
fn os_name(os_release: &str) -> Option<String> {
    let value = os_release
        .lines()
        .find_map(|line| line.strip_prefix("PRETTY_NAME="))?;
    let value = value.trim().trim_matches('"').trim_matches('\'');
    (!value.is_empty()).then(|| value.to_string())
}

fn kernel_release() -> Option<String> {
    let mut name = MaybeUninit::<libc::utsname>::zeroed();
    // SAFETY: uname fills the structure it is given.
    if unsafe { libc::uname(name.as_mut_ptr()) } != 0 {
        return None;
    }
    // SAFETY: filled in above; `release` is NUL-terminated.
    let name = unsafe { name.assume_init() };
    let release = unsafe { CStr::from_ptr(name.release.as_ptr()) };
    Some(release.to_string_lossy().into_owned())
}

/// File system magic numbers (`statfs`'s `f_type`) of network shares.
const NETWORK_FILE_SYSTEMS: &[(i64, &str)] = &[
    (0x6969, "nfs"),
    (0xff53_4d42, "cifs"),
    (0xfe53_4d42, "smb2"),
    (0x517b, "smb"),
    (0x0102_1997, "9p"),
];

/// Names of common local file systems by magic number.
const LOCAL_FILE_SYSTEMS: &[(i64, &str)] = &[
    (0xef53, "ext4"),
    (0x9123_683e, "btrfs"),
    (0x5846_5342, "xfs"),
    (0x2fc1_2fc1, "zfs"),
    (0xf2f5_2010, "f2fs"),
    (0x2011_bab0, "exfat"),
    (0x4d44, "vfat"),
    (0x5346_544e, "ntfs"),
    (0x7366_746e, "ntfs3"),
    (0x0102_1994, "tmpfs"),
    (0xcafe_4a11, "bcachefs"),
    // Local or not (ntfs-3g, sshfs): the write-speed test judges it.
    (0x6573_5546, "fuse"),
];

/// Free space and kind of the volume holding `path`.
pub fn volume(path: &Path) -> anyhow::Result<Volume> {
    let path = CString::new(path.as_os_str().as_bytes())?;
    let mut stats = MaybeUninit::<libc::statfs>::uninit();
    // SAFETY: statfs fills the structure it is given.
    if unsafe { libc::statfs(path.as_ptr(), stats.as_mut_ptr()) } != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    // SAFETY: filled in above.
    let stats = unsafe { stats.assume_init() };
    // `f_type`'s integer type differs between architectures.
    #[allow(clippy::useless_conversion)]
    let kind = i64::from(stats.f_type);
    let named = |list: &'static [(i64, &'static str)]| {
        list.iter()
            .find(|(magic, _)| *magic == kind)
            .map(|(_, name)| *name)
    };
    let network = named(NETWORK_FILE_SYSTEMS);
    let file_system = network
        .or_else(|| named(LOCAL_FILE_SYSTEMS))
        .map_or_else(|| format!("{kind:#x}"), str::to_string);
    Ok(Volume {
        free_bytes: stats.f_bavail * stats.f_bsize as u64,
        file_system,
        network: network.is_some(),
    })
}

/// Write 4 KB at the start of `probe` and `fdatasync` it, timed.
pub fn write_latency(probe: &Path) -> std::io::Result<std::time::Duration> {
    use std::os::unix::fs::FileExt;
    use std::os::unix::io::AsRawFd;
    let started = std::time::Instant::now();
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(probe)?;
    file.write_all_at(&[0; 4096], 0)?;
    // SAFETY: an open descriptor.
    if unsafe { libc::fdatasync(file.as_raw_fd()) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(started.elapsed())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_proc_and_os_release() {
        let cpuinfo = "processor\t: 0\nmodel name\t: Intel(R) Core(TM) Ultra 9 285K\n";
        assert_eq!(
            cpu_name(cpuinfo).as_deref(),
            Some("Intel(R) Core(TM) Ultra 9 285K")
        );
        assert_eq!(
            memory_kib("MemTotal:       65536000 kB\n"),
            Some(65_536_000)
        );
        assert_eq!(
            os_name("NAME=Arch\nPRETTY_NAME=\"Arch Linux\"\n").as_deref(),
            Some("Arch Linux")
        );
    }
}
