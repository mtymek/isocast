//! Frames: page-aligned CPU memory that the GPU also uses directly, as
//! Vulkan images bound to it (`VK_EXT_external_memory_host`), the way
//! macOS's IOSurfaces are both. The GPU decides each plane's row pitch, so
//! a pool asks it before allocating; without a GPU, rows are packed to 256
//! bytes and frames can't be shown to one.

use std::ptr::NonNull;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context, Result, bail};
use isocast_engine::gpu::Gpu;

use crate::gpu::{self, HostImport};

/// The most frames one pool hands out before it waits for some to come
/// back.
pub const MAX_POOL_FRAMES: usize = 32;

/// Host pointers and sizes imported into Vulkan must be page-aligned.
pub(crate) const PAGE: usize = 4096;

/// What a frame holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// 8-bit BGRA.
    Bgra,
    /// 8-bit 4:2:0 Y'CbCr, video range, Rec. 709: a luma plane and a
    /// half-size plane of interleaved Cb and Cr.
    YCbCr,
}

/// Where a plane lies in a frame's memory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Plane {
    /// Where the image bound for it starts (aligned for the GPU).
    pub base: usize,
    /// Where its first row starts, from `base`.
    pub offset: usize,
    /// Bytes from one row's start to the next.
    pub row_pitch: usize,
    /// Bytes the image takes, from `base`.
    pub size: usize,
    pub width: u32,
    pub height: u32,
}

/// How a pool's frames are laid out, and what the GPU said about them.
#[derive(Debug, Clone)]
pub struct Layout {
    pub kind: Kind,
    pub width: u32,
    pub height: u32,
    pub planes: Vec<Plane>,
    /// Bytes of memory per frame (a whole number of pages).
    pub len: usize,
    /// Vulkan memory types the images may be bound to; 0 without a GPU.
    pub(crate) memory_types: u32,
}

impl Layout {
    /// The GPU's layout for `kind` frames of this size, or packed rows
    /// when there is no GPU to ask.
    fn new(kind: Kind, width: u32, height: u32) -> Result<Self> {
        if width == 0 || height == 0 {
            bail!("frame pool: size {width}x{height} must be at least 1x1");
        }
        if kind == Kind::YCbCr && (!width.is_multiple_of(2) || !height.is_multiple_of(2)) {
            bail!("frame pool: Y'CbCr size {width}x{height} must be even");
        }
        match Gpu::new() {
            Ok(gpu) if gpu::host_memory(&gpu) => gpu::layout(&gpu, kind, width, height),
            _ => Self::packed(kind, width, height),
        }
    }

    fn packed(kind: Kind, width: u32, height: u32) -> Result<Self> {
        let plane = |base: usize, width: u32, height: u32, bytes: usize| -> Result<Plane> {
            let row_pitch = (width as usize)
                .checked_mul(bytes)
                .and_then(|row| row.checked_next_multiple_of(256))
                .context("frame pool: frame is too large")?;
            let size = row_pitch
                .checked_mul(height as usize)
                .context("frame pool: frame is too large")?;
            Ok(Plane {
                base,
                offset: 0,
                row_pitch,
                size,
                width,
                height,
            })
        };
        let mut planes = Vec::new();
        match kind {
            Kind::Bgra => planes.push(plane(0, width, height, 4)?),
            Kind::YCbCr => {
                let luma = plane(0, width, height, 1)?;
                let base = luma.size.next_multiple_of(PAGE);
                planes.push(luma);
                planes.push(plane(base, width / 2, height / 2, 2)?);
            }
        }
        Self::finish(kind, width, height, planes, 0)
    }

    pub(crate) fn finish(
        kind: Kind,
        width: u32,
        height: u32,
        planes: Vec<Plane>,
        memory_types: u32,
    ) -> Result<Self> {
        let end = planes
            .iter()
            .map(|plane| plane.base + plane.size)
            .max()
            .unwrap_or(0);
        let len = end
            .checked_next_multiple_of(PAGE)
            .filter(|len| isize::try_from(*len).is_ok())
            .with_context(|| format!("frame pool: size {width}x{height} is too large"))?;
        Ok(Self {
            kind,
            width,
            height,
            planes,
            len,
            memory_types,
        })
    }
}

/// Anonymous, page-aligned memory, left out of child processes (plugins
/// fork from isocastd; memory pinned for the GPU must not be shared
/// copy-on-write).
pub(crate) struct Mapping {
    ptr: NonNull<u8>,
    len: usize,
}

// SAFETY: plain memory; writes happen only while one frame owns it.
unsafe impl Send for Mapping {}
unsafe impl Sync for Mapping {}

impl Mapping {
    fn new(len: usize) -> Result<Self> {
        // SAFETY: a fresh private anonymous mapping.
        let ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                -1,
                0,
            )
        };
        if ptr == libc::MAP_FAILED {
            return Err(std::io::Error::last_os_error())
                .with_context(|| format!("frame pool: couldn't allocate a frame ({len} bytes)"));
        }
        let mapping = Self {
            ptr: NonNull::new(ptr.cast()).context("frame pool: mmap returned null")?,
            len,
        };
        // SAFETY: advice about the mapping just made.
        if unsafe { libc::madvise(ptr, len, libc::MADV_DONTFORK) } != 0 {
            return Err(std::io::Error::last_os_error())
                .context("frame pool: couldn't keep a frame out of child processes");
        }
        Ok(mapping)
    }

    pub(crate) fn ptr(&self) -> *mut u8 {
        self.ptr.as_ptr()
    }

    pub(crate) fn len(&self) -> usize {
        self.len
    }
}

