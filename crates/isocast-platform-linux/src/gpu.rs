//! Frames shown to the GPU without copying: each plane is a linear Vulkan
//! image bound to the frame's own memory, imported once per frame buffer
//! with `VK_EXT_external_memory_host`.
//!
//! Images start `UNINITIALIZED` to wgpu, which moves them out of
//! `VK_IMAGE_LAYOUT_UNDEFINED` on first use. The Vulkan spec lets a driver
//! discard contents on that transition; for linear images Mesa's drivers
//! keep them, and the `gpu` contract checks drawn pixels survive.

use std::collections::HashMap;
use std::sync::{Arc, Weak};

use anyhow::{Context, Result, bail};
use ash::vk;
use isocast_engine::gpu::{FrameImporter, Gpu, GpuFrame, GpuPixels, Matrix, YCbCr};
use wgpu::hal;

use crate::frames::{Frame, Kind, Layout, Mapping, PAGE, Plane, Slot};

/// The Vulkan pieces a frame import needs.
struct Vulkan {
    device: ash::Device,
    instance: ash::Instance,
    physical: vk::PhysicalDevice,
    host: ash::ext::external_memory_host::Device,
}

/// The GPU's Vulkan device, if it has host memory import enabled.
fn vulkan(gpu: &Gpu) -> Option<Vulkan> {
    // SAFETY: only reads the device's handles, which live as long as it.
    let device = unsafe { gpu.device.as_hal::<hal::api::Vulkan>() }?;
    if !device
        .enabled_device_extensions()
        .contains(&isocast_engine::gpu::VULKAN_HOST_MEMORY)
    {
        return None;
    }
    let instance = device.shared_instance().raw_instance().clone();
    let raw = device.raw_device().clone();
    let host = ash::ext::external_memory_host::Device::new(&instance, &raw);
    Some(Vulkan {
        device: raw,
        instance,
        physical: device.raw_physical_device(),
        host,
    })
}

/// Whether frames can be shown to `gpu` without copying.
pub(crate) fn host_memory(gpu: &Gpu) -> bool {
    vulkan(gpu).is_some()
}

fn vk_format(kind: Kind, plane: usize) -> (vk::Format, wgpu::TextureFormat) {
    match (kind, plane) {
        (Kind::Bgra, _) => (vk::Format::B8G8R8A8_UNORM, wgpu::TextureFormat::Bgra8Unorm),
        (Kind::YCbCr, 0) => (vk::Format::R8_UNORM, wgpu::TextureFormat::R8Unorm),
        (Kind::YCbCr, _) => (vk::Format::R8G8_UNORM, wgpu::TextureFormat::Rg8Unorm),
    }
}

/// A linear image for one plane, ready to bind to host memory.
fn create_image(vulkan: &Vulkan, format: vk::Format, width: u32, height: u32) -> Result<vk::Image> {
    let mut external = vk::ExternalMemoryImageCreateInfo::default()
        .handle_types(vk::ExternalMemoryHandleTypeFlags::HOST_ALLOCATION_EXT);
    let info = vk::ImageCreateInfo::default()
        .image_type(vk::ImageType::TYPE_2D)
        .format(format)
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
    // SAFETY: a valid create info on a live device.
    unsafe { vulkan.device.create_image(&info, None) }
        .with_context(|| format!("couldn't make a linear {format:?} {width}x{height} image"))
}

/// The GPU's layout for `kind` frames: each plane's row pitch and size,
/// planes on page boundaries, and the memory types they may use.
pub(crate) fn layout(gpu: &Gpu, kind: Kind, width: u32, height: u32) -> Result<Layout> {
    let vulkan = vulkan(gpu).context("the GPU can't use frames in CPU memory")?;
    let sizes: &[(u32, u32)] = match kind {
        Kind::Bgra => &[(width, height)],
        Kind::YCbCr => &[(width, height), (width / 2, height / 2)],
    };
    let mut planes = Vec::new();
    let mut memory_types = u32::MAX;
    let mut base = 0usize;
    for (index, &(width, height)) in sizes.iter().enumerate() {
        let (format, _) = vk_format(kind, index);
        let image = create_image(&vulkan, format, width, height)?;
        // SAFETY: the image just made; destroyed right after.
        let (requirements, subresource) = unsafe {
            let requirements = vulkan.device.get_image_memory_requirements(image);
            let subresource = vulkan.device.get_image_subresource_layout(
                image,
                vk::ImageSubresource {
                    aspect_mask: vk::ImageAspectFlags::COLOR,
                    mip_level: 0,
                    array_layer: 0,
                },
            );
            vulkan.device.destroy_image(image, None);
            (requirements, subresource)
        };
        let alignment = (requirements.alignment as usize).max(PAGE);
        base = base.next_multiple_of(alignment);
        planes.push(Plane {
            base,
            offset: subresource.offset as usize,
            row_pitch: subresource.row_pitch as usize,
            size: requirements.size as usize,
            width,
            height,
        });
        base += requirements.size as usize;
        memory_types &= requirements.memory_type_bits;
    }
    if memory_types == 0 {
        bail!("the GPU has no memory type for every plane of a {kind:?} frame");
    }
    Layout::finish(kind, width, height, planes, memory_types)
}

