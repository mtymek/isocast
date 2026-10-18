//! Video and audio encoders over libavcodec: H.264 and HEVC on VA-API
//! (the GPU's media engine) when a render node offers it, else libx264 for
//! H.264; AAC-LC with FFmpeg's own encoder.

use std::ffi::{CStr, CString, c_int};
use std::ptr;
use std::sync::OnceLock;

use anyhow::{Context, Result, bail};
use ffmpeg_next::ffi;

use crate::frames::{Frame, Kind};

/// An FFmpeg error code as text.
fn error_text(code: c_int) -> String {
    let mut buffer = [0 as std::ffi::c_char; 128];
    // SAFETY: writes at most the buffer's length, NUL-terminated.
    unsafe { ffi::av_strerror(code, buffer.as_mut_ptr(), buffer.len()) };
    // SAFETY: av_strerror always NUL-terminates.
    unsafe { CStr::from_ptr(buffer.as_ptr()) }
        .to_string_lossy()
        .into_owned()
}

fn check(code: c_int, what: &str) -> Result<c_int> {
    if code < 0 {
        bail!("{what}: {}", error_text(code));
    }
    Ok(code)
}

fn quiet() {
    static ONCE: OnceLock<()> = OnceLock::new();
    ONCE.get_or_init(|| {
        // SAFETY: sets a global level.
        unsafe { ffi::av_log_set_level(ffi::AV_LOG_FATAL) };
    });
}

/// The first `/dev/dri/renderD*` node VA-API opens, found once.
fn vaapi_node() -> Option<&'static CStr> {
    static NODE: OnceLock<Option<CString>> = OnceLock::new();
    NODE.get_or_init(|| {
        quiet();
        let mut nodes: Vec<_> = std::fs::read_dir("/dev/dri")
            .ok()?
            .filter_map(|entry| entry.ok()?.file_name().into_string().ok())
            .filter(|name| name.starts_with("renderD"))
            .collect();
        nodes.sort();
        nodes.into_iter().find_map(|name| {
            let path = CString::new(format!("/dev/dri/{name}")).ok()?;
            let mut device = ptr::null_mut();
            // SAFETY: opens and frees a device context.
            let code = unsafe {
                ffi::av_hwdevice_ctx_create(
                    &mut device,
                    ffi::AVHWDeviceType::AV_HWDEVICE_TYPE_VAAPI,
                    path.as_ptr(),
                    ptr::null_mut(),
                    0,
                )
            };
            // SAFETY: frees what av_hwdevice_ctx_create made, if anything.
            unsafe { ffi::av_buffer_unref(&mut device) };
            (code >= 0).then_some(path)
        })
    })
    .as_deref()
}

