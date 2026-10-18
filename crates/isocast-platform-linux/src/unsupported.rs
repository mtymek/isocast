//! What the Linux platform can't do yet: a resource type with no values,
//! and the error saying so.

use anyhow::{Result, anyhow};
use isocast_engine::{
    AudioEncoder, EncodedVideo, FileWriter, Finished, MediaReader, Time, VideoEncoder,
};

use crate::frames::Frame;

/// The error every operation not built yet returns.
pub fn unsupported(what: &str) -> anyhow::Error {
    anyhow!(
        "{what}: not supported on Linux yet (see PLAN_LINUX.md); this build composites \
         synthetic sources but doesn't capture, encode or record"
    )
}

/// A resource the Linux platform can't make yet (a capture, writer,
/// encoder, media reader or playback). It has no values, so no operation
/// can pretend to have one.
#[derive(Debug)]
pub enum Unsupported {}

impl FileWriter for Unsupported {
    type Frame = Frame;

    fn start(&mut self, _at: Time) -> Result<()> {
        match *self {}
    }

    fn append_video(&mut self, _frame: &Frame, _pts: Time) -> Result<bool> {
        match *self {}
    }

    fn append_audio(&mut self, _track: usize, _samples: &[f32], _pts: Time) -> Result<bool> {
        match *self {}
    }

    fn finish(self, _end: Time) -> Result<Finished> {
        match self {}
    }

    fn cancel(self) {
        match self {}
    }
}

impl MediaReader for Unsupported {
    type Frame = Frame;

    fn next_video(&mut self) -> Result<Option<(Frame, std::time::Duration)>> {
        match *self {}
    }

    fn next_audio(&mut self) -> Result<Option<(Vec<f32>, std::time::Duration)>> {
        match *self {}
    }
}

impl VideoEncoder for Unsupported {
    type Frame = Frame;

    fn encode(&mut self, _frame: &Frame, _pts: Time, _keyframe: bool) -> Result<()> {
        match *self {}
    }

    fn receive(&mut self) -> Result<Option<EncodedVideo>> {
        match *self {}
    }

    fn receive_timeout(&mut self, _timeout: std::time::Duration) -> Result<Option<EncodedVideo>> {
        match *self {}
    }

    fn dropped(&self) -> u64 {
        match *self {}
    }

    fn set_rate(&mut self, _fps: u32, _bitrate_kbps: u32) -> Result<()> {
        match *self {}
    }
}

impl AudioEncoder for Unsupported {
    fn encode(&mut self, _samples: &[f32]) -> Result<Vec<Vec<u8>>> {
        match *self {}
    }

    fn delay(&self) -> u32 {
        match *self {}
    }
}
