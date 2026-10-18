//! Camera pictures (MJPEG, YUYV, NV12) into the frames' 8-bit 4:2:0
//! Y'CbCr: Rec. 709, video range.

use std::ffi::{CString, c_int};
use std::ptr;

use anyhow::{Result, bail};
use ffmpeg_next::ffi;

use crate::frames::Plane;

/// Converts one camera's pictures; keeps its decoder and scaler between
/// pictures.
pub struct Converter {
    width: u32,
    height: u32,
    decoder: *mut ffi::AVCodecContext,
    packet: *mut ffi::AVPacket,
    decoded: *mut ffi::AVFrame,
    scaler: *mut ffi::SwsContext,
    /// The source format the scaler was made for.
    scaler_format: Option<ffi::AVPixelFormat>,
}

// SAFETY: one thread (the camera's) uses a converter at a time.
unsafe impl Send for Converter {}

impl Converter {
    pub fn new(width: u32, height: u32) -> Self {
        Self {
            width,
            height,
            decoder: ptr::null_mut(),
            packet: ptr::null_mut(),
            decoded: ptr::null_mut(),
            scaler: ptr::null_mut(),
            scaler_format: None,
        }
    }

    /// Write `data`, a `fourcc` picture, into the NV12 `planes`.
    pub fn convert(
        &mut self,
        fourcc: &str,
        data: &[u8],
        planes: &mut [&mut [u8]],
        layout: &[Plane],
    ) -> Result<()> {
        let (width, height) = (self.width as usize, self.height as usize);
        match fourcc {
            "NV12" => {
                let luma = width * height;
                if data.len() < luma * 3 / 2 {
                    bail!("camera: short NV12 picture ({} bytes)", data.len());
                }
                // Cameras give NV12 in video range; Rec. 601 versus 709 is
                // left as is (the difference is small and cameras rarely
                // say).
                for (index, rows) in [(0, height), (1, height / 2)] {
                    let source = &data[index * luma..];
                    let pitch = layout[index].row_pitch;
                    for row in 0..rows {
                        planes[index][row * pitch..row * pitch + width]
                            .copy_from_slice(&source[row * width..(row + 1) * width]);
                    }
                }
                Ok(())
            }
            "YUYV" => {
                if data.len() < width * height * 2 {
                    bail!("camera: short YUYV picture ({} bytes)", data.len());
                }
                let source = [data.as_ptr(), ptr::null(), ptr::null(), ptr::null()];
                let pitch = [(width * 2) as c_int, 0, 0, 0];
                // SAFETY: the source covers the picture at its pitch.
                unsafe {
                    self.scale(
                        ffi::AVPixelFormat::AV_PIX_FMT_YUYV422,
                        false,
                        &source,
                        &pitch,
                        planes,
                        layout,
                    )
                }
            }
            "MJPG" => self.mjpeg(data, planes, layout),
            other => bail!("camera: the {other} pixel format isn't supported on Linux yet"),
        }
    }

