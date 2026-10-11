//! Bounded, timestamped history of compressed video frames.
//!
//! Every retained sequence starts at a keyframe so a saved snapshot can be
//! decoded on its own. The time limit keeps the preceding keyframe; the byte
//! limit is strict and may discard the entire current group of pictures.

use std::collections::VecDeque;

#[derive(Clone)]
pub struct Frame<T> {
    pub sample: T,
    pub pts: f64,
    pub duration: f64,
    pub size: usize,
    pub keyframe: bool,
}

pub struct ReplayBuffer<T> {
    frames: VecDeque<Frame<T>>,
    bytes: usize,
    max_seconds: f64,
    max_bytes: usize,
}

impl<T> ReplayBuffer<T> {
    pub const fn new(max_seconds: f64, max_bytes: usize) -> Self {
        Self {
            frames: VecDeque::new(),
            bytes: 0,
            max_seconds,
            max_bytes,
        }
    }

    /// Returns whether the incoming frame remains in the buffer.
    ///
    /// Frames before the first keyframe are ignored. Invalid input is rejected
    /// without disturbing the history. A backwards timestamp starts a new
    /// history, preventing frames from separate capture sessions being mixed.
    pub fn push(&mut self, frame: Frame<T>) -> bool {
        if !self.max_seconds.is_finite()
            || self.max_seconds <= 0.0
            || self.max_bytes == 0
            || !frame.pts.is_finite()
            || !frame.duration.is_finite()
            || frame.duration < 0.0
            || !(frame.pts + frame.duration).is_finite()
            || frame.size == 0
        {
            return false;
        }

        if self.frames.back().is_some_and(|last| frame.pts < last.pts) {
            self.clear();
        }
        if self.frames.is_empty() && !frame.keyframe {
            return false;
        }
        if frame.size > self.max_bytes {
            self.clear();
            return false;
        }

        // This is only possible for a limit near usize::MAX. Resetting avoids
        // overflowing the accounting and still preserves a decodable prefix.
        if self.bytes.checked_add(frame.size).is_none() {
            self.clear();
            if !frame.keyframe {
                return false;
            }
        }

        self.bytes += frame.size;
        let cutoff = frame.pts - self.max_seconds;
        self.frames.push_back(frame);

        if let Some(start) = self
            .frames
            .iter()
            .rposition(|frame| frame.keyframe && frame.pts <= cutoff)
        {
            self.discard_prefix(start);
        }

        while self.bytes > self.max_bytes {
            match self.frames.iter().skip(1).position(|frame| frame.keyframe) {
                Some(next_keyframe) => self.discard_prefix(next_keyframe + 1),
                None => self.clear(),
            }
        }

        !self.frames.is_empty()
    }

    pub fn clear(&mut self) {
        self.frames.clear();
        self.bytes = 0;
    }

    pub fn len(&self) -> usize {
        self.frames.len()
    }

    pub const fn bytes(&self) -> usize {
        self.bytes
    }

    /// Duration from the first frame's PTS through the final frame's end.
    pub fn duration(&self) -> f64 {
        match (self.frames.front(), self.frames.back()) {
            (Some(first), Some(last)) => (last.pts + last.duration - first.pts).max(0.0),
            _ => 0.0,
        }
    }

    fn discard_prefix(&mut self, count: usize) {
        for _ in 0..count {
            let popped = self.frames.pop_front();
            if let Some(frame) = popped {
                self.bytes -= frame.size;
            }
        }
    }
}

impl<T: Clone> ReplayBuffer<T> {
    /// Takes an independently owned snapshot without stopping live capture.
    pub fn snapshot(&self) -> Vec<Frame<T>> {
        self.frames.iter().cloned().collect()
    }
}

/// A packet of uncompressed playback audio on the video capture timeline.
#[derive(Clone)]
pub struct AudioFrame<T> {
    pub sample: T,
    pub pts: f64,
    pub duration: f64,
    pub size: usize,
}

