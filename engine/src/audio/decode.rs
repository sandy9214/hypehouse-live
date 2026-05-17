//! Decode service — control-thread side.
//!
//! ADR-004 §"Why not put the reducer on the audio thread?" + §"Open
//! implementation questions" both call out: **decoding stays on the
//! control thread**. The audio thread only consumes pre-decoded `f32`
//! buffers.
//!
//! For v0.1 this is a stub that fabricates a sine-wave buffer per
//! requested track. Real `symphonia` decode lands in a later PR; the
//! contract is identical: `decode_track` returns a [`BufferId`] you can
//! hand to the audio thread inside an `AudioCommandKind::DeckLoadBuffer`.
//!
//! The audio thread looks up `BufferId → Arc<DecodedBuffer>` in a
//! separately-published registry (see [`BufferRegistry`]). That lookup
//! is wait-free.

use std::collections::HashMap;
use std::sync::Arc;

use crate::audio::command::BufferId;

/// A pre-decoded mono `f32` buffer the audio thread can render from.
/// `Arc` so we can hand cheap clones out to the audio thread without
/// copying samples.
#[derive(Clone)]
pub struct DecodedBuffer {
    pub samples: Arc<Vec<f32>>,
    pub sample_rate: u32,
}

/// Trait so the translator can be parameterized over real / stub /
/// mock decode without leaking implementation details.
pub trait DecodeService: Send {
    /// Synchronously decode (or fetch from cache) a buffer for the
    /// given track. Returns the registry handle. For real decode this
    /// will return a placeholder id immediately and post the real
    /// `DeckLoadBuffer` command once decode completes; the stub does
    /// the work inline.
    fn decode_track(&mut self, track_id: &str, path: &str, sample_rate: u32) -> BufferId;

    /// Look up a buffer by id. Returns `None` if the id is unknown.
    fn buffer(&self, id: BufferId) -> Option<DecodedBuffer>;
}

/// Stub decode service: generates a 1-second 440Hz sine wave for any
/// requested track and caches it by `track_id`. Adequate for the v0.1
/// audio-thread wire-up; real decode comes in a follow-up PR.
pub struct StubDecodeService {
    next_id: u32,
    by_track: HashMap<String, BufferId>,
    buffers: HashMap<u32, DecodedBuffer>,
}

impl Default for StubDecodeService {
    fn default() -> Self {
        Self::new()
    }
}

impl StubDecodeService {
    pub fn new() -> Self {
        Self {
            next_id: 1, // 0 reserved as sentinel
            by_track: HashMap::new(),
            buffers: HashMap::new(),
        }
    }

    /// Allocate a fresh 1-second 440Hz sine buffer at the given sample
    /// rate.
    fn make_sine(sample_rate: u32) -> DecodedBuffer {
        let n = sample_rate as usize;
        let mut samples = Vec::with_capacity(n);
        let two_pi = std::f32::consts::TAU;
        let freq = 440.0_f32;
        for i in 0..n {
            let t = i as f32 / sample_rate as f32;
            samples.push((two_pi * freq * t).sin() * 0.2);
        }
        DecodedBuffer {
            samples: Arc::new(samples),
            sample_rate,
        }
    }
}

impl DecodeService for StubDecodeService {
    fn decode_track(&mut self, track_id: &str, _path: &str, sample_rate: u32) -> BufferId {
        if let Some(existing) = self.by_track.get(track_id) {
            return *existing;
        }
        let id = BufferId(self.next_id);
        self.next_id += 1;
        let buf = Self::make_sine(sample_rate);
        self.buffers.insert(id.0, buf);
        self.by_track.insert(track_id.to_string(), id);
        id
    }

    fn buffer(&self, id: BufferId) -> Option<DecodedBuffer> {
        self.buffers.get(&id.0).cloned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stub_decode_caches_by_track_id() {
        let mut svc = StubDecodeService::new();
        let id1 = svc.decode_track("song-a", "/a.mp3", 48_000);
        let id2 = svc.decode_track("song-a", "/a.mp3", 48_000);
        assert_eq!(id1, id2);
    }

    #[test]
    fn stub_decode_generates_one_second_buffer() {
        let mut svc = StubDecodeService::new();
        let id = svc.decode_track("song-b", "/b.mp3", 48_000);
        let buf = svc.buffer(id).expect("buffer should be retrievable");
        assert_eq!(buf.samples.len(), 48_000);
        assert_eq!(buf.sample_rate, 48_000);
    }

    #[test]
    fn unknown_buffer_id_returns_none() {
        let svc = StubDecodeService::new();
        assert!(svc.buffer(BufferId(999)).is_none());
    }
}
