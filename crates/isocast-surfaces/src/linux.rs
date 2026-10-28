//! Linux: a surface is a DMA-BUF, a picture in the daemon's GPU memory
//! exported as a file descriptor. A Unix socket carries descriptors
//! (`SCM_RIGHTS`), so the daemon listens on a socket of its own in
//! `$XDG_RUNTIME_DIR/isocast` (the "service"), answering only processes of
//! its own user; a client sends its one-time token and receives the ring's
//! descriptors with a line describing each. After that only slot numbers
//! travel, as on macOS.
//!
//! The client's GPU imports the DMA-BUF in place, so it must be the GPU that
//! drew it: every surface carries its Vulkan device UUID, and a device with
//! another one is refused (no cross-GPU or CPU fallback here; the app shows
//! the engine's H.264 instead). `gpu_adapter` tells a client which adapter
//! to open ([`vulkan_adapter`]).

use std::collections::HashMap;
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
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
    /// The Vulkan device UUID of the GPU it lives on.
    gpu: [u8; 16],
    id: u32,
}

static NEXT_ID: AtomicU32 = AtomicU32::new(1);

impl Surface {
    /// On Linux a surface is made on the GPU that draws into it
    /// ([`Surface::rgba_on`]), not on its own.
    pub fn rgba(_width: u32, _height: u32) -> Result<Self> {
        bail!("Linux shared surfaces require rgba_on with the producer's Vulkan device")
    }

    fn from_dmabuf(
        fd: OwnedFd,
        (width, height): (u32, u32),
        (stride, offset, modifier): (u32, u32, u64),
        gpu: [u8; 16],
    ) -> Self {
        Self(Arc::new(Inner {
            fd,
            width,
            height,
            stride,
            offset,
            modifier,
            gpu,
            id: NEXT_ID.fetch_add(1, Ordering::Relaxed),
        }))
    }

    pub fn width(&self) -> u32 {
        self.0.width
    }

    pub fn height(&self) -> u32 {
        self.0.height
    }

    /// Unique in this process (for logs).
    pub fn id(&self) -> u32 {
        self.0.id
    }