/// Audio has no keyframe boundary; retain packets by elapsed time instead.
pub struct AudioBuffer<T> {
    packets: VecDeque<AudioFrame<T>>,
    max_seconds: f64,
}

impl<T> AudioBuffer<T> {
    pub const fn new(max_seconds: f64) -> Self {
        Self {
            packets: VecDeque::new(),
            max_seconds,
        }
    }

    pub fn push(&mut self, packet: AudioFrame<T>) {
        let end = packet.pts + packet.duration;
        if !self.max_seconds.is_finite()
            || self.max_seconds <= 0.0
            || !packet.pts.is_finite()
            || !packet.duration.is_finite()
            || packet.duration <= 0.0
            || !end.is_finite()
            || packet.size == 0
        {
            return;
        }
        if self
            .packets
            .back()
            .is_some_and(|last| packet.pts < last.pts)
        {
            self.clear();
        }
        let cutoff = end - self.max_seconds;
        self.packets.push_back(packet);
        while self
            .packets
            .front()
            .is_some_and(|first| first.pts + first.duration <= cutoff)
        {
            self.packets.pop_front();
        }
    }

    pub fn clear(&mut self) {
        self.packets.clear();
    }
}

impl<T: Clone> AudioBuffer<T> {
    /// Export performs sample-accurate trimming of the edge packets.
    pub fn snapshot_between(&self, start: f64, end: f64) -> Vec<AudioFrame<T>> {
        self.packets
            .iter()
            .filter(|packet| packet.pts < end && packet.pts + packet.duration > start)
            .cloned()
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::{AudioBuffer, AudioFrame, Frame, ReplayBuffer};
    use std::{cell::Cell, rc::Rc};

    fn frame(pts: f64, keyframe: bool, size: usize) -> Frame<u64> {
        Frame {
            sample: pts.to_bits(),
            pts,
            duration: 1.0,
            size,
            keyframe,
        }
    }

    fn audio(pts: f64) -> AudioFrame<u64> {
        AudioFrame {
            sample: pts.to_bits(),
            pts,
            duration: 1.0,
            size: 4,
        }
    }

    #[test]
    fn audio_snapshot_keeps_overlapping_edges_and_survives_clear() {
        let mut buffer = AudioBuffer::new(3.0);
        for second in 0..4 {
            buffer.push(audio(f64::from(second)));
        }
        // The packet ending at 1s has expired; edge packets overlap the clip.
        let snapshot = buffer.snapshot_between(1.5, 3.5);
        assert_eq!(
            snapshot.iter().map(|packet| packet.pts).collect::<Vec<_>>(),
            vec![1.0, 2.0, 3.0]
        );
        buffer.clear();
        assert!(buffer.snapshot_between(0.0, 10.0).is_empty());
        assert_eq!(snapshot.len(), 3);
        assert_eq!(snapshot[0].sample, 1.0_f64.to_bits());
        buffer.push(audio(8.0));
        assert_eq!(snapshot[2].pts, 3.0);
    }

    #[test]
    fn audio_rejects_invalid_packets_and_resets_on_a_new_timeline() {
        let mut buffer = AudioBuffer::new(3.0);
        buffer.push(audio(10.0));
        for pts in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            buffer.push(audio(pts));
        }
        let mut empty = audio(11.0);
        empty.size = 0;
        buffer.push(empty);
        for duration in [0.0, -1.0, f64::NAN, f64::INFINITY] {
            let mut invalid = audio(11.0);
            invalid.duration = duration;
            buffer.push(invalid);
        }
        assert_eq!(buffer.snapshot_between(0.0, 20.0).len(), 1);
        buffer.push(audio(0.0));
        let snapshot = buffer.snapshot_between(0.0, 20.0);
        assert_eq!(snapshot.len(), 1);
        assert_eq!(snapshot[0].pts, 0.0);
        for seconds in [0.0, -1.0, f64::NAN, f64::INFINITY] {
            let mut invalid = AudioBuffer::new(seconds);
            invalid.push(audio(0.0));
            assert!(invalid.snapshot_between(0.0, 20.0).is_empty());
        }
    }

