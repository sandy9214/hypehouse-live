//! cpal initialization + audio-thread callback.
//!
//! [`spawn_audio_thread`] opens the default output device, builds a stream
//! whose callback:
//!
//! 1. Reads the current engine clock frame.
//! 2. Drains every [`AudioCommand`] in the ring whose `at_frame <=
//!    end_of_this_buffer`, applying each to the [`AudioMixer`].
//! 3. Renders the next buffer of samples into the cpal output slice.
//! 4. Bumps the shared clock.
//!
//! ADR-004 hard rules enforced inside the callback:
//! * No allocation — only reads from the ring + writes into the cpal
//!   slice + arithmetic on stack state.
//! * No mutex — the ring is lock-free SPSC, the clock is `AtomicU64`.
//! * No blocking — no I/O, no `println!`, no panic-on-error logging
//!   (errors are silently coalesced).

use std::sync::Arc;

use anyhow::{anyhow, Result};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{Sample, SampleFormat, Stream, StreamConfig, SupportedStreamConfig};

use crate::audio::{AudioConsumer, AudioMixer, DecodeService, SharedClock};
use crate::recording::MasterRecorderSink;

/// Extract the sample rate from a [`SupportedStreamConfig`] as `u32`.
///
/// Why a helper:
///
/// cpal 0.16 (current pinned) returns `SampleRate(u32)` — a newtype —
/// from `SupportedStreamConfig::sample_rate()`. cpal 0.17 (tracked by
/// issue #14, the audio-stack dependabot bump) changes the return type
/// to `u32` directly. The version bump is otherwise a clean drop-in,
/// but every call site that goes through `.0` to unwrap the newtype
/// will fail to compile on 0.17.
///
/// Funnelling the cast through this helper means the bump becomes a
/// single-line patch (drop the `.0`) in one place, not a sed across the
/// callback path. See also issue #26 (parent — Rust 1.88 toolchain
/// bump + deferred dependabot PRs).
#[inline]
fn supported_sample_rate(c: &SupportedStreamConfig) -> u32 {
    // cpal 0.16: `sample_rate()` -> `SampleRate(u32)`; `.0` extracts the inner u32.
    // cpal 0.17: `sample_rate()` -> `u32` directly; drop the `.0` here when bumping.
    c.sample_rate().0
}

/// Owns the cpal `Stream` (which keeps the audio callback alive). When
/// the handle is dropped the stream is torn down + the OS thread joins.
pub struct AudioStreamHandle {
    _stream: Stream,
    pub sample_rate: u32,
    pub channels: u16,
}

/// Build + start the output stream. The producer side of the SPSC ring
/// stays with the caller (control thread); we take the consumer.
///
/// An optional `recorder_sink` tees the master mix into the per-session
/// `master.wav` recorder (see [`crate::recording`]). When `None`, the
/// tee path is a single `is_some()` check per render chunk — no extra
/// cost on the audio thread.
pub fn spawn_audio_thread(
    consumer: AudioConsumer,
    clock: SharedClock,
    decode: Arc<dyn DecodeService>,
    recorder_sink: Option<MasterRecorderSink>,
) -> Result<AudioStreamHandle> {
    let host = cpal::default_host();
    let device = host
        .default_output_device()
        .ok_or_else(|| anyhow!("no default audio output device"))?;

    let supported = device
        .default_output_config()
        .map_err(|e| anyhow!("default_output_config failed: {e}"))?;

    let sample_rate = supported_sample_rate(&supported);
    let channels = supported.channels();
    let sample_format = supported.sample_format();
    let config: StreamConfig = supported.into();

    let mut mixer = AudioMixer::with_decode(sample_rate, decode);
    if let Some(sink) = recorder_sink {
        mixer.attach_recorder(sink);
    }

    // Hand the consumer + mixer + clock to the callback. cpal callbacks
    // must be `Send + 'static`; we move owned values in.
    let stream = match sample_format {
        SampleFormat::F32 => {
            build_stream::<f32>(&device, &config, mixer, consumer, clock, channels)?
        }
        SampleFormat::I16 => {
            build_stream::<i16>(&device, &config, mixer, consumer, clock, channels)?
        }
        SampleFormat::U16 => {
            build_stream::<u16>(&device, &config, mixer, consumer, clock, channels)?
        }
        other => {
            return Err(anyhow!(
                "unsupported sample format {other:?}; expected f32/i16/u16"
            ))
        }
    };

    stream
        .play()
        .map_err(|e| anyhow!("failed to start audio stream: {e}"))?;

    Ok(AudioStreamHandle {
        _stream: stream,
        sample_rate,
        channels,
    })
}