impl Drop for Mapping {
    fn drop(&mut self) {
        // SAFETY: the mapping made in `new`, unused from here on.
        unsafe { libc::munmap(self.ptr.as_ptr().cast(), self.len) };
    }
}

/// A frame's memory and its import into the GPU. The import is freed
/// before the memory (fields drop in order). Textures bound to it hold an
/// `Arc` of it, so the memory outlives every image using it.
pub(crate) struct Allocation {
    pub(crate) import: Option<HostImport>,
    pub(crate) mapping: Mapping,
    /// Unique for the process's lifetime, never reused.
    pub(crate) id: u64,
}

/// One buffer of a pool. A frame holds it; the pool reuses it once nothing
/// else does (frames and uncached GPU textures hold `Arc<Slot>`; cached
/// textures hold only the allocation).
pub(crate) struct Slot {
    pub(crate) allocation: Arc<Allocation>,
}

/// A video frame. Cloning is cheap: it shares the same memory.
#[derive(Clone)]
pub struct Frame {
    pub(crate) slot: Arc<Slot>,
    pub(crate) layout: Arc<Layout>,
}

impl Frame {
    pub fn kind(&self) -> Kind {
        self.layout.kind
    }

    pub fn width(&self) -> u32 {
        self.layout.width
    }

    pub fn height(&self) -> u32 {
        self.layout.height
    }

    pub fn layout(&self) -> &Layout {
        &self.layout
    }

    /// Plane `index`'s bytes (BGRA: 0; Y'CbCr: 0 luma, 1 chroma), each row
    /// [`Plane::row_pitch`] apart. Valid once the GPU has finished drawing
    /// into it.
    pub fn plane(&self, index: usize) -> &[u8] {
        let plane = &self.layout.planes[index];
        let start = plane.base + plane.offset;
        let len = plane.row_pitch * (plane.height as usize - 1)
            + plane_row_bytes(self.layout.kind, index, plane.width);
        let mapping = &self.slot.allocation.mapping;
        debug_assert!(start + len <= mapping.len());
        // SAFETY: inside the mapping, which the frame keeps alive.
        unsafe { std::slice::from_raw_parts(mapping.ptr().add(start), len) }
    }
}

/// Bytes of pixels in one row of a plane (no padding).
fn plane_row_bytes(kind: Kind, index: usize, width: u32) -> usize {
    let bytes = match (kind, index) {
        (Kind::Bgra, _) => 4,
        (Kind::YCbCr, 0) => 1,
        (Kind::YCbCr, _) => 2,
    };
    width as usize * bytes
}

static NEXT_ID: AtomicU64 = AtomicU64::new(1);

/// A pool of up to [`MAX_POOL_FRAMES`] frames of one size and kind,
/// allocated (and imported into the GPU) as needed.
pub struct FramePool {
    layout: Arc<Layout>,
    slots: Vec<Arc<Slot>>,
}

impl FramePool {
    pub fn new(kind: Kind, width: u32, height: u32) -> Result<Self> {
        Ok(Self {
            layout: Arc::new(Layout::new(kind, width, height)?),
            slots: Vec::new(),
        })
    }

    /// A slot nothing else holds, or a new one while the pool has room;
    /// `None` if every one is still in use.
    fn free_slot(&mut self) -> Result<Option<usize>> {
        if let Some(index) = self
            .slots
            .iter()
            .position(|slot| Arc::strong_count(slot) == 1)
        {
            return Ok(Some(index));
        }
        if self.slots.len() >= MAX_POOL_FRAMES {
            return Ok(None);
        }
        let mapping = Mapping::new(self.layout.len)?;
        let import = if self.layout.memory_types != 0 {
            Some(gpu::import(
                &Gpu::new()?,
                &mapping,
                self.layout.memory_types,
            )?)
        } else {
            None
        };
        let allocation = Arc::new(Allocation {
            import,
            mapping,
            id: NEXT_ID.fetch_add(1, Ordering::Relaxed),
        });
        self.slots.push(Arc::new(Slot { allocation }));
        Ok(Some(self.slots.len() - 1))
    }

    fn frame(&self, index: usize) -> Frame {
        Frame {
            slot: Arc::clone(&self.slots[index]),
            layout: Arc::clone(&self.layout),
        }
    }
}

impl isocast_engine::FramePool for FramePool {
    type Frame = Frame;

    fn draw(&mut self, draw: &mut dyn FnMut(&mut [u8], usize)) -> Result<Option<Frame>> {
        if self.layout.kind != Kind::Bgra {
            bail!("frame pool: only BGRA frames are drawn on the CPU");
        }
        let Some(index) = self.free_slot()? else {
            return Ok(None);
        };
        let plane = self.layout.planes[0];
        let mapping = &self.slots[index].allocation.mapping;
        let start = plane.base + plane.offset;
        let len = plane.row_pitch * plane.height as usize;
        debug_assert!(start + len <= mapping.len());
        // SAFETY: the pool holds the only reference to this slot, so no
        // frame reads it while it is drawn; inside the mapping.
        let pixels = unsafe { std::slice::from_raw_parts_mut(mapping.ptr().add(start), len) };
        draw(pixels, plane.row_pitch);
        Ok(Some(self.frame(index)))
    }

    fn take(&mut self) -> Result<Option<Frame>> {
        Ok(self.free_slot()?.map(|index| self.frame(index)))
    }
}