    fn mjpeg(&mut self, data: &[u8], planes: &mut [&mut [u8]], layout: &[Plane]) -> Result<()> {
        // SAFETY: the decoder, packet and frame are made once and freed in
        // Drop; the packet points at `data` only for this call.
        unsafe {
            if self.decoder.is_null() {
                let name = CString::new("mjpeg")?;
                let codec = ffi::avcodec_find_decoder_by_name(name.as_ptr());
                if codec.is_null() {
                    bail!("this FFmpeg has no MJPEG decoder");
                }
                self.decoder = ffi::avcodec_alloc_context3(codec);
                if self.decoder.is_null()
                    || ffi::avcodec_open2(self.decoder, codec, ptr::null_mut()) < 0
                {
                    bail!("couldn't open the MJPEG decoder");
                }
                self.packet = ffi::av_packet_alloc();
                self.decoded = ffi::av_frame_alloc();
            }
            (*self.packet).data = data.as_ptr().cast_mut();
            (*self.packet).size = data.len() as c_int;
            let sent = ffi::avcodec_send_packet(self.decoder, self.packet);
            (*self.packet).data = ptr::null_mut();
            (*self.packet).size = 0;
            if sent < 0 {
                bail!("camera: an MJPEG picture didn't decode");
            }
            if ffi::avcodec_receive_frame(self.decoder, self.decoded) < 0 {
                bail!("camera: an MJPEG picture didn't decode");
            }
            let frame = &*self.decoded;
            if (frame.width as u32, frame.height as u32) != (self.width, self.height) {
                bail!(
                    "camera: a {}x{} MJPEG picture from a {}x{} stream",
                    frame.width,
                    frame.height,
                    self.width,
                    self.height
                );
            }
            // `yuvj` formats are full range; MJPEG from cameras always is.
            let format: ffi::AVPixelFormat = std::mem::transmute(frame.format);
            let source = [
                frame.data[0].cast_const(),
                frame.data[1].cast_const(),
                frame.data[2].cast_const(),
                frame.data[3].cast_const(),
            ];
            let pitch = [
                frame.linesize[0],
                frame.linesize[1],
                frame.linesize[2],
                frame.linesize[3],
            ];
            let result = self.scale(format, true, &source, &pitch, planes, layout);
            ffi::av_frame_unref(self.decoded);
            result
        }
    }

    /// Convert with libswscale from `format` (Rec. 601, `full_range` or
    /// video range) to NV12 Rec. 709 video range.
    ///
    /// # Safety
    /// `source` and `pitch` must describe a picture of the converter's size.
    unsafe fn scale(
        &mut self,
        format: ffi::AVPixelFormat,
        full_range: bool,
        source: &[*const u8; 4],
        pitch: &[c_int; 4],
        planes: &mut [&mut [u8]],
        layout: &[Plane],
    ) -> Result<()> {
        let (width, height) = (self.width as c_int, self.height as c_int);
        // SAFETY: per the caller; the scaler is freed in Drop.
        unsafe {
            if self.scaler_format != Some(format) {
                ffi::sws_freeContext(self.scaler);
                // SWS_BILINEAR: an enum in FFmpeg 7.1+, a macro before.
                const BILINEAR: c_int = 2;
                self.scaler = ffi::sws_getContext(
                    width,
                    height,
                    format,
                    width,
                    height,
                    ffi::AVPixelFormat::AV_PIX_FMT_NV12,
                    BILINEAR,
                    ptr::null_mut(),
                    ptr::null_mut(),
                    ptr::null(),
                );
                if self.scaler.is_null() {
                    self.scaler_format = None;
                    bail!("camera: couldn't convert {format:?} pictures");
                }
                ffi::sws_setColorspaceDetails(
                    self.scaler,
                    ffi::sws_getCoefficients(ffi::SWS_CS_DEFAULT),
                    c_int::from(full_range),
                    ffi::sws_getCoefficients(ffi::SWS_CS_ITU709),
                    0,
                    0,
                    1 << 16,
                    1 << 16,
                );
                self.scaler_format = Some(format);
            }
            let dest = [
                planes[0].as_mut_ptr(),
                planes[1].as_mut_ptr(),
                ptr::null_mut(),
                ptr::null_mut(),
            ];
            let dest_pitch = [
                layout[0].row_pitch as c_int,
                layout[1].row_pitch as c_int,
                0,
                0,
            ];
            let rows = ffi::sws_scale(
                self.scaler,
                source.as_ptr(),
                pitch.as_ptr(),
                0,
                height,
                dest.as_ptr(),
                dest_pitch.as_ptr(),
            );
            if rows != height {
                bail!("camera: conversion gave {rows} of {height} rows");
            }
        }
        Ok(())
    }
}

impl Drop for Converter {
    fn drop(&mut self) {
        // SAFETY: frees what was made; each free takes null.
        unsafe {
            ffi::avcodec_free_context(&mut self.decoder);
            ffi::av_packet_free(&mut self.packet);
            ffi::av_frame_free(&mut self.decoded);
            ffi::sws_freeContext(self.scaler);
        }
    }
}