    #[test]
    fn ignores_frames_until_first_keyframe() {
        let mut buffer = ReplayBuffer::new(10.0, 100);
        assert!(!buffer.push(frame(0.0, false, 10)));
        assert_eq!(buffer.len(), 0);
        assert_eq!(buffer.bytes(), 0);
        assert_eq!(buffer.duration(), 0.0);
        assert!(buffer.push(frame(1.0, true, 10)));
        assert!(buffer.push(frame(2.0, false, 10)));
        assert_eq!(buffer.len(), 2);
        assert_eq!(buffer.duration(), 2.0);
    }

    #[test]
    fn retains_keyframe_preceding_time_cutoff() {
        let mut buffer = ReplayBuffer::new(5.0, 1000);
        for second in 0..=8 {
            assert!(buffer.push(frame(f64::from(second), second % 2 == 0, 10)));
        }
        let snapshot = buffer.snapshot();
        assert_eq!(snapshot.first().map(|frame| frame.pts), Some(2.0));
        assert!(snapshot.first().is_some_and(|frame| frame.keyframe));
        assert_eq!(buffer.len(), 7);
        assert_eq!(buffer.bytes(), 70);
        assert_eq!(buffer.duration(), 7.0);

        assert!(buffer.push(frame(9.0, false, 10)));
        assert_eq!(buffer.snapshot().first().map(|frame| frame.pts), Some(4.0));
        assert_eq!(buffer.bytes(), 60);
    }

    #[test]
    fn byte_limit_discards_whole_gops_and_recovers_at_keyframe() {
        let mut buffer = ReplayBuffer::new(60.0, 25);
        assert!(buffer.push(frame(0.0, true, 10)));
        assert!(buffer.push(frame(1.0, false, 10)));
        assert!(buffer.push(frame(2.0, true, 10)));
        assert_eq!(buffer.snapshot()[0].pts, 2.0);
        assert_eq!(buffer.len(), 1);
        assert_eq!(buffer.bytes(), 10);

        assert!(buffer.push(frame(3.0, false, 10)));
        assert!(!buffer.push(frame(4.0, false, 10)));
        assert_eq!(buffer.len(), 0);
        assert_eq!(buffer.bytes(), 0);
        assert!(!buffer.push(frame(5.0, false, 10)));
        assert!(buffer.push(frame(6.0, true, 10)));
        assert_eq!(buffer.bytes(), 10);
    }

    #[test]
    fn single_oversized_frame_clears_history() {
        let mut buffer = ReplayBuffer::new(10.0, 10);
        assert!(buffer.push(frame(0.0, true, 10)));
        assert!(!buffer.push(frame(1.0, true, 11)));
        assert_eq!(buffer.len(), 0);
        assert!(!buffer.push(frame(2.0, false, 1)));
        assert!(buffer.push(frame(3.0, true, 1)));
    }

    #[test]
    fn snapshot_survives_new_frames_and_clear() {
        let mut buffer = ReplayBuffer::new(2.0, 100);
        assert!(buffer.push(frame(0.0, true, 10)));
        assert!(buffer.push(frame(1.0, false, 10)));
        let saved = buffer.snapshot();
        assert_eq!(buffer.len(), 2);

        for second in 2..=5 {
            assert!(buffer.push(frame(f64::from(second), second % 2 == 0, 10)));
        }
        assert_eq!(buffer.snapshot()[0].pts, 2.0);
        assert_eq!(saved.len(), 2);
        assert_eq!(saved[0].sample, 0.0_f64.to_bits());
        assert_eq!(saved[1].sample, 1.0_f64.to_bits());
        buffer.clear();
        assert_eq!(buffer.len(), 0);
        assert_eq!(buffer.bytes(), 0);
        assert_eq!(saved.len(), 2);
        assert!(!buffer.push(frame(6.0, false, 10)));
    }