/// Whether `gpu` is a discrete GPU (frames in CPU memory sit across PCIe).
pub(crate) fn discrete(gpu: &Gpu) -> bool {
    gpu.device.adapter_info().device_type == wgpu::DeviceType::DiscreteGpu
}

/// A frame's planes in GPU memory, drawn into by the GPU and copied to the
/// frame's CPU memory when a consumer needs the pixels there.
pub(crate) struct Shadow {
    device: wgpu::Device,
    queue: wgpu::Queue,
    pub(crate) planes: Vec<wgpu::Texture>,
    staging: Vec<wgpu::Buffer>,
    /// The GPU drew since the last copy down.
    dirty: std::sync::Mutex<bool>,
}

// SAFETY: every field is a wgpu handle (`Send` and `Sync`) or a mutex.
// Said explicitly because proving it through wgpu-core's types overflows
// the trait solver in isocastd (see `Owner`).
unsafe impl Send for Shadow {}
unsafe impl Sync for Shadow {}

impl Shadow {
    pub(crate) fn new(gpu: &Gpu, layout: &Layout) -> Result<Self> {
        let mut planes = Vec::new();
        let mut staging = Vec::new();
        for (index, plane) in layout.planes.iter().enumerate() {
            let (_, format) = vk_format(layout.kind, index);
            planes.push(gpu.device.create_texture(&wgpu::TextureDescriptor {
                label: Some("frame shadow"),
                size: wgpu::Extent3d {
                    width: plane.width,
                    height: plane.height,
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format,
                usage: wgpu::TextureUsages::RENDER_ATTACHMENT
                    | wgpu::TextureUsages::TEXTURE_BINDING
                    | wgpu::TextureUsages::COPY_SRC
                    | wgpu::TextureUsages::COPY_DST,
                view_formats: &[],
            }));
            staging.push(gpu.device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("frame shadow download"),
                size: (plane.row_pitch * plane.height as usize) as u64,
                usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
                mapped_at_creation: false,
            }));
        }
        Ok(Self {
            device: gpu.device.clone(),
            queue: gpu.queue.clone(),
            planes,
            staging,
            dirty: std::sync::Mutex::new(false),
        })
    }

    /// The GPU is about to draw into the shadow.
    pub(crate) fn drawn(&self) {
        if let Ok(mut dirty) = self.dirty.lock() {
            *dirty = true;
        }
    }

    /// Copy the shadow into `mapping` if the GPU drew since the last copy.
    pub(crate) fn download(&self, layout: &Layout, mapping: &Mapping) -> Result<()> {
        let mut dirty = self
            .dirty
            .lock()
            .map_err(|_| anyhow::anyhow!("a frame download panicked"))?;
        if !*dirty {
            return Ok(());
        }
        let device = &self.device;
        let mut encoder = device.create_command_encoder(&Default::default());
        for ((texture, buffer), plane) in self.planes.iter().zip(&self.staging).zip(&layout.planes)
        {
            encoder.copy_texture_to_buffer(
                texture.as_image_copy(),
                wgpu::TexelCopyBufferInfo {
                    buffer,
                    layout: wgpu::TexelCopyBufferLayout {
                        offset: 0,
                        bytes_per_row: Some(plane.row_pitch as u32),
                        rows_per_image: Some(plane.height),
                    },
                },
                texture.size(),
            );
        }
        let submission = self.queue.submit([encoder.finish()]);
        let (sender, receiver) = std::sync::mpsc::channel();
        for buffer in &self.staging {
            let sender = sender.clone();
            buffer
                .slice(..)
                .map_async(wgpu::MapMode::Read, move |result| drop(sender.send(result)));
        }
        drop(sender);
        device
            .poll(wgpu::PollType::Wait {
                submission_index: Some(submission),
                timeout: Some(std::time::Duration::from_secs(2)),
            })
            .context("waiting for a frame to copy down from the GPU")?;
        for result in receiver.iter() {
            result.context("mapping a frame's download")?;
        }
        for (buffer, plane) in self.staging.iter().zip(&layout.planes) {
            let data = buffer
                .slice(..)
                .get_mapped_range()
                .map_err(|error| anyhow::anyhow!("reading a frame's download: {error}"))?;
            let start = plane.base + plane.offset;
            debug_assert!(start + data.len() <= mapping.len());
            // SAFETY: inside the frame's mapping; readers of this frame
            // wait on `dirty`, which is held.
            unsafe {
                std::ptr::copy_nonoverlapping(data.as_ptr(), mapping.ptr().add(start), data.len());
            }
            drop(data);
            buffer.unmap();
        }
        *dirty = false;
        Ok(())
    }
}