    /// A new `width`×`height` 8-bit RGBA surface in `device`'s GPU memory,
    /// its contents undefined: a linear Vulkan image exported as a
    /// DMA-BUF. Draw into it through [`Surface::texture`], as clients show
    /// it. `device` must be Vulkan with `VULKAN_EXTERNAL_MEMORY_DMA_BUF`.
    #[cfg(feature = "wgpu")]
    pub fn rgba_on(device: &wgpu::Device, width: u32, height: u32) -> Result<Self> {
        use ash::vk;
        use wgpu::hal;

        if !(1..=8192).contains(&width) || !(1..=8192).contains(&height) {
            bail!("a shared surface can't be {width}×{height}; give 1 to 8192 each way");
        }
        dmabuf_device(device)?;
        let gpu = vulkan_gpu(device)?;
        // SAFETY: only reads the device's handles, which live as long as it.
        let (raw, instance, physical, enabled) = unsafe {
            let hal_device = device
                .as_hal::<hal::api::Vulkan>()
                .context("shared surfaces need a Vulkan device")?;
            (
                hal_device.raw_device().clone(),
                hal_device.shared_instance().raw_instance().clone(),
                hal_device.raw_physical_device(),
                hal_device.enabled_device_extensions().to_vec(),
            )
        };
        for extension in [
            ash::khr::external_memory_fd::NAME,
            ash::ext::external_memory_dma_buf::NAME,
        ] {
            if !enabled.contains(&extension) {
                bail!("this GPU can't export DMA-BUF shared surfaces (no {extension:?})");
            }
        }
        let handle = vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT;
        let mut external = vk::ExternalMemoryImageCreateInfo::default().handle_types(handle);
        let info = vk::ImageCreateInfo::default()
            .image_type(vk::ImageType::TYPE_2D)
            .format(vk::Format::R8G8B8A8_UNORM)
            .extent(vk::Extent3D {
                width,
                height,
                depth: 1,
            })
            .mip_levels(1)
            .array_layers(1)
            .samples(vk::SampleCountFlags::TYPE_1)
            .tiling(vk::ImageTiling::LINEAR)
            .usage(
                vk::ImageUsageFlags::COLOR_ATTACHMENT
                    | vk::ImageUsageFlags::SAMPLED
                    | vk::ImageUsageFlags::TRANSFER_SRC
                    | vk::ImageUsageFlags::TRANSFER_DST,
            )
            .sharing_mode(vk::SharingMode::EXCLUSIVE)
            .initial_layout(vk::ImageLayout::UNDEFINED)
            .push_next(&mut external);
        // SAFETY: valid create infos on a live device. The image and its
        // memory only exist to make the DMA-BUF: both go before returning,
        // and the descriptor keeps the pixels (the producer imports it like
        // any client, so every texture of it is made one way).
        unsafe {
            let image = raw
                .create_image(&info, None)
                .context("couldn't make a shared surface image")?;
            let requirements = raw.get_image_memory_requirements(image);
            let layout = raw.get_image_subresource_layout(
                image,
                vk::ImageSubresource {
                    aspect_mask: vk::ImageAspectFlags::COLOR,
                    mip_level: 0,
                    array_layer: 0,
                },
            );
            let properties = instance.get_physical_device_memory_properties(physical);
            let types = &properties.memory_types[..properties.memory_type_count as usize];
            let allowed = |index: usize| requirements.memory_type_bits & (1 << index) != 0;
            let index = (0..types.len())
                .find(|&index| {
                    allowed(index)
                        && types[index]
                            .property_flags
                            .contains(vk::MemoryPropertyFlags::DEVICE_LOCAL)
                })
                .or_else(|| (0..types.len()).find(|&index| allowed(index)));
            let Some(index) = index else {
                raw.destroy_image(image, None);
                bail!("no GPU memory type for a shared surface");
            };
            let mut export = vk::ExportMemoryAllocateInfo::default().handle_types(handle);
            let mut dedicated = vk::MemoryDedicatedAllocateInfo::default().image(image);
            let allocate = vk::MemoryAllocateInfo::default()
                .allocation_size(requirements.size)
                .memory_type_index(index as u32)
                .push_next(&mut export)
                .push_next(&mut dedicated);
            let memory = match raw.allocate_memory(&allocate, None) {
                Ok(memory) => memory,
                Err(error) => {
                    raw.destroy_image(image, None);
                    return Err(error).context("couldn't allocate a shared surface");
                }
            };
            let exported = raw
                .bind_image_memory(image, memory, 0)
                .context("couldn't bind a shared surface")
                .and_then(|()| {
                    ash::khr::external_memory_fd::Device::new(&instance, &raw)
                        .get_memory_fd(
                            &vk::MemoryGetFdInfoKHR::default()
                                .memory(memory)
                                .handle_type(handle),
                        )
                        .context("couldn't export a shared surface")
                });
            raw.destroy_image(image, None);
            raw.free_memory(memory, None);
            let fd = OwnedFd::from_raw_fd(exported?);
            Ok(Self::from_dmabuf(
                fd,
                (width, height),
                (
                    u32::try_from(layout.row_pitch).context("a shared surface's row pitch")?,
                    u32::try_from(layout.offset).context("a shared surface's offset")?,
                    0, // DRM_FORMAT_MOD_LINEAR
                ),
                gpu.uuid,
            ))
        }
    }

