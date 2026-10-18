//! Linux: a surface is a DMA-BUF, a GPU image the daemon exports as a file
//! descriptor. A Unix socket carries descriptors (`SCM_RIGHTS`), so the
//! daemon listens on a socket of its own in `$XDG_RUNTIME_DIR` (the
//! "service"); a client sends its one-time token and receives the ring's
//! descriptors with a line describing each. After that only slot numbers
//! travel, as on macOS.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};

use crate::{MAX_SLOTS, Token};

/// A picture in GPU memory other processes can show: an 8-bit RGBA
/// DMA-BUF with one plane. Clones share it.
#[derive(Clone)]
pub struct Surface(Arc<Inner>);

struct Inner {
    fd: OwnedFd,
    width: u32,
    height: u32,
    /// Bytes from one row to the next.
    stride: u32,
    /// Where the plane starts in the buffer.
    offset: u32,
    /// The DRM format modifier (0: linear).
    modifier: u64,
    id: u32,
}

static NEXT_ID: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(1);

impl Surface {
    /// On Linux surfaces are made by the GPU that draws into them (the
    /// platform's `shared_surface`), not on their own.
    pub fn rgba(_width: u32, _height: u32) -> Result<Self> {
        bail!("on Linux a shared surface is made by the GPU that draws into it")
    }

    /// A DMA-BUF of an 8-bit RGBA picture.
    pub fn from_dmabuf(
        fd: OwnedFd,
        width: u32,
        height: u32,
        stride: u32,
        offset: u32,
        modifier: u64,
    ) -> Self {
        Self(Arc::new(Inner {
            fd,
            width,
            height,
            stride,
            offset,
            modifier,
            id: NEXT_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
        }))
    }

    pub fn width(&self) -> u32 {
        self.0.width
    }

    pub fn height(&self) -> u32 {
        self.0.height
    }

    /// Unique in this process.
    pub fn id(&self) -> u32 {
        self.0.id
    }

    /// An `Rgba8Unorm` texture of `device` over the DMA-BUF, in place. The
    /// device must be Vulkan with `VULKAN_EXTERNAL_MEMORY_DMA_BUF`.
    #[cfg(feature = "wgpu")]
    pub fn texture(
        &self,
        device: &wgpu::Device,
        usage: wgpu::TextureUsages,
    ) -> Result<wgpu::Texture> {
        use wgpu::hal;
        if !device
            .features()
            .contains(wgpu::Features::VULKAN_EXTERNAL_MEMORY_DMA_BUF)
        {
            bail!(
                "this GPU device can't import shared surfaces (no VULKAN_EXTERNAL_MEMORY_DMA_BUF)"
            );
        }
        let fd = self
            .0
            .fd
            .try_clone()
            .context("couldn't duplicate a shared surface's descriptor")?;
        let size = wgpu::Extent3d {
            width: self.0.width,
            height: self.0.height,
            depth_or_array_layers: 1,
        };
        let mut uses = wgpu::TextureUses::empty();
        if usage.contains(wgpu::TextureUsages::TEXTURE_BINDING) {
            uses |= wgpu::TextureUses::RESOURCE;
        }
        if usage.contains(wgpu::TextureUsages::RENDER_ATTACHMENT) {
            uses |= wgpu::TextureUses::COLOR_TARGET;
        }
        if usage.contains(wgpu::TextureUsages::COPY_SRC) {
            uses |= wgpu::TextureUses::COPY_SRC;
        }
        // SAFETY: the descriptor is a DMA-BUF of this size, format, stride,
        // offset and modifier (the daemon's `from_dmabuf` describes it);
        // wgpu takes ownership of the duplicate.
        unsafe {
            let hal_device = device
                .as_hal::<hal::api::Vulkan>()
                .context("shared surfaces need a Vulkan device")?;
            let hal_texture = hal_device
                .texture_from_dmabuf_fd(
                    fd,
                    &hal::TextureDescriptor {
                        label: Some("shared surface"),
                        size,
                        mip_level_count: 1,
                        sample_count: 1,
                        dimension: wgpu::TextureDimension::D2,
                        format: wgpu::TextureFormat::Rgba8Unorm,
                        usage: uses,
                        memory_flags: hal::MemoryFlags::empty(),
                        view_formats: Vec::new(),
                    },
                    self.0.modifier,
                    u64::from(self.0.stride),
                    u64::from(self.0.offset),
                )
                .map_err(|error| anyhow!("importing a shared surface: {error:?}"))?;
            drop(hal_device);
            Ok(device.create_texture_from_hal::<hal::api::Vulkan>(
                hal_texture,
                &wgpu::TextureDescriptor {
                    label: Some("shared surface"),
                    size,
                    mip_level_count: 1,
                    sample_count: 1,
                    dimension: wgpu::TextureDimension::D2,
                    format: wgpu::TextureFormat::Rgba8Unorm,
                    usage,
                    view_formats: &[],
                },
                wgpu::TextureUses::UNINITIALIZED,
            ))
        }
    }
}