/// A frame buffer's memory, imported into the GPU.
pub(crate) struct HostImport {
    device: ash::Device,
    pub(crate) memory: vk::DeviceMemory,
    /// The wgpu device it belongs to, kept so the handles stay valid.
    pub(crate) owner: Owner,
}

/// A `wgpu::Device`, behind a type the compiler needn't look into: proving
/// a frame `Send` and `Sync` through wgpu-core's types overflows the trait
/// solver in isocastd.
pub(crate) struct Owner(pub(crate) wgpu::Device);

// SAFETY: `wgpu::Device` is `Send` and `Sync`.
unsafe impl Send for Owner {}
unsafe impl Sync for Owner {}

impl Drop for HostImport {
    fn drop(&mut self) {
        // SAFETY: every image bound to it holds the allocation, so none is
        // left when this runs.
        unsafe { self.device.free_memory(self.memory, None) };
    }
}

/// Import `mapping` into `gpu` for images of `memory_types`.
pub(crate) fn import(gpu: &Gpu, mapping: &Mapping, memory_types: u32) -> Result<HostImport> {
    let vulkan = vulkan(gpu).context("the GPU can't use frames in CPU memory")?;
    let handle = vk::ExternalMemoryHandleTypeFlags::HOST_ALLOCATION_EXT;
    let mut properties = vk::MemoryHostPointerPropertiesEXT::default();
    // SAFETY: a page-aligned pointer to a live mapping.
    unsafe {
        (vulkan.host.fp().get_memory_host_pointer_properties_ext)(
            vulkan.device.handle(),
            handle,
            mapping.ptr().cast(),
            &mut properties,
        )
        .result()
    }
    .context("couldn't ask the GPU about a frame's memory")?;
    let types = properties.memory_type_bits & memory_types;
    if types == 0 {
        bail!(
            "the GPU can't bind frame images to CPU memory (host pointer types {:#x}, image \
             types {memory_types:#x})",
            properties.memory_type_bits
        );
    }
    // Prefer memory the CPU sees without flushing.
    // SAFETY: reads the physical device's memory types.
    let memory = unsafe {
        vulkan
            .instance
            .get_physical_device_memory_properties(vulkan.physical)
    };
    let usable: Vec<u32> = (0..memory.memory_type_count)
        .filter(|index| types & (1 << index) != 0)
        .collect();
    let index = usable
        .iter()
        .copied()
        .find(|&index| {
            memory.memory_types[index as usize]
                .property_flags
                .contains(vk::MemoryPropertyFlags::HOST_COHERENT)
        })
        .or(usable.first().copied())
        .context("no usable memory type")?;
    let mut host = vk::ImportMemoryHostPointerInfoEXT::default()
        .handle_type(handle)
        .host_pointer(mapping.ptr().cast());
    let info = vk::MemoryAllocateInfo::default()
        .allocation_size(mapping.len() as u64)
        .memory_type_index(index)
        .push_next(&mut host);
    // SAFETY: imports the live mapping, which outlives the import (see
    // `Allocation`).
    let memory = unsafe { vulkan.device.allocate_memory(&info, None) }
        .context("couldn't import a frame's memory into the GPU")?;
    Ok(HostImport {
        device: vulkan.device,
        memory,
        owner: Owner(gpu.device.clone()),
    })
}

/// What a texture keeps alive until wgpu destroys it.
enum Keep {
    /// The frame: it stays out of its pool (uncached imports).
    Frame { _slot: Arc<Slot> },
    /// Only its memory (cached textures; the caller holds the frame).
    Memory {
        _allocation: Arc<crate::frames::Allocation>,
    },
}