    struct DropProbe(Rc<Cell<usize>>);

    impl Drop for DropProbe {
        fn drop(&mut self) {
            self.0.set(self.0.get() + 1);
        }
    }

    #[test]
    fn clear_and_eviction_release_samples() {
        let drops = Rc::new(Cell::new(0));
        let mut buffer = ReplayBuffer::new(1.0, 100);
        for second in 0..3 {
            assert!(buffer.push(Frame {
                sample: DropProbe(drops.clone()),
                pts: f64::from(second),
                duration: 1.0,
                size: 10,
                keyframe: true,
            }));
        }
        assert_eq!(drops.get(), 1);
        buffer.clear();
        assert_eq!(drops.get(), 3);
        drop(buffer);
        assert_eq!(drops.get(), 3);
    }

    #[test]
    fn backwards_timestamp_starts_new_history() {
        let mut buffer = ReplayBuffer::new(10.0, 100);
        assert!(buffer.push(frame(10.0, true, 10)));
        assert!(buffer.push(frame(11.0, false, 10)));
        assert!(buffer.push(frame(0.0, true, 10)));
        assert_eq!(buffer.len(), 1);
        assert_eq!(buffer.snapshot()[0].pts, 0.0);

        assert!(buffer.push(frame(1.0, false, 10)));
        assert!(!buffer.push(frame(0.5, false, 10)));
        assert_eq!(buffer.len(), 0);
        assert!(buffer.push(frame(2.0, true, 10)));
    }

    #[test]
    fn invalid_frames_preserve_valid_history() {
        let mut buffer = ReplayBuffer::new(10.0, 100);
        assert!(buffer.push(frame(1.0, true, 10)));
        for pts in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            assert!(!buffer.push(frame(pts, true, 10)));
        }
        for duration in [f64::NAN, f64::INFINITY, -1.0] {
            let mut bad = frame(2.0, true, 10);
            bad.duration = duration;
            assert!(!buffer.push(bad));
        }
        assert!(!buffer.push(frame(2.0, true, 0)));
        let mut overflow = frame(f64::MAX, true, 10);
        overflow.duration = f64::MAX;
        assert!(!buffer.push(overflow));
        assert_eq!(buffer.len(), 1);
        assert_eq!(buffer.bytes(), 10);
        assert_eq!(buffer.snapshot()[0].pts, 1.0);
    }

    #[test]
    fn zero_sample_duration_is_supported() {
        let mut buffer = ReplayBuffer::new(10.0, 100);
        let mut first = frame(1.0, true, 10);
        first.duration = 0.0;
        assert!(buffer.push(first));
        assert_eq!(buffer.duration(), 0.0);
    }

    #[test]
    fn invalid_limits_do_not_accumulate_frames() {
        for seconds in [0.0, -1.0, f64::NAN, f64::INFINITY] {
            let mut buffer = ReplayBuffer::new(seconds, 100);
            assert!(!buffer.push(frame(0.0, true, 10)));
            assert_eq!(buffer.bytes(), 0);
        }
        let mut buffer = ReplayBuffer::new(10.0, 0);
        assert!(!buffer.push(frame(0.0, true, 10)));
        assert_eq!(buffer.len(), 0);
    }

    #[test]
    fn byte_accounting_cannot_overflow() {
        let mut buffer = ReplayBuffer::new(10.0, usize::MAX);
        assert!(buffer.push(frame(0.0, true, usize::MAX)));
        assert!(!buffer.push(frame(1.0, false, 1)));
        assert_eq!(buffer.bytes(), 0);
        assert!(buffer.push(frame(2.0, true, usize::MAX)));
        assert!(buffer.push(frame(3.0, true, 1)));
        assert_eq!(buffer.bytes(), 1);
        assert_eq!(buffer.len(), 1);
    }
}