impl std::fmt::Debug for Surface {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Surface({}×{}, dmabuf {}, stride {}, modifier {:#x})",
            self.0.width,
            self.0.height,
            self.0.fd.as_raw_fd(),
            self.0.stride,
            self.0.modifier
        )
    }
}

/// The host clock now, in nanoseconds: `CLOCK_MONOTONIC`, the engine's.
pub fn host_now() -> u64 {
    let mut time = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: writes the clock's time into `time`.
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut time) };
    time.tv_sec as u64 * 1_000_000_000 + time.tv_nsec as u64
}

/// The daemon's side: a socket that hands each offered ring of surfaces
/// to the client holding its token, once.
pub struct Exporter {
    path: PathBuf,
    offered: Arc<Mutex<HashMap<[u8; 16], Vec<Surface>>>>,
}

impl Exporter {
    /// The process's exporter, listening from the first call.
    pub fn shared() -> Result<&'static Exporter> {
        static SHARED: OnceLock<std::result::Result<Exporter, String>> = OnceLock::new();
        SHARED
            .get_or_init(|| Exporter::start().map_err(|error| format!("{error:#}")))
            .as_ref()
            .map_err(|error| anyhow!("{error}"))
    }

    fn start() -> Result<Self> {
        let folder = std::env::var_os("XDG_RUNTIME_DIR")
            .map(PathBuf::from)
            .filter(|path| path.is_absolute())
            .context("XDG_RUNTIME_DIR isn't set: no private folder for the surfaces socket")?;
        let path = folder.join(format!("isocast-surfaces-{}.sock", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let listener = UnixListener::bind(&path)
            .with_context(|| format!("listening on {}", path.display()))?;
        std::fs::set_permissions(&path, std::os::unix::fs::PermissionsExt::from_mode(0o600))?;
        let offered: Arc<Mutex<HashMap<[u8; 16], Vec<Surface>>>> = Arc::default();
        let serving = Arc::clone(&offered);
        std::thread::Builder::new()
            .name("shared surfaces".into())
            .spawn(move || {
                for connection in listener.incoming().flatten() {
                    let _ = serve(connection, &serving);
                }
            })?;
        Ok(Self { path, offered })
    }

    /// Where clients fetch rings: the socket's path.
    pub fn service(&self) -> &str {
        self.path.to_str().unwrap_or_default()
    }

    /// Offer `surfaces` to whoever brings the returned token, once.
    pub fn offer(&self, surfaces: Vec<Surface>) -> Token {
        let token = Token::random();
        if let Ok(mut offered) = self.offered.lock() {
            offered.insert(token.0, surfaces);
        }
        token
    }

    /// Take back an offer not fetched yet.
    pub fn withdraw(&self, token: Token) {
        if let Ok(mut offered) = self.offered.lock() {
            offered.remove(&token.0);
        }
    }
}

impl Drop for Exporter {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Answer one client: its token, then the ring's description and
/// descriptors.
fn serve(
    mut connection: UnixStream,
    offered: &Mutex<HashMap<[u8; 16], Vec<Surface>>>,
) -> Result<()> {
    connection.set_read_timeout(Some(Duration::from_secs(2)))?;
    // Only this user's processes.
    let mut cred = libc::ucred {
        pid: 0,
        uid: 0,
        gid: 0,
    };
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    // SAFETY: getsockopt writes at most `len` bytes into `cred`.
    let code = unsafe {
        libc::getsockopt(
            connection.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&mut cred as *mut libc::ucred).cast(),
            &mut len,
        )
    };
    // SAFETY: no preconditions.
    if code != 0 || cred.uid != unsafe { libc::geteuid() } {
        bail!("a client of another user");
    }
    let mut token = [0u8; 16];
    connection.read_exact(&mut token)?;
    let surfaces = offered
        .lock()
        .map_err(|_| anyhow!("poisoned"))?
        .remove(&token);
    let Some(surfaces) = surfaces else {
        connection.write_all(b"error: no ring for this token\n")?;
        return Ok(());
    };
    let description: Vec<String> = surfaces
        .iter()
        .map(|s| {
            format!(
                "{} {} {} {} {}",
                s.0.width, s.0.height, s.0.stride, s.0.offset, s.0.modifier
            )
        })
        .collect();
    let line = format!("ok {}\n", description.join(";"));
    let fds: Vec<RawFd> = surfaces.iter().map(|s| s.0.fd.as_raw_fd()).collect();
    send_with_fds(&connection, line.as_bytes(), &fds)
}

fn send_with_fds(socket: &UnixStream, data: &[u8], fds: &[RawFd]) -> Result<()> {
    let space = unsafe { libc::CMSG_SPACE(std::mem::size_of_val(fds) as u32) } as usize;
    let mut control = vec![0u8; space];
    let mut iov = libc::iovec {
        iov_base: data.as_ptr().cast_mut().cast(),
        iov_len: data.len(),
    };
    // SAFETY: a message with one data buffer and one SCM_RIGHTS control
    // message sized for `fds`, all alive for the call.
    unsafe {
        let mut message: libc::msghdr = std::mem::zeroed();
        message.msg_iov = &mut iov;
        message.msg_iovlen = 1;
        message.msg_control = control.as_mut_ptr().cast();
        message.msg_controllen = space as _;
        let header = libc::CMSG_FIRSTHDR(&message);
        (*header).cmsg_level = libc::SOL_SOCKET;
        (*header).cmsg_type = libc::SCM_RIGHTS;
        (*header).cmsg_len = libc::CMSG_LEN(std::mem::size_of_val(fds) as u32) as _;
        std::ptr::copy_nonoverlapping(
            fds.as_ptr(),
            libc::CMSG_DATA(header).cast::<RawFd>(),
            fds.len(),
        );
        if libc::sendmsg(socket.as_raw_fd(), &message, 0) < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
    }
    Ok(())
}

/// Fetch the ring offered under `token` from the daemon's `service`.
pub fn fetch(service: &str, token: Token, timeout: Duration) -> Result<Vec<Surface>> {
    let mut socket = UnixStream::connect(service)
        .with_context(|| format!("connecting to the surfaces socket {service}"))?;
    socket.set_read_timeout(Some(timeout))?;
    socket.set_write_timeout(Some(timeout))?;
    socket.write_all(&token.0)?;
    let mut data = vec![0u8; 4096];
    let space =
        unsafe { libc::CMSG_SPACE((MAX_SLOTS * std::mem::size_of::<RawFd>()) as u32) } as usize;
    let mut control = vec![0u8; space];
    let mut iov = libc::iovec {
        iov_base: data.as_mut_ptr().cast(),
        iov_len: data.len(),
    };
    let mut fds: Vec<OwnedFd> = Vec::new();
    // SAFETY: receives into buffers alive for the call; every descriptor
    // that arrives is taken into an `OwnedFd` at once.
    let received = unsafe {
        let mut message: libc::msghdr = std::mem::zeroed();
        message.msg_iov = &mut iov;
        message.msg_iovlen = 1;
        message.msg_control = control.as_mut_ptr().cast();
        message.msg_controllen = space as _;
        let received = libc::recvmsg(socket.as_raw_fd(), &mut message, libc::MSG_CMSG_CLOEXEC);
        if received < 0 {
            return Err(std::io::Error::last_os_error()).context("receiving shared surfaces");
        }
        let mut header = libc::CMSG_FIRSTHDR(&message);
        while !header.is_null() {
            if (*header).cmsg_level == libc::SOL_SOCKET && (*header).cmsg_type == libc::SCM_RIGHTS {
                let count = ((*header).cmsg_len as usize - libc::CMSG_LEN(0) as usize)
                    / std::mem::size_of::<RawFd>();
                let first = libc::CMSG_DATA(header).cast::<RawFd>();
                for index in 0..count {
                    fds.push(OwnedFd::from_raw_fd(*first.add(index)));
                }
            }
            header = libc::CMSG_NXTHDR(&message, header);
        }
        received as usize
    };
    let text = String::from_utf8_lossy(&data[..received]);
    let line = text.lines().next().unwrap_or_default();
    let Some(description) = line.strip_prefix("ok ") else {
        bail!("the daemon refused the ring: {line}");
    };
    let parts: Vec<&str> = description.split(';').collect();
    if parts.len() != fds.len() {
        bail!(
            "the daemon described {} surfaces but sent {} descriptors",
            parts.len(),
            fds.len()
        );
    }
    parts
        .into_iter()
        .zip(fds)
        .map(|(part, fd)| {
            let numbers: Vec<u64> = part
                .split_whitespace()
                .map(|n| n.parse::<u64>())
                .collect::<std::result::Result<_, _>>()
                .context("a surface's description")?;
            let [width, height, stride, offset, modifier] = numbers[..] else {
                bail!("a surface's description has {} numbers", numbers.len());
            };
            Ok(Surface::from_dmabuf(
                fd,
                width as u32,
                height as u32,
                stride as u32,
                offset as u32,
                modifier,
            ))
        })
        .collect()
}
