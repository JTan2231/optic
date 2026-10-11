//! Windows tray, capture, and media services implemented in Rust.

use std::sync::Arc;

pub mod app;
pub mod capture;
pub mod media;
mod pixels;

pub type FrameCallback = fn(u64, media::EncodedSample);
pub type EventCallback = fn(u64, i32, &str);
pub type FrameSink = Arc<dyn Fn(u64, media::EncodedSample) + Send + Sync>;
