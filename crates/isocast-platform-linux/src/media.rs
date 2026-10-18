//! What media files hold, read with libavformat.

use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use ffmpeg_next::{codec, format, media};
use isocast_engine::MediaInfo;

/// The picture size, whether there is sound, and the duration of `path`.
pub fn media_info(path: &Path) -> Result<MediaInfo> {
    if !path.exists() {
        bail!("{} does not exist", path.display());
    }
    ffmpeg_next::init().context("couldn't start FFmpeg")?;
    let input = format::input(path).with_context(|| format!("couldn't open {}", path.display()))?;
    let video = match input.streams().best(media::Type::Video) {
        Some(stream) => {
            let decoder = codec::context::Context::from_parameters(stream.parameters())?
                .decoder()
                .video()?;
            Some((decoder.width(), decoder.height()))
        }
        None => None,
    };
    let audio = input.streams().best(media::Type::Audio).is_some();
    if video.is_none() && !audio {
        bail!("{} has no video or audio FFmpeg can read", path.display());
    }
    let micros = input.duration().max(0) as u64;
    Ok(MediaInfo {
        video,
        audio,
        duration: Duration::from_micros(micros),
    })
}

/// Decodes one part of a media file from its start: video into CPU frames
/// (NV12, or BGRA for pictures with alpha or an odd size), audio into
/// interleaved stereo at 48 kHz.
pub struct Reader {
    input: format::context::Input,
    stream: usize,
    time_base: ffmpeg_next::Rational,
    start: i64,
    decoder: Decoder,
    ended: bool,
}

enum Decoder {
    Video {
        decoder: ffmpeg_next::decoder::Video,
        pool: Option<(u32, u32, crate::frames::FramePool, crate::frames::Kind)>,
        scaler: *mut ffmpeg_next::ffi::SwsContext,
        scaler_key: Option<(i32, i32, i32, i32, i32)>,
    },
    Audio {
        decoder: ffmpeg_next::decoder::Audio,
        resampler: Option<ffmpeg_next::software::resampling::Context>,
    },
}

// SAFETY: a reader is used by one thread at a time; the scaler is only
// touched through it.
unsafe impl Send for Reader {}

impl Drop for Reader {
    fn drop(&mut self) {
        if let Decoder::Video { scaler, .. } = &mut self.decoder {
            // SAFETY: made by sws_getContext or null.
            unsafe { ffmpeg_next::ffi::sws_freeContext(*scaler) };
        }
    }
}

pub fn open(path: &Path, part: isocast_engine::MediaPart) -> Result<Reader> {
    ffmpeg_next::init().context("couldn't start FFmpeg")?;
    let input = format::input(path).with_context(|| format!("couldn't open {}", path.display()))?;
    let wanted = match part {
        isocast_engine::MediaPart::Video => media::Type::Video,
        isocast_engine::MediaPart::Audio => media::Type::Audio,
    };
    let stream = input
        .streams()
        .best(wanted)
        .with_context(|| format!("{} has no {wanted:?} track", path.display()))?;
    let index = stream.index();
    let time_base = stream.time_base();
    let start = stream.start_time().max(0);
    let mut context = codec::context::Context::from_parameters(stream.parameters())?;
    // Let the decoder use the machine's cores.
    if let Ok(cores) = std::thread::available_parallelism() {
        context.set_threading(codec::threading::Config {
            kind: codec::threading::Type::Frame,
            count: cores.get().min(16),
        });
    }
    let decoder = match part {
        isocast_engine::MediaPart::Video => Decoder::Video {
            decoder: context.decoder().video()?,
            pool: None,
            scaler: std::ptr::null_mut(),
            scaler_key: None,
        },
        isocast_engine::MediaPart::Audio => Decoder::Audio {
            decoder: context.decoder().audio()?,
            resampler: None,
        },
    };
    Ok(Reader {
        input,
        stream: index,
        time_base,
        start,
        decoder,
        ended: false,
    })
}