/// Plane `index` of `frame` as a texture bound to its memory.
fn texture(
    gpu: &Gpu,
    frame: &Frame,
    index: usize,
    target: bool,
    keep: Keep,
) -> Result<wgpu::Texture> {
    let allocation = &frame.slot.allocation;
    let import = allocation
        .import
        .as_ref()
        .context("this frame isn't in GPU-visible memory (no Vulkan host memory import)")?;
    if import.owner.0 != gpu.device {
        bail!("this frame's memory belongs to an earlier GPU device");
    }
    let vulkan = vulkan(gpu).context("the GPU can't use frames in CPU memory")?;
    let plane = frame.layout.planes[index];
    let (format, wgpu_format) = vk_format(frame.layout.kind, index);
    let image = create_image(&vulkan, format, plane.width, plane.height)?;
    // SAFETY: the image was made with the layout `plane` describes, and the
    // memory holds it at `base`.
    if let Err(error) = unsafe {
        vulkan
            .device
            .bind_image_memory(image, import.memory, plane.base as u64)
    } {
        // SAFETY: the unbound image just made.
        unsafe { vulkan.device.destroy_image(image, None) };
        return Err(error).context("couldn't bind a frame image to its memory");
    }
    let size = wgpu::Extent3d {
        width: plane.width,
        height: plane.height,
        depth_or_array_layers: 1,
    };
    let usage = wgpu::TextureUsages::TEXTURE_BINDING
        | wgpu::TextureUsages::COPY_SRC
        | wgpu::TextureUsages::COPY_DST
        | if target {
            wgpu::TextureUsages::RENDER_ATTACHMENT
        } else {
            wgpu::TextureUsages::empty()
        };
    let destroy = vulkan.device.clone();
    let drop_callback: hal::DropCallback = Box::new(move || {
        // SAFETY: wgpu calls this once the GPU is done with the image.
        unsafe { destroy.destroy_image(image, None) };
        drop(keep);
    });
    // SAFETY: a texture of `gpu`'s device, made to `desc`; the image stays
    // valid until the drop callback, the memory longer (`keep`).
    let texture = unsafe {
        let hal_device = gpu
            .device
            .as_hal::<hal::api::Vulkan>()
            .context("the GPU isn't a Vulkan device")?;
        let hal_texture = hal_device.texture_from_raw(
            image,
            &hal::TextureDescriptor {
                label: Some("frame"),
                size,
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: wgpu_format,
                usage: wgpu::TextureUses::RESOURCE
                    | wgpu::TextureUses::COLOR_TARGET
                    | wgpu::TextureUses::COPY_SRC
                    | wgpu::TextureUses::COPY_DST,
                memory_flags: hal::MemoryFlags::empty(),
                view_formats: Vec::new(),
            },
            Some(drop_callback),
            hal::vulkan::TextureMemory::External,
        );
        drop(hal_device);
        gpu.device.create_texture_from_hal::<hal::api::Vulkan>(
            hal_texture,
            &wgpu::TextureDescriptor {
                label: Some("frame"),
                size,
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: wgpu_format,
                usage,
                view_formats: &[],
            },
            wgpu::TextureUses::UNINITIALIZED,
        )
    };
    Ok(texture)
}

/// Frames' video range, Rec. 709 Y'CbCr coding.
const CODING: YCbCr = YCbCr {
    matrix: Matrix::Bt709,
    full_range: false,
    bits: 8,
};

fn pixels(frame: &Frame, planes: Vec<wgpu::Texture>) -> GpuFrame {
    let mut planes = planes.into_iter();
    let first = planes.next().expect("a frame has a plane");
    GpuFrame {
        width: frame.width(),
        height: frame.height(),
        pixels: match planes.next() {
            None => GpuPixels::Rgb(first),
            Some(chroma) => GpuPixels::YCbCr {
                luma: first,
                chroma,
                coding: CODING,
            },
        },
    }
}

/// Textures kept for a frame buffer: valid while its slot is in its pool.
struct Cached {
    slot: Weak<Slot>,
    planes: Vec<wgpu::Texture>,
}

/// Shows Linux frames to one GPU device.
pub struct Importer {
    gpu: Gpu,
    cache: HashMap<u64, Cached>,
}

impl Importer {
    pub fn new(gpu: &Gpu) -> Result<Self> {
        if !host_memory(gpu) {
            bail!(
                "showing frames to the GPU needs Vulkan with VK_EXT_external_memory_host; this \
                 GPU's driver doesn't offer it"
            );
        }
        Ok(Self {
            gpu: gpu.clone(),
            cache: HashMap::new(),
        })
    }

    fn uncached(&self, frame: &Frame, target: bool) -> Result<Vec<wgpu::Texture>> {
        (0..frame.layout.planes.len())
            .map(|index| {
                texture(
                    &self.gpu,
                    frame,
                    index,
                    target,
                    Keep::Frame {
                        _slot: Arc::clone(&frame.slot),
                    },
                )
            })
            .collect()
    }