/// The hardware video encoders this machine can open, named as `doctor`
/// matches them (`H.264 (VA-API, /dev/dri/renderD129)`). Each is opened
/// once, small, to be sure; the answer is kept for the process.
pub fn hardware_encoders() -> Vec<String> {
    static FOUND: OnceLock<Vec<String>> = OnceLock::new();
    FOUND
        .get_or_init(|| {
            let Some(node) = vaapi_node() else {
                return Vec::new();
            };
            [("H.264", "h264_vaapi"), ("HEVC", "hevc_vaapi")]
                .into_iter()
                .filter(|(_, name)| {
                    VideoEncoder::open(name, Some(node), 256, 256, 30, 30_000, 70).is_ok()
                })
                .map(|(label, _)| format!("{label} (VA-API, {})", node.to_string_lossy()))
                .collect()
        })
        .clone()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Codec {
    H264,
    Hevc,
}

/// One compressed picture: Annex B, pts and dts on the encoder's timescale.
pub struct Packet {
    pub data: Vec<u8>,
    pub pts: i64,
    pub dts: i64,
    pub key: bool,
}

/// A libavcodec video encoder taking 8-bit 4:2:0 frames.
pub struct VideoEncoder {
    context: *mut ffi::AVCodecContext,
    /// VA-API frames to upload into; null for a software encoder.
    hw_frames: *mut ffi::AVBufferRef,
    packet: *mut ffi::AVPacket,
    /// What it runs on, like `h264_vaapi (/dev/dri/renderD129)`.
    pub name: String,
    width: u32,
    height: u32,
    /// How it was opened, to open a twin (`parameter_sets`).
    opened: Opened,
    /// BGRA to NV12 conversion, made on first use.
    scaler: *mut ffi::SwsContext,
    nv12: Vec<u8>,
    /// Whether any picture went in: draining a VA-API encoder that never
    /// got one crashes inside libavcodec 63.
    sent: bool,
}

#[derive(Clone)]
struct Opened {
    codec: &'static str,
    node: Option<&'static CStr>,
    fps: u32,
    timescale: u32,
    quality: u8,
}

// SAFETY: one thread uses an encoder at a time (the writer's).
unsafe impl Send for VideoEncoder {}

impl VideoEncoder {
    /// `quality` is Isocast's 1–100 scale; `timescale` ticks per second.
    pub fn new(
        codec: Codec,
        width: u32,
        height: u32,
        fps: u32,
        timescale: u32,
        quality: u8,
    ) -> Result<Self> {
        quiet();
        let mut failures = Vec::new();
        if let Some(node) = vaapi_node() {
            let name = match codec {
                Codec::H264 => "h264_vaapi",
                Codec::Hevc => "hevc_vaapi",
            };
            match Self::open(name, Some(node), width, height, fps, timescale, quality) {
                Ok(encoder) => return Ok(encoder),
                Err(error) => failures.push(format!("{name}: {error:#}")),
            }
        } else {
            failures.push("VA-API: no render node opens".into());
        }
        if codec == Codec::H264 {
            match Self::open("libx264", None, width, height, fps, timescale, quality) {
                Ok(encoder) => return Ok(encoder),
                Err(error) => failures.push(format!("libx264: {error:#}")),
            }
        }
        bail!(
            "no {codec:?} encoder works on this machine ({})",
            failures.join("; ")
        )
    }

    fn open(
        name: &'static str,
        node: Option<&'static CStr>,
        width: u32,
        height: u32,
        fps: u32,
        timescale: u32,
        quality: u8,
    ) -> Result<Self> {
        let cname = CString::new(name)?;
        // SAFETY: a lookup by name.
        let codec = unsafe { ffi::avcodec_find_encoder_by_name(cname.as_ptr()) };
        if codec.is_null() {
            bail!("this FFmpeg has no {name} encoder");
        }
        // SAFETY: allocates a context for `codec`; owned by `encoder` below,
        // which frees it on every path.
        let context = unsafe { ffi::avcodec_alloc_context3(codec) };
        if context.is_null() {
            bail!("couldn't allocate a {name} context");
        }
        let mut encoder = Self {
            context,
            hw_frames: ptr::null_mut(),
            // SAFETY: allocates a packet; freed in Drop.
            packet: unsafe { ffi::av_packet_alloc() },
            name: match node {
                Some(node) => format!("{name} ({})", node.to_string_lossy()),
                None => name.to_string(),
            },
            width,
            height,
            opened: Opened {
                codec: name,
                node,
                fps,
                timescale,
                quality,
            },
            scaler: ptr::null_mut(),
            nv12: Vec::new(),
            sent: false,
        };
        // Constant QP on Isocast's 1–100 scale: 70 (high) is QP 22.
        let qp = (40.0 - f64::from(quality.clamp(1, 100)) * 0.25).round() as i64;
        // SAFETY: configures the context before opening it.
        unsafe {
            let c = &mut *context;
            c.width = width as c_int;
            c.height = height as c_int;
            c.time_base = ffi::AVRational {
                num: 1,
                den: timescale as c_int,
            };
            c.framerate = ffi::AVRational {
                num: fps as c_int,
                den: 1,
            };
            c.gop_size = fps as c_int;
            c.max_b_frames = 0;
            c.flags |= ffi::AV_CODEC_FLAG_GLOBAL_HEADER as c_int;
            c.color_primaries = ffi::AVColorPrimaries::AVCOL_PRI_BT709;
            c.color_trc = ffi::AVColorTransferCharacteristic::AVCOL_TRC_BT709;
            c.colorspace = ffi::AVColorSpace::AVCOL_SPC_BT709;
            c.color_range = ffi::AVColorRange::AVCOL_RANGE_MPEG;
            let set = |key: &str, value: &str| {
                let (key, value) = (CString::new(key).unwrap(), CString::new(value).unwrap());
                ffi::av_opt_set(c.priv_data, key.as_ptr(), value.as_ptr(), 0)
            };
            if let Some(node) = node {
                let mut device = ptr::null_mut();
                check(
                    ffi::av_hwdevice_ctx_create(
                        &mut device,
                        ffi::AVHWDeviceType::AV_HWDEVICE_TYPE_VAAPI,
                        node.as_ptr(),
                        ptr::null_mut(),
                        0,
                    ),
                    "opening VA-API",
                )?;
                let frames = ffi::av_hwframe_ctx_alloc(device);
                ffi::av_buffer_unref(&mut device);
                if frames.is_null() {
                    bail!("couldn't allocate VA-API frames");
                }
                encoder.hw_frames = frames;
                let f = &mut *((*frames).data as *mut ffi::AVHWFramesContext);
                f.format = ffi::AVPixelFormat::AV_PIX_FMT_VAAPI;
                f.sw_format = ffi::AVPixelFormat::AV_PIX_FMT_NV12;
                f.width = width as c_int;
                f.height = height as c_int;
                f.initial_pool_size = 8;
                check(ffi::av_hwframe_ctx_init(frames), "setting up VA-API frames")?;
                c.pix_fmt = ffi::AVPixelFormat::AV_PIX_FMT_VAAPI;
                c.hw_frames_ctx = ffi::av_buffer_ref(frames);
                set("rc_mode", "CQP");
                set("qp", &qp.to_string());
            } else {
                c.pix_fmt = ffi::AVPixelFormat::AV_PIX_FMT_NV12;
                set("preset", "veryfast");
                set("crf", &qp.to_string());
            }
            check(
                ffi::avcodec_open2(context, codec, ptr::null_mut()),
                &format!("opening {name}"),
            )?;
        }
        Ok(encoder)
    }

    /// The encoder's parameter sets (VPS/SPS/PPS), Annex B.
    pub fn extradata(&self) -> &[u8] {
        // SAFETY: an open context; extradata lives as long as it.
        unsafe {
            let c = &*self.context;
            if c.extradata.is_null() {
                return &[];
            }
            std::slice::from_raw_parts(c.extradata, c.extradata_size as usize)
        }
    }

    /// The parameter sets (VPS/SPS/PPS) its stream will carry. Some
    /// encoders' in-band sets differ from their extradata (VA-API writes
    /// extradata before rate control sets its PPS), so a twin encoder makes
    /// one black picture and its sets are taken; extradata only when the
    /// picture carries none (libx264 with global headers).
    pub fn parameter_sets(&self) -> Result<Vec<Vec<u8>>> {
        let o = self.opened.clone();
        let mut twin = Self::open(
            o.codec,
            o.node,
            self.width,
            self.height,
            o.fps,
            o.timescale,
            o.quality,
        )?;
        twin.send_black()?;
        twin.sent = true;
        twin.flush()?;
        let mut sets = Vec::new();
        while let Some(packet) = twin.receive()? {
            sets = crate::codec_config::split_annex_b(&packet.data)
                .into_iter()
                .filter(|nal| {
                    crate::codec_config::is_parameter_set(nal, o.codec.starts_with("hevc"))
                })
                .map(<[u8]>::to_vec)
                .collect();
            if !sets.is_empty() || packet.key {
                break;
            }
        }
        if sets.is_empty() {
            sets = crate::codec_config::split_annex_b(self.extradata())
                .into_iter()
                .map(<[u8]>::to_vec)
                .collect();
        }
        Ok(sets)
    }

    /// Encode one black picture at time 0 (for `parameter_sets`).
    fn send_black(&mut self) -> Result<()> {
        let (width, height) = (self.width as usize, self.height as usize);
        let luma = vec![16u8; width * height];
        let chroma = vec![128u8; width * height / 2];
        // SAFETY: wraps the two vectors, which outlive the call; freed below.
        unsafe {
            let mut sw = ffi::av_frame_alloc();
            let s = &mut *sw;
            s.format = ffi::AVPixelFormat::AV_PIX_FMT_NV12 as c_int;
            s.width = width as c_int;
            s.height = height as c_int;
            s.data[0] = luma.as_ptr().cast_mut();
            s.linesize[0] = width as c_int;
            s.data[1] = chroma.as_ptr().cast_mut();
            s.linesize[1] = width as c_int;
            s.pts = 0;
            let result = self.send_frame(sw);
            ffi::av_frame_free(&mut sw);
            result
        }
    }

    /// Send a software NV12 frame, uploading it first for VA-API.
    ///
    /// # Safety
    /// `sw` must be a valid NV12 frame of the encoder's size.
    unsafe fn send_frame(&mut self, sw: *mut ffi::AVFrame) -> Result<()> {
        // SAFETY: per the caller; the hardware frame is freed on every path.
        unsafe {
            if self.hw_frames.is_null() {
                return check(
                    ffi::avcodec_send_frame(self.context, sw),
                    "sending a frame to the encoder",
                )
                .map(drop);
            }
            let mut hw = ffi::av_frame_alloc();
            let result = check(
                ffi::av_hwframe_get_buffer(self.hw_frames, hw, 0),
                "getting a VA-API surface",
            )
            .and_then(|_| {
                check(
                    ffi::av_hwframe_transfer_data(hw, sw, 0),
                    "uploading a frame to VA-API",
                )
            })
            .and_then(|_| {
                (*hw).pts = (*sw).pts;
                check(
                    ffi::avcodec_send_frame(self.context, hw),
                    "sending a frame to the encoder",
                )
            });
            ffi::av_frame_free(&mut hw);
            result.map(drop)
        }
    }

    /// Encode `frame` at `pts` (encoder ticks). BGRA frames are converted
    /// to Y'CbCr on the CPU first (the engine records from Y'CbCr pools;
    /// BGRA comes from callers that don't).
    pub fn send(&mut self, frame: &Frame, pts: i64) -> Result<()> {
        if (frame.width(), frame.height()) != (self.width, self.height) {
            bail!(
                "a {}x{} frame for a {}x{} encoder",
                frame.width(),
                frame.height(),
                self.width,
                self.height
            );
        }
        let (planes, pitches): ([*const u8; 2], [usize; 2]) = match frame.kind() {
            Kind::YCbCr => {
                frame.sync()?;
                let layout = frame.layout();
                (
                    [frame.plane(0).as_ptr(), frame.plane(1).as_ptr()],
                    [layout.planes[0].row_pitch, layout.planes[1].row_pitch],
                )
            }
            Kind::Bgra => {
                self.convert_to_nv12(frame)?;
                let width = self.width as usize;
                let luma = width * self.height as usize;
                (
                    [self.nv12.as_ptr(), self.nv12[luma..].as_ptr()],
                    [width, width],
                )
            }
        };
        // SAFETY: the frame wraps the planes without owning them (no
        // buffer, so libavcodec copies what it keeps); they outlive the
        // calls below. The AVFrame is freed on every path.
        unsafe {
            let mut sw = ffi::av_frame_alloc();
            let s = &mut *sw;
            s.format = ffi::AVPixelFormat::AV_PIX_FMT_NV12 as c_int;
            s.width = self.width as c_int;
            s.height = self.height as c_int;
            for index in 0..2 {
                s.data[index] = planes[index].cast_mut();
                s.linesize[index] = pitches[index] as c_int;
            }
            s.pts = pts;
            s.color_range = ffi::AVColorRange::AVCOL_RANGE_MPEG;
            s.colorspace = ffi::AVColorSpace::AVCOL_SPC_BT709;
            let result = self.send_frame(sw);
            ffi::av_frame_free(&mut sw);
            self.sent |= result.is_ok();
            result
        }
    }

    /// Convert a BGRA frame into `self.nv12` (packed rows), Rec. 709 video
    /// range.
    fn convert_to_nv12(&mut self, frame: &Frame) -> Result<()> {
        let (width, height) = (self.width as c_int, self.height as c_int);
        // SAFETY: the context is made once for this size and freed in Drop;
        // source and destination cover `height` rows at their pitches.
        unsafe {
            if self.scaler.is_null() {
                // SWS_BILINEAR: an enum in FFmpeg 7.1+, a macro before.
                const BILINEAR: c_int = 2;
                self.scaler = ffi::sws_getContext(
                    width,
                    height,
                    ffi::AVPixelFormat::AV_PIX_FMT_BGRA,
                    width,
                    height,
                    ffi::AVPixelFormat::AV_PIX_FMT_NV12,
                    BILINEAR,
                    ptr::null_mut(),
                    ptr::null_mut(),
                    ptr::null(),
                );
                if self.scaler.is_null() {
                    bail!("couldn't set up BGRA to Y'CbCr conversion");
                }
                let table = ffi::sws_getCoefficients(ffi::SWS_CS_ITU709);
                ffi::sws_setColorspaceDetails(self.scaler, table, 1, table, 0, 0, 1 << 16, 1 << 16);
                self.nv12 = vec![0; self.width as usize * self.height as usize * 3 / 2];
            }
            let luma = self.width as usize * self.height as usize;
            let source = [
                frame.plane(0).as_ptr(),
                ptr::null(),
                ptr::null(),
                ptr::null(),
            ];
            let source_pitch = [frame.layout().planes[0].row_pitch as c_int, 0, 0, 0];
            let dest = [
                self.nv12.as_mut_ptr(),
                self.nv12.as_mut_ptr().add(luma),
                ptr::null_mut(),
                ptr::null_mut(),
            ];
            let dest_pitch = [width, width, 0, 0];
            let rows = ffi::sws_scale(
                self.scaler,
                source.as_ptr(),
                source_pitch.as_ptr(),
                0,
                height,
                dest.as_ptr(),
                dest_pitch.as_ptr(),
            );
            if rows != height {
                bail!("BGRA to Y'CbCr conversion gave {rows} of {height} rows");
            }
        }
        Ok(())
    }

    /// No more frames: the encoder gives out what it holds.
    pub fn flush(&mut self) -> Result<()> {
        if !self.sent {
            return Ok(());
        }
        // SAFETY: a null frame starts draining.
        let code = unsafe { ffi::avcodec_send_frame(self.context, ptr::null()) };
        if code < 0 && code != ffi::AVERROR_EOF {
            check(code, "draining the encoder")?;
        }
        Ok(())
    }

    /// The next finished picture, if any.
    pub fn receive(&mut self) -> Result<Option<Packet>> {
        // SAFETY: an open context and an allocated packet, unreferenced
        // after it is copied out.
        unsafe {
            let code = ffi::avcodec_receive_packet(self.context, self.packet);
            if code == ffi::AVERROR(libc::EAGAIN) || code == ffi::AVERROR_EOF {
                return Ok(None);
            }
            check(code, "receiving a picture from the encoder")?;
            let p = &*self.packet;
            let packet = Packet {
                data: std::slice::from_raw_parts(p.data, p.size as usize).to_vec(),
                pts: p.pts,
                dts: p.dts,
                key: p.flags & ffi::AV_PKT_FLAG_KEY as c_int != 0,
            };
            ffi::av_packet_unref(self.packet);
            Ok(Some(packet))
        }
    }
}

impl Drop for VideoEncoder {
    fn drop(&mut self) {
        // SAFETY: frees what `open` allocated; each free takes null.
        unsafe {
            ffi::avcodec_free_context(&mut self.context);
            ffi::av_buffer_unref(&mut self.hw_frames);
            ffi::av_packet_free(&mut self.packet);
            ffi::sws_freeContext(self.scaler);
        }
    }
}

/// One compressed AAC packet: raw (no ADTS), timed in samples from the
/// stream's pre-roll origin (so the first is at 0, not `-priming`).
pub struct AudioPacket {
    pub data: Vec<u8>,
    pub time: u64,
    pub samples: u32,
}

/// FFmpeg's AAC-LC encoder at 48 kHz, taking interleaved samples.
pub struct AudioEncoder {
    context: *mut ffi::AVCodecContext,
    packet: *mut ffi::AVPacket,
    channels: usize,
    /// Interleaved samples not yet a whole frame.
    pending: Vec<f32>,
    /// Samples sent so far.
    sent: i64,
    pub priming: u32,
}

// SAFETY: one thread uses an encoder at a time (the writer's).
unsafe impl Send for AudioEncoder {}

const AAC_FRAME: usize = 1024;

impl AudioEncoder {
    pub fn new(channels: u32) -> Result<Self> {
        quiet();
        let name = CString::new("aac")?;
        // SAFETY: a lookup by name.
        let codec = unsafe { ffi::avcodec_find_encoder_by_name(name.as_ptr()) };
        if codec.is_null() {
            bail!("this FFmpeg has no AAC encoder");
        }
        // SAFETY: allocates a context owned by `encoder` below.
        let context = unsafe { ffi::avcodec_alloc_context3(codec) };
        if context.is_null() {
            bail!("couldn't allocate an AAC context");
        }
        let mut encoder = Self {
            context,
            // SAFETY: freed in Drop.
            packet: unsafe { ffi::av_packet_alloc() },
            channels: channels as usize,
            pending: Vec::new(),
            sent: 0,
            priming: 0,
        };
        // SAFETY: configures and opens the context.
        unsafe {
            let c = &mut *context;
            c.sample_fmt = ffi::AVSampleFormat::AV_SAMPLE_FMT_FLTP;
            c.sample_rate = 48_000;
            ffi::av_channel_layout_default(&mut c.ch_layout, channels as c_int);
            c.bit_rate = if channels == 1 { 128_000 } else { 192_000 };
            c.time_base = ffi::AVRational {
                num: 1,
                den: 48_000,
            };
            c.flags |= ffi::AV_CODEC_FLAG_GLOBAL_HEADER as c_int;
            check(
                ffi::avcodec_open2(context, codec, ptr::null_mut()),
                "opening the AAC encoder",
            )?;
            if c.frame_size as usize != AAC_FRAME {
                bail!("the AAC encoder wants {}-sample frames", c.frame_size);
            }
            encoder.priming = c.initial_padding.max(0) as u32;
        }
        Ok(encoder)
    }

    /// Queue interleaved samples; whole frames go to the encoder.
    pub fn send(&mut self, samples: &[f32]) -> Result<()> {
        self.pending.extend_from_slice(samples);
        let whole = AAC_FRAME * self.channels;
        while self.pending.len() >= whole {
            let frame: Vec<f32> = self.pending.drain(..whole).collect();
            self.encode(&frame, AAC_FRAME)?;
        }
        Ok(())
    }

    /// The last partial frame, then drain.
    pub fn flush(&mut self) -> Result<()> {
        if !self.pending.is_empty() {
            let count = self.pending.len() / self.channels;
            let mut frame = std::mem::take(&mut self.pending);
            frame.resize(AAC_FRAME * self.channels, 0.0);
            self.encode(&frame, count)?;
        }
        // SAFETY: a null frame starts draining.
        let code = unsafe { ffi::avcodec_send_frame(self.context, ptr::null()) };
        if code < 0 && code != ffi::AVERROR_EOF {
            check(code, "draining the AAC encoder")?;
        }
        Ok(())
    }

    fn encode(&mut self, interleaved: &[f32], count: usize) -> Result<()> {
        // SAFETY: a frame with its own planar buffers, filled from
        // `interleaved` and freed after sending (libavcodec takes a ref).
        unsafe {
            let mut frame = ffi::av_frame_alloc();
            let f = &mut *frame;
            f.format = ffi::AVSampleFormat::AV_SAMPLE_FMT_FLTP as c_int;
            f.sample_rate = 48_000;
            f.nb_samples = count as c_int;
            ffi::av_channel_layout_copy(&mut f.ch_layout, &(*self.context).ch_layout);
            let result =
                check(ffi::av_frame_get_buffer(frame, 0), "allocating audio").and_then(|_| {
                    for channel in 0..self.channels {
                        let plane =
                            std::slice::from_raw_parts_mut(f.data[channel].cast::<f32>(), count);
                        for (i, sample) in plane.iter_mut().enumerate() {
                            *sample = interleaved[i * self.channels + channel];
                        }
                    }
                    f.pts = self.sent;
                    self.sent += count as i64;
                    check(
                        ffi::avcodec_send_frame(self.context, frame),
                        "sending audio to the AAC encoder",
                    )
                });
            ffi::av_frame_free(&mut frame);
            result.map(drop)
        }
    }

    /// The next finished packet, if any.
    pub fn receive(&mut self) -> Result<Option<AudioPacket>> {
        // SAFETY: as for video.
        unsafe {
            let code = ffi::avcodec_receive_packet(self.context, self.packet);
            if code == ffi::AVERROR(libc::EAGAIN) || code == ffi::AVERROR_EOF {
                return Ok(None);
            }
            check(code, "receiving AAC")?;
            let p = &*self.packet;
            let time = p.pts + i64::from(self.priming);
            let packet = AudioPacket {
                data: std::slice::from_raw_parts(p.data, p.size as usize).to_vec(),
                time: u64::try_from(time).context("AAC packet before the stream's start")?,
                samples: p.duration.clamp(1, AAC_FRAME as i64) as u32,
            };
            ffi::av_packet_unref(self.packet);
            Ok(Some(packet))
        }
    }
}

impl Drop for AudioEncoder {
    fn drop(&mut self) {
        // SAFETY: frees what `new` allocated.
        unsafe {
            ffi::avcodec_free_context(&mut self.context);
            ffi::av_packet_free(&mut self.packet);
        }
    }
}