impl Reader {
    /// Feed the decoder the next packet of its stream; false at the end.
    fn feed(&mut self) -> Result<bool> {
        if self.ended {
            return Ok(false);
        }
        let mut packet = ffmpeg_next::Packet::empty();
        loop {
            match packet.read(&mut self.input) {
                Ok(()) if packet.stream() == self.stream => {
                    match &mut self.decoder {
                        Decoder::Video { decoder, .. } => decoder.send_packet(&packet)?,
                        Decoder::Audio { decoder, .. } => decoder.send_packet(&packet)?,
                    }
                    return Ok(true);
                }
                Ok(()) => continue,
                Err(ffmpeg_next::Error::Eof) => {
                    match &mut self.decoder {
                        Decoder::Video { decoder, .. } => decoder.send_eof()?,
                        Decoder::Audio { decoder, .. } => decoder.send_eof()?,
                    }
                    self.ended = true;
                    return Ok(true);
                }
                Err(error) => return Err(error.into()),
            }
        }
    }

    fn time(&self, pts: Option<i64>) -> std::time::Duration {
        let ticks = (pts.unwrap_or(self.start) - self.start).max(0) as f64;
        Duration::from_secs_f64(ticks * f64::from(self.time_base))
    }
}

/// Whether a pixel format has an alpha channel.
fn has_alpha(format: ffmpeg_next::ffi::AVPixelFormat) -> bool {
    // SAFETY: a lookup; null for an unknown format.
    let descriptor = unsafe { ffmpeg_next::ffi::av_pix_fmt_desc_get(format) };
    !descriptor.is_null()
        && unsafe { (*descriptor).flags } & ffmpeg_next::ffi::AV_PIX_FMT_FLAG_ALPHA as u64 != 0
}

impl isocast_engine::MediaReader for Reader {
    type Frame = crate::frames::Frame;

    fn next_video(&mut self) -> Result<Option<(crate::frames::Frame, Duration)>> {
        use crate::frames::{FramePool, Kind};
        let mut decoded = ffmpeg_next::frame::Video::empty();
        loop {
            let Decoder::Video { decoder, .. } = &mut self.decoder else {
                bail!("this reader decodes audio");
            };
            match decoder.receive_frame(&mut decoded) {
                Ok(()) => break,
                Err(ffmpeg_next::Error::Eof) => return Ok(None),
                Err(ffmpeg_next::Error::Other { errno }) if errno == libc::EAGAIN => {
                    if !self.feed()? {
                        return Ok(None);
                    }
                }
                Err(error) => return Err(error.into()),
            }
        }
        let at = self.time(decoded.timestamp().or(decoded.pts()));
        let Decoder::Video {
            pool,
            scaler,
            scaler_key,
            ..
        } = &mut self.decoder
        else {
            unreachable!()
        };
        // SAFETY: reads the decoded frame's fields; converts it into the
        // pool's planes with libswscale at their pitches.
        unsafe {
            let raw = &*decoded.as_ptr();
            let (width, height) = (raw.width as u32, raw.height as u32);
            let format: ffmpeg_next::ffi::AVPixelFormat = std::mem::transmute(raw.format);
            let kind = if has_alpha(format) || width % 2 != 0 || height % 2 != 0 {
                Kind::Bgra
            } else {
                Kind::YCbCr
            };
            if pool
                .as_ref()
                .is_none_or(|(w, h, _, k)| (*w, *h, *k) != (width, height, kind))
            {
                *pool = Some((width, height, FramePool::cpu(kind, width, height)?, kind));
            }
            let key = (
                raw.width,
                raw.height,
                raw.format,
                raw.colorspace as i32,
                raw.color_range as i32,
            );
            if *scaler_key != Some(key) {
                ffmpeg_next::ffi::sws_freeContext(*scaler);
                const BILINEAR: std::ffi::c_int = 2;
                let output = match kind {
                    Kind::Bgra => ffmpeg_next::ffi::AVPixelFormat::AV_PIX_FMT_BGRA,
                    Kind::YCbCr => ffmpeg_next::ffi::AVPixelFormat::AV_PIX_FMT_NV12,
                };
                *scaler = ffmpeg_next::ffi::sws_getContext(
                    raw.width,
                    raw.height,
                    format,
                    raw.width,
                    raw.height,
                    output,
                    BILINEAR,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    std::ptr::null(),
                );
                if scaler.is_null() {
                    *scaler_key = None;
                    bail!("can't convert {format:?} video");
                }
                let source = match raw.colorspace {
                    ffmpeg_next::ffi::AVColorSpace::AVCOL_SPC_BT709 => {
                        ffmpeg_next::ffi::SWS_CS_ITU709
                    }
                    ffmpeg_next::ffi::AVColorSpace::AVCOL_SPC_BT2020_NCL
                    | ffmpeg_next::ffi::AVColorSpace::AVCOL_SPC_BT2020_CL => {
                        ffmpeg_next::ffi::SWS_CS_BT2020
                    }
                    ffmpeg_next::ffi::AVColorSpace::AVCOL_SPC_UNSPECIFIED if raw.height >= 720 => {
                        ffmpeg_next::ffi::SWS_CS_ITU709
                    }
                    _ => ffmpeg_next::ffi::SWS_CS_DEFAULT,
                };
                let full = std::ffi::c_int::from(
                    raw.color_range == ffmpeg_next::ffi::AVColorRange::AVCOL_RANGE_JPEG,
                );
                ffmpeg_next::ffi::sws_setColorspaceDetails(
                    *scaler,
                    ffmpeg_next::ffi::sws_getCoefficients(source),
                    full,
                    ffmpeg_next::ffi::sws_getCoefficients(ffmpeg_next::ffi::SWS_CS_ITU709),
                    std::ffi::c_int::from(kind == Kind::Bgra),
                    0,
                    1 << 16,
                    1 << 16,
                );
                *scaler_key = Some(key);
            }
            let scaler = *scaler;
            let (_, _, pool, _) = pool.as_mut().expect("made above");
            let source = [
                raw.data[0].cast_const(),
                raw.data[1].cast_const(),
                raw.data[2].cast_const(),
                raw.data[3].cast_const(),
            ];
            let pitch = [
                raw.linesize[0],
                raw.linesize[1],
                raw.linesize[2],
                raw.linesize[3],
            ];
            let filled = pool.fill(|planes, layout| {
                let mut dest = [std::ptr::null_mut::<u8>(); 4];
                let mut dest_pitch = [0 as std::ffi::c_int; 4];
                for (index, plane) in planes.iter_mut().enumerate() {
                    dest[index] = plane.as_mut_ptr();
                    dest_pitch[index] = layout[index].row_pitch as std::ffi::c_int;
                }
                let rows = ffmpeg_next::ffi::sws_scale(
                    scaler,
                    source.as_ptr(),
                    pitch.as_ptr(),
                    0,
                    height as std::ffi::c_int,
                    dest.as_ptr(),
                    dest_pitch.as_ptr(),
                );
                if rows != height as std::ffi::c_int {
                    bail!("video conversion gave {rows} of {height} rows");
                }
                Ok(())
            })?;
            match filled {
                Some(frame) => Ok(Some((frame, at))),
                // Every frame is in use downstream: the caller asked too
                // fast; it holds them, so this can't make progress.
                None => bail!("every decoded frame is still in use"),
            }
        }
    }