    fn cached(&mut self, frame: &Frame) -> Result<Vec<wgpu::Texture>> {
        // Forget buffers their pools have let go of.
        self.cache
            .retain(|_, cached| cached.slot.strong_count() > 0);
        let id = frame.slot.allocation.id;
        if let Some(cached) = self.cache.get(&id) {
            return Ok(cached.planes.clone());
        }
        let planes = (0..frame.layout.planes.len())
            .map(|index| {
                texture(
                    &self.gpu,
                    frame,
                    index,
                    true,
                    Keep::Memory {
                        _allocation: Arc::clone(&frame.slot.allocation),
                    },
                )
            })
            .collect::<Result<Vec<_>>>()?;
        self.cache.insert(
            id,
            Cached {
                slot: Arc::downgrade(&frame.slot),
                planes: planes.clone(),
            },
        );
        Ok(planes)
    }

    /// New textures holding what `planes` hold, copied on the GPU.
    fn copy_of(&self, planes: &[wgpu::Texture]) -> Vec<wgpu::Texture> {
        let mut encoder = self.gpu.device.create_command_encoder(&Default::default());
        let copies = planes
            .iter()
            .map(|plane| {
                let copy = self.gpu.device.create_texture(&wgpu::TextureDescriptor {
                    label: Some("frame import"),
                    size: plane.size(),
                    mip_level_count: 1,
                    sample_count: 1,
                    dimension: wgpu::TextureDimension::D2,
                    format: plane.format(),
                    usage: wgpu::TextureUsages::TEXTURE_BINDING
                        | wgpu::TextureUsages::COPY_SRC
                        | wgpu::TextureUsages::COPY_DST,
                    view_formats: &[],
                });
                encoder.copy_texture_to_texture(
                    plane.as_image_copy(),
                    copy.as_image_copy(),
                    plane.size(),
                );
                copy
            })
            .collect();
        self.gpu.queue.submit([encoder.finish()]);
        copies
    }

    fn check(frame: &Frame, kind: Kind) -> Result<()> {
        if frame.kind() != kind {
            bail!(
                "a {:?} frame where a {kind:?} frame was expected",
                frame.kind()
            );
        }
        Ok(())
    }
}

impl FrameImporter for Importer {
    type Frame = Frame;

    fn import(&mut self, frame: &Frame) -> Result<GpuFrame> {
        if let Some(shadow) = &frame.slot.shadow {
            // A copy of its own, so the frame can go back to its pool.
            let planes = self.copy_of(&shadow.planes);
            return Ok(pixels(frame, planes));
        }
        let planes = self.uncached(frame, false)?;
        Ok(pixels(frame, planes))
    }

    fn target(&mut self, frame: &Frame) -> Result<wgpu::Texture> {
        Self::check(frame, Kind::Bgra)?;
        Ok(self.uncached(frame, true)?.remove(0))
    }

    fn target_cached(&mut self, frame: &Frame) -> Result<Option<wgpu::Texture>> {
        Self::check(frame, Kind::Bgra)?;
        Ok(Some(self.cached(frame)?.remove(0)))
    }

    fn ycbcr_target(&mut self, frame: &Frame) -> Result<(wgpu::Texture, wgpu::Texture)> {
        Self::check(frame, Kind::YCbCr)?;
        if let Some(shadow) = &frame.slot.shadow {
            shadow.drawn();
            return Ok((shadow.planes[0].clone(), shadow.planes[1].clone()));
        }
        let mut planes = self.uncached(frame, true)?;
        let chroma = planes.remove(1);
        Ok((planes.remove(0), chroma))
    }

    fn import_cached(&mut self, frame: &Frame) -> Result<Option<GpuFrame>> {
        if let Some(shadow) = &frame.slot.shadow {
            return Ok(Some(pixels(frame, shadow.planes.clone())));
        }
        let planes = self.cached(frame)?;
        Ok(Some(pixels(frame, planes)))
    }

    fn ycbcr_target_cached(
        &mut self,
        frame: &Frame,
    ) -> Result<Option<(wgpu::Texture, wgpu::Texture)>> {
        Self::check(frame, Kind::YCbCr)?;
        if let Some(shadow) = &frame.slot.shadow {
            shadow.drawn();
            return Ok(Some((shadow.planes[0].clone(), shadow.planes[1].clone())));
        }
        let mut planes = self.cached(frame)?;
        let chroma = planes.remove(1);
        Ok(Some((planes.remove(0), chroma)))
    }
}
