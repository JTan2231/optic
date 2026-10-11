//! Windows tray, capture, and media services implemented in Rust.

use std::sync::Arc;

pub mod app;
mod audio;
pub mod capture;
pub mod media;
mod pixels;

pub type FrameCallback = fn(u64, media::EncodedSample);
pub type AudioCallback = fn(u64, media::AudioSample);
pub type EventCallback = fn(u64, i32, &str);
pub type FrameSink = Arc<dyn Fn(u64, media::EncodedSample) + Send + Sync>;
pub type AudioSink = Arc<dyn Fn(u64, media::AudioSample) + Send + Sync>;