    fn next_audio(&mut self) -> Result<Option<(Vec<f32>, Duration)>> {
        use ffmpeg_next::ChannelLayout;
        use ffmpeg_next::format::Sample;
        use ffmpeg_next::format::sample::Type;
        let mut decoded = ffmpeg_next::frame::Audio::empty();
        loop {
            let Decoder::Audio { decoder, .. } = &mut self.decoder else {
                bail!("this reader decodes video");
            };
            match decoder.receive_frame(&mut decoded) {
                Ok(()) => break,
                Err(ffmpeg_next::Error::Eof) => return Ok(None),
                Err(ffmpeg_next::Error::Other { errno }) if errno == libc::EAGAIN => {
                    if !self.feed()? {
                        return Ok(None);
                    }
                }
                Err(error) => return Err(error.into()),
            }
        }
        let at = self.time(decoded.timestamp().or(decoded.pts()));
        let Decoder::Audio { resampler, .. } = &mut self.decoder else {
            unreachable!()
        };
        if decoded.channel_layout().channels() == 0 {
            decoded.set_channel_layout(ChannelLayout::default(decoded.channels() as i32));
        }
        let resampler = match resampler {
            Some(resampler) => resampler,
            None => resampler.insert(ffmpeg_next::software::resampling::Context::get(
                decoded.format(),
                decoded.channel_layout(),
                decoded.rate(),
                Sample::F32(Type::Packed),
                ChannelLayout::STEREO,
                isocast_engine::SAMPLE_RATE,
            )?),
        };
        let mut output = ffmpeg_next::frame::Audio::empty();
        resampler.run(&decoded, &mut output)?;
        let count = output.samples() * 2;
        let bytes = &output.data(0)[..count * 4];
        let samples = bytes
            .as_chunks::<4>()
            .0
            .iter()
            .map(|b| f32::from_ne_bytes(*b))
            .collect();
        Ok(Some((samples, at)))
    }
}