    /// The surface as an `Rgba8Unorm` texture of `device` with `usage`, in
    /// place: the GPU reads and writes the DMA-BUF itself. `device` must be
    /// Vulkan with `VULKAN_EXTERNAL_MEMORY_DMA_BUF`, on the GPU the surface
    /// was made on. For Slint's `Image::try_from(wgpu::Texture)` ask for
    /// `TEXTURE_BINDING | RENDER_ATTACHMENT`.
    #[cfg(feature = "wgpu")]
    pub fn texture(
        &self,
        device: &wgpu::Device,
        usage: wgpu::TextureUsages,
    ) -> Result<wgpu::Texture> {
        use wgpu::hal;

        dmabuf_device(device)?;
        let gpu = vulkan_gpu(device)?;
        if gpu.uuid != self.0.gpu {
            bail!(
                "shared surface GPU mismatch: drawn on Vulkan device {}, this device is {}; app \
                 and daemon must use the same GPU",
                hex(&self.0.gpu),
                hex(&gpu.uuid)
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
        for (wanted, used) in [
            (
                wgpu::TextureUsages::TEXTURE_BINDING,
                wgpu::TextureUses::RESOURCE,
            ),
            (
                wgpu::TextureUsages::RENDER_ATTACHMENT,
                wgpu::TextureUses::COLOR_TARGET,
            ),
            (wgpu::TextureUsages::COPY_SRC, wgpu::TextureUses::COPY_SRC),
            (wgpu::TextureUsages::COPY_DST, wgpu::TextureUses::COPY_DST),
        ] {
            if usage.contains(wanted) {
                uses |= used;
            }
        }
        // SAFETY: the descriptor is a DMA-BUF of this size, format, stride,
        // offset and modifier on this GPU (`rgba_on` describes it); wgpu
        // takes ownership of the duplicate, and frees it on failure.
        unsafe {
            let hal_texture = {
                let hal_device = device
                    .as_hal::<hal::api::Vulkan>()
                    .context("shared surfaces need a Vulkan device")?;
                hal_device
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
                    .map_err(|error| anyhow!("couldn't import a shared surface: {error:?}"))?
            };
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

/// `device` can share DMA-BUFs.
#[cfg(feature = "wgpu")]
fn dmabuf_device(device: &wgpu::Device) -> Result<()> {
    if !device
        .features()
        .contains(wgpu::Features::VULKAN_EXTERNAL_MEMORY_DMA_BUF)
    {
        bail!("this GPU device can't share DMA-BUF surfaces (no VULKAN_EXTERNAL_MEMORY_DMA_BUF)");
    }
    Ok(())
}

/// Which Vulkan GPU a device is on: its PCI vendor and device ids (what
/// the daemon's `gpu_adapter` returns on Linux, for a client to open the
/// same adapter) and its device UUID (what each surface carries).
#[cfg(feature = "wgpu")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VulkanGpu {
    pub vendor: u32,
    pub device: u32,
    pub uuid: [u8; 16],
}

/// The Vulkan GPU `device` is on.
#[cfg(feature = "wgpu")]
pub fn vulkan_gpu(device: &wgpu::Device) -> Result<VulkanGpu> {
    use ash::vk;
    // SAFETY: queries the device's own physical device through its live
    // instance; the properties are written into locals.
    unsafe {
        let hal_device = device
            .as_hal::<wgpu::hal::api::Vulkan>()
            .context("shared surfaces need a Vulkan device")?;
        let shared = hal_device.shared_instance();
        if shared.instance_api_version() < vk::API_VERSION_1_1 {
            bail!("shared surfaces need Vulkan 1.1 (to tell GPUs apart)");
        }
        let mut id = vk::PhysicalDeviceIDProperties::default();
        let mut properties = vk::PhysicalDeviceProperties2::default().push_next(&mut id);
        shared
            .raw_instance()
            .get_physical_device_properties2(hal_device.raw_physical_device(), &mut properties);
        Ok(VulkanGpu {
            vendor: properties.properties.vendor_id,
            device: properties.properties.device_id,
            uuid: id.device_uuid,
        })
    }
}

/// The Vulkan adapter with PCI ids `(vendor, device)` (the daemon's
/// `gpu_adapter`), if this process sees one. Two identical cards share ids;
/// the surfaces' device UUIDs tell them apart, so the wrong one of them is
/// refused at import, not shown.
#[cfg(feature = "wgpu")]
pub fn vulkan_adapter(
    instance: &wgpu::Instance,
    (vendor, device): (u32, u32),
) -> Option<wgpu::Adapter> {
    block_on(instance.enumerate_adapters(wgpu::Backends::VULKAN))
        .into_iter()
        .find(|adapter| {
            let info = adapter.get_info();
            (info.vendor, info.device) == (vendor, device)
        })
}

/// Native wgpu adapter requests complete synchronously.
#[cfg(feature = "wgpu")]
fn block_on<F: std::future::Future>(future: F) -> F::Output {
    let mut future = std::pin::pin!(future);
    let mut context = std::task::Context::from_waker(std::task::Waker::noop());
    loop {
        if let std::task::Poll::Ready(output) = future.as_mut().poll(&mut context) {
            return output;
        }
        std::thread::yield_now();
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn unhex(text: &str) -> Option<[u8; 16]> {
    let mut bytes = [0u8; 16];
    if text.len() != 32 {
        return None;
    }
    for (index, byte) in bytes.iter_mut().enumerate() {
        *byte = u8::from_str_radix(text.get(index * 2..index * 2 + 2)?, 16).ok()?;
    }
    Some(bytes)
}

/// The host clock now, in nanoseconds: `CLOCK_MONOTONIC`. (The Linux
/// engine's frame times count from its own start, so they don't compare
/// with it; see `isocast frames --shared`.)
pub fn host_now() -> u64 {
    let mut time = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: writes the clock's time into `time`.
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut time) };
    time.tv_sec as u64 * 1_000_000_000 + time.tv_nsec as u64
}

/// Rings offered and not fetched yet, by token.
type Offers = Mutex<HashMap<Token, Vec<Surface>>>;

/// The daemon's side: a socket that hands each offered ring of surfaces
/// to the client holding its token, once.
pub struct Exporter {
    path: PathBuf,
    offers: Arc<Offers>,
}

impl Exporter {
    /// This process's exporter, listening from the first call.
    pub fn shared() -> Result<&'static Exporter> {
        static SHARED: OnceLock<Result<Exporter, String>> = OnceLock::new();
        SHARED
            .get_or_init(|| Exporter::start().map_err(|err| format!("{err:#}")))
            .as_ref()
            .map_err(|err| anyhow!("{err}"))
    }

    fn start() -> Result<Self> {
        let folder = std::env::var_os("XDG_RUNTIME_DIR")
            .map(PathBuf::from)
            .filter(|path| path.is_absolute() && path.is_dir())
            .context("XDG_RUNTIME_DIR isn't set: no private folder for the surfaces socket")?
            .join("isocast");
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&folder)
            .with_context(|| format!("creating {}", folder.display()))?;
        remove_stale(&folder);
        let path = folder.join(format!("surfaces-{}.sock", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let listener = UnixListener::bind(&path)
            .with_context(|| format!("listening on {}", path.display()))?;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
        let offers = Arc::new(Offers::default());
        let served = offers.clone();
        std::thread::Builder::new()
            .name("shared surfaces".into())
            .spawn(move || {
                // One client at a time, each bounded by its timeouts.
                for connection in listener.incoming().flatten() {
                    let _ = serve(connection, &served);
                }
            })
            .context("could not start the shared surface service")?;
        Ok(Self { path, offers })
    }

    /// Where clients fetch rings: the socket's path.
    pub fn service(&self) -> &str {
        self.path.to_str().unwrap_or_default()
    }

    /// Offer `surfaces` to whoever brings the returned token, once.
    pub fn offer(&self, surfaces: Vec<Surface>) -> Token {
        let token = Token::random();
        self.offers
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(token, surfaces);
        token
    }

    /// Take back an offer not fetched yet.
    pub fn withdraw(&self, token: Token) {
        self.offers
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&token);
    }
}

/// Remove the sockets of daemons that are gone (the exporter lives as long
/// as its process, which never drops it).
fn remove_stale(folder: &std::path::Path) {
    let Ok(entries) = std::fs::read_dir(folder) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let pid = name
            .to_str()
            .and_then(|name| name.strip_prefix("surfaces-"))
            .and_then(|name| name.strip_suffix(".sock"))
            .and_then(|pid| pid.parse::<u32>().ok());
        if let Some(pid) = pid
            && !std::path::Path::new(&format!("/proc/{pid}")).exists()
        {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// Answer one client: its token, then the ring's description and
/// descriptors.
fn serve(mut connection: UnixStream, offers: &Offers) -> Result<()> {
    connection.set_read_timeout(Some(Duration::from_secs(2)))?;
    connection.set_write_timeout(Some(Duration::from_secs(2)))?;
    // Only this user's processes.
    let mut credentials = libc::ucred {
        pid: 0,
        uid: 0,
        gid: 0,
    };
    let mut length = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    // SAFETY: getsockopt writes at most `length` bytes into `credentials`.
    let code = unsafe {
        libc::getsockopt(
            connection.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&raw mut credentials).cast(),
            &mut length,
        )
    };
    // SAFETY: no preconditions.
    if code != 0 || credentials.uid != unsafe { libc::geteuid() } {
        connection.write_all(b"error: not this user's process\n")?;
        bail!("a client of another user");
    }
    let mut token = [0u8; 16];
    connection.read_exact(&mut token)?;
    let surfaces = offers
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(&Token(token));
    let Some(surfaces) = surfaces else {
        connection.write_all(b"error: no ring for this token (it works once)\n")?;
        return Ok(());
    };
    let description: Vec<String> = surfaces
        .iter()
        .map(|s| {
            let s = &s.0;
            format!(
                "{} {} {} {} {} {}",
                s.width,
                s.height,
                s.stride,
                s.offset,
                s.modifier,
                hex(&s.gpu)
            )
        })
        .collect();
    let line = format!("ok {}\n", description.join(";"));
    let fds: Vec<RawFd> = surfaces.iter().map(|s| s.0.fd.as_raw_fd()).collect();
    send_with_fds(&connection, line.as_bytes(), &fds)
}

fn send_with_fds(socket: &UnixStream, data: &[u8], fds: &[RawFd]) -> Result<()> {
    // SAFETY: computes a size; no memory is touched.
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
        let sent = libc::sendmsg(socket.as_raw_fd(), &message, libc::MSG_NOSIGNAL);
        if sent < 0 {
            return Err(std::io::Error::last_os_error()).context("sending shared surfaces");
        }
        if sent as usize != data.len() {
            bail!(
                "sent {sent} of {} bytes of a ring's description",
                data.len()
            );
        }
    }
    Ok(())
}

/// Fetch the ring offered under `token` from the daemon's socket `service`.
pub fn fetch(service: &str, token: Token, timeout: Duration) -> Result<Vec<Surface>> {
    if !service.starts_with('/') {
        bail!("{service:?} isn't a Linux shared surface service (a socket path)");
    }
    let mut socket = UnixStream::connect(service)
        .with_context(|| format!("connecting to isocastd's surfaces socket {service}"))?;
    socket.set_read_timeout(Some(timeout))?;
    socket.set_write_timeout(Some(timeout))?;
    socket.write_all(&token.0)?;
    let mut data = vec![0u8; 4096];
    // SAFETY: computes a size; no memory is touched.
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
    let mut received = unsafe {
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
                    fds.push(OwnedFd::from_raw_fd(first.add(index).read_unaligned()));
                }
            }
            header = libc::CMSG_NXTHDR(&message, header);
        }
        if message.msg_flags & libc::MSG_CTRUNC != 0 {
            bail!("isocastd sent more shared surfaces than a ring can have");
        }
        received as usize
    };
    // The rest of the line, if it came in pieces.
    while !data[..received].contains(&b'\n') && received < data.len() {
        match socket.read(&mut data[received..])? {
            0 => break,
            read => received += read,
        }
    }
    let text = String::from_utf8_lossy(&data[..received]);
    let line = text.lines().next().unwrap_or_default();
    let Some(description) = line.strip_prefix("ok ") else {
        bail!("isocastd refused the ring: {line}");
    };
    let parts: Vec<&str> = description.split(';').collect();
    if parts.len() != fds.len() {
        bail!(
            "isocastd described {} surfaces but sent {} descriptors",
            parts.len(),
            fds.len()
        );
    }
    parts
        .into_iter()
        .zip(fds)
        .map(|(part, fd)| {
            let fields: Vec<&str> = part.split_whitespace().collect();
            let [width, height, stride, offset, modifier, gpu] = fields[..] else {
                bail!("a shared surface's description has {} fields", fields.len());
            };
            let number = |text: &str| {
                text.parse::<u32>()
                    .context("a shared surface's description")
            };
            Ok(Surface::from_dmabuf(
                fd,
                (number(width)?, number(height)?),
                (
                    number(stride)?,
                    number(offset)?,
                    modifier
                        .parse()
                        .context("a shared surface's DRM modifier")?,
                ),
                unhex(gpu).context("a shared surface's GPU")?,
            ))
        })
        .collect()
}