fn build_stream<S>(
    device: &cpal::Device,
    config: &StreamConfig,
    mut mixer: AudioMixer,
    mut consumer: AudioConsumer,
    clock: SharedClock,
    channels: u16,
) -> Result<Stream>
where
    S: Sample + cpal::FromSample<f32> + cpal::SizedSample + Send + 'static,
{
    // Stack scratch buffer for the mono mix path. 4096 mono frames is
    // a generous upper bound on cpal's per-callback frame count
    // (typically 64..1024). Using a fixed-size array avoids any heap
    // touch inside the callback. If a host ever asks for more, we mix
    // in 4096-frame chunks within the callback below.
    const MAX_MONO_FRAMES: usize = 4096;
    let err_fn = |e| {
        // Logging from the audio thread is FORBIDDEN inside the
        // callback (ADR-004), but cpal's separate `err_fn` is invoked
        // off the realtime path on error, so it's safe.
        tracing::error!(target: "audio", error = ?e, "cpal stream error");
    };

    let channels = channels as usize;

    let stream = device
        .build_output_stream::<S, _, _>(
            config,
            move |data: &mut [S], _info| {
                // Total interleaved samples; mono frames = total / channels.
                let total_samples = data.len();
                let total_mono_frames = total_samples / channels.max(1);

                let mut mono_scratch = [0.0f32; MAX_MONO_FRAMES];
                let mut written_frames = 0usize;

                while written_frames < total_mono_frames {
                    let chunk = (total_mono_frames - written_frames).min(MAX_MONO_FRAMES);
                    let chunk_end_frame = clock.frame() + chunk as u64;

                    // Drain commands due by `chunk_end_frame`. Use
                    // `try_pop` until either ring is empty or the
                    // next command is in the future.
                    //
                    // NOTE: `ringbuf` doesn't expose peek-without-pop
                    // cheaply, so for v0.1 we pop-and-execute every
                    // pending command. The audio-thread mixer
                    // tolerates "in the future" commands as
                    // immediate-apply; sample-accurate scheduling
                    // lands in a later PR when we add a small
                    // priority-queue scratch on the audio thread.
                    let _ = chunk_end_frame;
                    while let Some(cmd) = consumer.try_pop() {
                        mixer.apply(cmd);
                    }

                    let slice = &mut mono_scratch[..chunk];
                    for s in slice.iter_mut() {
                        *s = 0.0;
                    }
                    mixer.render(slice);

                    // Interleave into the device's channel layout.
                    let out =
                        &mut data[written_frames * channels..(written_frames + chunk) * channels];
                    for (i, mono) in slice.iter().enumerate() {
                        for ch in 0..channels {
                            out[i * channels + ch] = S::from_sample(*mono);
                        }
                    }

                    clock.advance(chunk as u32);
                    written_frames += chunk;
                }
            },
            err_fn,
            None,
        )
        .map_err(|e| anyhow!("build_output_stream failed: {e}"))?;

    Ok(stream)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::command::AudioCommandKind;
    use crate::audio::ring::AudioRing;
    use crate::audio::AudioCommand;
    use crate::state::DeckId;

    // We can't actually open a real audio device in CI, but we can
    // unit-test the alloc-free + correctness contract on the mixer +
    // ring + clock combo that lives behind the callback.
    #[test]
    fn integrated_pipeline_alloc_free_drain() {
        let (mut prod, mut cons) = AudioRing::new().split();
        prod.try_push(AudioCommand {
            at_frame: 0,
            kind: AudioCommandKind::DeckPlay { deck: DeckId::A },
        })
        .unwrap();
        prod.try_push(AudioCommand {
            at_frame: 0,
            kind: AudioCommandKind::Crossfader {
                target: 0.0,
                ramp_frames: 240,
            },
        })
        .unwrap();

        let clock = SharedClock::new();
        let mut mixer = AudioMixer::new(48_000);
        let mut buf = [0.0f32; 256];

        assert_no_alloc::assert_no_alloc(|| {
            while let Some(cmd) = cons.try_pop() {
                mixer.apply(cmd);
            }
            mixer.render(&mut buf);
            clock.advance(buf.len() as u32);
        });

        assert_eq!(clock.frame(), 256);
        assert!(mixer.is_playing(DeckId::A));
    }
}
