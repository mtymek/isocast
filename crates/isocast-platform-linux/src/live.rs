//! Real-time encoders for streams and monitors: H.264 at a bitrate with
//! in-band parameter sets, and AAC-LC.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use isocast_engine::{
    AudioEncoder, EncodedVideo, LiveAudioSpec, LiveVideoSpec, ParameterSets, Time, VideoEncoder,
};

use crate::codec_config::split_annex_b;
use crate::encode::{self, Codec, Rate};
use crate::frames::Frame;

/// Encoder ticks are microseconds.
const TIMESCALE: u32 = 1_000_000;

pub struct LiveVideo {
    encoder: encode::VideoEncoder,
    ready: VecDeque<EncodedVideo>,
    spec: LiveVideoSpec,
    last_tick: Option<i64>,
}

impl LiveVideo {
    pub fn new(spec: &LiveVideoSpec) -> Result<Self> {
        if !spec.width.is_multiple_of(2) || !spec.height.is_multiple_of(2) {
            bail!("live H.264: {}x{} must be even", spec.width, spec.height);
        }
        let encoder = encode::VideoEncoder::new(
            Codec::H264,
            spec.width,
            spec.height,
            spec.fps.max(1),
            TIMESCALE,
            Rate::Bitrate {
                kbps: spec.bitrate_kbps.max(1),
                constant: spec.constant_bitrate,
                keyframe_interval: spec.keyframe_interval.max(1),
                low_latency: spec.low_latency,
            },
        )?;
        Ok(Self {
            encoder,
            ready: VecDeque::new(),
            spec: *spec,
            last_tick: None,
        })
    }

    /// Move what the encoder finished into `ready`, as length-prefixed NAL
    /// units with the SPS and PPS of keyframes set apart.
    fn collect(&mut self) -> Result<()> {
        while let Some(packet) = self.encoder.receive()? {
            let mut data = Vec::with_capacity(packet.data.len() + 16);
            let mut sets = ParameterSets::default();
            for nal in split_annex_b(&packet.data) {
                match nal[0] & 0x1f {
                    7 => sets.sps.push(nal.to_vec()),
                    8 => sets.pps.push(nal.to_vec()),
                    _ => {
                        data.extend_from_slice(&(nal.len() as u32).to_be_bytes());
                        data.extend_from_slice(nal);
                    }
                }
            }
            let parameter_sets = (packet.key && !sets.sps.is_empty()).then_some(sets);
            self.ready.push_back(EncodedVideo {
                data,
                pts: Time::from_nanos(packet.pts * 1000),
                dts: Time::from_nanos(packet.dts * 1000),
                keyframe: packet.key,
                parameter_sets,
            });
        }
        Ok(())
    }
}

impl VideoEncoder for LiveVideo {
    type Frame = Frame;

    fn encode(&mut self, frame: &Frame, pts: Time, keyframe: bool) -> Result<()> {
        let mut tick = pts.as_nanos() / 1000;
        // Strictly increasing, as encoders need.
        if let Some(last) = self.last_tick
            && tick <= last
        {
            tick = last + 1;
        }
        self.last_tick = Some(tick);
        self.encoder.send_key(frame, tick, keyframe)?;
        self.collect()
    }

    fn receive(&mut self) -> Result<Option<EncodedVideo>> {
        self.collect()?;
        Ok(self.ready.pop_front())
    }

    fn receive_timeout(&mut self, timeout: Duration) -> Result<Option<EncodedVideo>> {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(encoded) = self.receive()? {
                return Ok(Some(encoded));
            }
            if Instant::now() >= deadline {
                return Ok(None);
            }
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    fn dropped(&self) -> u64 {
        0
    }

    /// The encoders here keep their bitrate for the session; a new rate
    /// applies to the next session (the engine opens one on a size change).
    fn set_rate(&mut self, fps: u32, bitrate_kbps: u32) -> Result<()> {
        self.spec.fps = fps;
        self.spec.bitrate_kbps = bitrate_kbps;
        Ok(())
    }
}

pub struct LiveAudio {
    encoder: encode::AudioEncoder,
}

impl LiveAudio {
    pub fn new(spec: &LiveAudioSpec) -> Result<Self> {
        Ok(Self {
            encoder: encode::AudioEncoder::new(spec.channels, Some(spec.bitrate_kbps))?,
        })
    }
}

impl AudioEncoder for LiveAudio {
    fn encode(&mut self, samples: &[f32]) -> Result<Vec<Vec<u8>>> {
        self.encoder.send(samples)?;
        let mut frames = Vec::new();
        while let Some(packet) = self.encoder.receive()? {
            frames.push(packet.data);
        }
        Ok(frames)
    }

    fn delay(&self) -> u32 {
        self.encoder.priming
    }
}
