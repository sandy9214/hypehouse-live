//! Lock-free SPSC ring buffer between control thread and audio thread.
//!
//! Thin wrapper over `ringbuf::HeapRb`. Capacity 1024 per ADR-004
//! §"Open implementation questions" — a busy live set is ~10 events/s, so
//! 1024 = ~100s of buffering. Plenty of headroom even under MIDI flood.
//!
//! Allocation lives entirely inside [`AudioRing::new`]. Once `split()` has
//! handed out the producer + consumer, neither side allocates again on
//! push / pop.

use ringbuf::{
    traits::{Consumer, Observer, Producer, Split},
    HeapCons, HeapProd, HeapRb,
};

use crate::audio::AudioCommand;

/// Capacity of the control→audio command ring (ADR-004).
pub const RING_CAPACITY: usize = 1024;

/// Owning handle to a freshly constructed SPSC ring. Call [`AudioRing::split`]
/// to get the producer (control thread) + consumer (audio thread).
pub struct AudioRing {
    rb: HeapRb<AudioCommand>,
}

/// Producer end — owned by the control thread.
pub struct AudioProducer {
    inner: HeapProd<AudioCommand>,
}

/// Consumer end — owned by the audio thread.
pub struct AudioConsumer {
    inner: HeapCons<AudioCommand>,
}

impl Default for AudioRing {
    fn default() -> Self {
        Self::new()
    }
}

impl AudioRing {
    /// Allocate a fresh ring with the standard capacity. Heap allocation
    /// happens **here only**; push/pop are wait-free thereafter.
    pub fn new() -> Self {
        Self {
            rb: HeapRb::new(RING_CAPACITY),
        }
    }

    /// Split into producer + consumer. The producer goes to the control
    /// thread; the consumer to the audio thread.
    pub fn split(self) -> (AudioProducer, AudioConsumer) {
        let (prod, cons) = self.rb.split();
        (AudioProducer { inner: prod }, AudioConsumer { inner: cons })
    }
}

impl AudioProducer {
    /// Try to push a command. Returns the command back as `Err(cmd)` if
    /// the ring is full (ADR-004 §"State-log → command translation":
    /// control thread is allowed to log + drop in this case; the audio
    /// thread keeps rendering on its last state).
    #[inline]
    pub fn try_push(&mut self, cmd: AudioCommand) -> Result<(), AudioCommand> {
        self.inner.try_push(cmd)
    }

    /// Number of free slots remaining. Cheap — observer count.
    #[inline]
    pub fn vacant_len(&self) -> usize {
        self.inner.vacant_len()
    }
}

impl AudioConsumer {
    /// Pop the next pending command. Wait-free, alloc-free.
    #[inline]
    pub fn try_pop(&mut self) -> Option<AudioCommand> {
        self.inner.try_pop()
    }

    /// Number of pending commands. Cheap — observer count.
    #[inline]
    pub fn occupied_len(&self) -> usize {
        self.inner.occupied_len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::AudioCommandKind;
    use crate::state::DeckId;

    fn play_a() -> AudioCommand {
        AudioCommand {
            at_frame: 0,
            kind: AudioCommandKind::DeckPlay { deck: DeckId::A },
        }
    }

    #[test]
    fn push_and_pop_round_trip() {
        let ring = AudioRing::new();
        let (mut prod, mut cons) = ring.split();
        prod.try_push(play_a()).unwrap();
        let got = cons.try_pop().unwrap();
        assert_eq!(got, play_a());
        assert!(cons.try_pop().is_none());
    }

    #[test]
    fn capacity_is_1024() {
        let ring = AudioRing::new();
        let (mut prod, mut cons) = ring.split();
        // Fill the ring.
        for _ in 0..RING_CAPACITY {
            prod.try_push(play_a()).unwrap();
        }
        // Next push should fail (ring full).
        assert!(prod.try_push(play_a()).is_err());
        // Drain.
        let mut count = 0;
        while cons.try_pop().is_some() {
            count += 1;
        }
        assert_eq!(count, RING_CAPACITY);
    }

    #[test]
    fn alloc_free_after_construction() {
        // The producer + consumer trait methods must be alloc-free.
        // We gate one full round-trip under `assert_no_alloc`.
        let ring = AudioRing::new();
        let (mut prod, mut cons) = ring.split();
        assert_no_alloc::assert_no_alloc(|| {
            for _ in 0..128 {
                prod.try_push(play_a()).unwrap();
            }
            while cons.try_pop().is_some() {}
        });
    }
}
