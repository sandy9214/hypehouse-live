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

use std::sync::atomic::AtomicI16;
use std::sync::Arc;
use std::time::Instant;

use anyhow::{anyhow, Result};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{Sample, SampleFormat, Stream, StreamConfig, SupportedStreamConfig};

use crate::audio::{AudioConsumer, AudioMixer, DecodeService, PerfMetrics, SharedClock};
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
    /// Cloneable handle on the master-bus limiter's gain-reduction
    /// readout. The bridge thread reads this once per
    /// `state_changed` notification to stamp the live GR onto the
    /// payload so the UI meter can render it without polling.
    pub master_limiter_gr: Arc<AtomicI16>,
    /// Audio-thread performance counters (CPU%, render p99, underruns).
    /// The audio callback writes into the shared atomics on every
    /// render via [`PerfMetrics::record_render_ns`]; the bridge thread
    /// snapshots them for every outgoing `engine.state_changed`. See
    /// `audio::perf` for the contract.
    pub perf: PerfMetrics,
}

/// How `pick_output_device` resolved the requested device. Logged at
/// startup + returned to callers (UI list endpoint, future RPC).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputDeviceSelection {
    /// No substring was passed; the host's default device was used.
    Default,
    /// A substring matched a non-default device (e.g. BlackHole 2ch).
    Matched,
    /// A substring was passed but no device name contained it; fell
    /// back to the host default so the engine still starts. The caller
    /// (UI/CLI) is responsible for surfacing this to the operator.
    Fallback,
}

/// Pure substring-match helper — separates testable selection logic from
/// cpal device enumeration (which can crash inside macOS unit-test
/// processes due to CoreAudio thread-context quirks).
///
/// `None` or empty `substring` → `None` (caller should use default device).
/// Non-empty `substring` → index of the first name whose lowercased form
/// contains the lowercased needle, or `None` if no match.
pub(crate) fn match_device_by_substring<S: AsRef<str>>(
    names: &[S],
    substring: Option<&str>,
) -> Option<usize> {
    let needle = substring
        .map(str::trim)
        .filter(|s| !s.is_empty())?
        .to_lowercase();
    names
        .iter()
        .position(|n| n.as_ref().to_lowercase().contains(&needle))
}

/// Choose an output device on `host` honouring an optional case-insensitive
/// substring match. Returns the device + how the selection resolved.
///
/// Selection rules:
/// 1. If `substring` is `None` or empty → host default device.
/// 2. If `substring` matches the *first* device whose name contains it
///    (case-insensitive) → that device, [`OutputDeviceSelection::Matched`].
/// 3. If `substring` is set but no device matches → host default device,
///    [`OutputDeviceSelection::Fallback`]. The engine still starts so a
///    typo in the env var doesn't take audio offline.
///
/// Default match is intentional — for the livestream use case the user
/// typically passes a fragment like `"BlackHole"` or `"VB-Cable"`; we
/// don't want them to have to type the full `BlackHole 2ch (Virtual)`
/// label.
pub fn pick_output_device(
    host: &cpal::Host,
    substring: Option<&str>,
) -> Result<(cpal::Device, OutputDeviceSelection)> {
    let needle_trimmed = substring.map(str::trim).filter(|s| !s.is_empty());

    if let Some(needle) = needle_trimmed {
        let devices: Vec<cpal::Device> = host
            .output_devices()
            .map_err(|e| anyhow!("cpal output_devices() failed: {e}"))?
            .collect();
        let names: Vec<String> = devices
            .iter()
            .map(|d| d.name().unwrap_or_default())
            .collect();
        if let Some(idx) = match_device_by_substring(&names, Some(needle)) {
            let device = devices.into_iter().nth(idx).expect("idx in range");
            return Ok((device, OutputDeviceSelection::Matched));
        }
        let device = host.default_output_device().ok_or_else(|| {
            anyhow!("no default audio output device (and no match for substring '{needle}')")
        })?;
        return Ok((device, OutputDeviceSelection::Fallback));
    }

    let device = host
        .default_output_device()
        .ok_or_else(|| anyhow!("no default audio output device"))?;
    Ok((device, OutputDeviceSelection::Default))
}

/// Enumerate the host's output device names. Used by the UI device-list
/// endpoint + the CLI `--list-output-devices` flag. Defunct devices
/// (name() fails) are skipped. Order matches cpal enumeration order;
/// the host default is **not** flagged here — callers re-query
/// `host.default_output_device().name()` if they need that signal.
///
/// Returns an empty vec if the host has no devices (e.g. inside a
/// container without an audio sink); never errors.
pub fn enumerate_output_devices(host: &cpal::Host) -> Vec<String> {
    match host.output_devices() {
        Ok(devices) => devices.filter_map(|d| d.name().ok()).collect(),
        Err(_) => Vec::new(),
    }
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
    output_device_substring: Option<&str>,
) -> Result<AudioStreamHandle> {
    let host = cpal::default_host();
    let (device, selection) = pick_output_device(&host, output_device_substring)?;
    let device_name = device.name().unwrap_or_else(|_| "<unnamed>".to_string());
    tracing::info!(
        target: "audio",
        device = %device_name,
        selection = ?selection,
        "cpal output device selected"
    );

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
    // Grab the limiter's gain-reduction handle BEFORE the mixer is moved
    // into the cpal callback closure. The bridge thread reads this to
    // populate the `master_limiter_gain_reduction_db` field of every
    // `engine.state_changed` notification (UI meter).
    let master_limiter_gr = mixer.master_limiter_gain_reduction_atomic();

    // Live perf metrics — one set of atomics shared between the audio
    // callback (writer) and the bridge thread (reader). The cpal config
    // doesn't pin a buffer size on the default device probe; we seed
    // the callback period with the engine-favoured 512-frame default
    // and let `main()` refine it once the device's real buffer size is
    // observed (when supported by the host).
    let perf =
        PerfMetrics::with_callback_period(PerfMetrics::callback_period_from(512, sample_rate));
    let perf_for_cb = perf.clone();

    // Hand the consumer + mixer + clock to the callback. cpal callbacks
    // must be `Send + 'static`; we move owned values in.
    let stream = match sample_format {
        SampleFormat::F32 => build_stream::<f32>(
            &device,
            &config,
            mixer,
            consumer,
            clock,
            channels,
            perf_for_cb,
        )?,
        SampleFormat::I16 => build_stream::<i16>(
            &device,
            &config,
            mixer,
            consumer,
            clock,
            channels,
            perf_for_cb,
        )?,
        SampleFormat::U16 => build_stream::<u16>(
            &device,
            &config,
            mixer,
            consumer,
            clock,
            channels,
            perf_for_cb,
        )?,
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
        master_limiter_gr,
        perf,
    })
}

fn build_stream<S>(
    device: &cpal::Device,
    config: &StreamConfig,
    mut mixer: AudioMixer,
    mut consumer: AudioConsumer,
    clock: SharedClock,
    channels: u16,
    perf: PerfMetrics,
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
    // cpal's err_fn is invoked off the realtime path on stream errors
    // (underruns, device removal). Each call counts as one audio-side
    // underrun for the perf snapshot.
    let perf_err = perf.clone();
    let err_fn = move |e| {
        perf_err.record_audio_underrun();
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
                // Start the render-time stopwatch. `Instant::now()` is
                // a `clock_gettime(MONOTONIC)` on Unix + `QueryPerformance
                // Counter` on Windows — both are vDSO / syscall-free,
                // sub-100ns reads. Safe on the audio thread.
                let render_start = Instant::now();

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

                // End of render: record the wall-clock time the audio
                // thread spent inside this callback. `as_nanos()`
                // returns u128; clamp to u64 — a single render that ran
                // for more than ~584 years would wrap, which is well
                // outside the production budget.
                let render_ns = render_start.elapsed().as_nanos() as u64;
                perf.record_render_ns(render_ns);
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

    // Pure-function tests for the substring-match logic — exercise every
    // branch without touching cpal / CoreAudio (which segfaults inside
    // macOS unit-test threads on some hosts). The cpal integration is
    // smoke-tested in the `pick_output_device_*` block below, gated
    // `#[ignore]` so dev boxes opt in via `cargo test -- --ignored`.

    #[test]
    fn match_device_none_substring_returns_none() {
        let names: Vec<String> = vec!["BlackHole 2ch".into(), "MacBook Speakers".into()];
        assert_eq!(match_device_by_substring(&names, None), None);
    }

    #[test]
    fn match_device_empty_substring_returns_none() {
        let names: Vec<String> = vec!["BlackHole 2ch".into()];
        assert_eq!(match_device_by_substring(&names, Some("")), None);
        assert_eq!(match_device_by_substring(&names, Some("   ")), None);
    }

    #[test]
    fn match_device_case_insensitive_first_match() {
        let names: Vec<String> = vec![
            "MacBook Pro Speakers".into(),
            "BlackHole 2ch".into(),
            "External Headphones".into(),
        ];
        assert_eq!(
            match_device_by_substring(&names, Some("blackhole")),
            Some(1)
        );
        assert_eq!(match_device_by_substring(&names, Some("BLACK")), Some(1));
        assert_eq!(match_device_by_substring(&names, Some("MacBook")), Some(0));
    }

    #[test]
    fn match_device_no_match_returns_none() {
        let names: Vec<String> = vec!["MacBook Speakers".into(), "AirPods".into()];
        assert_eq!(
            match_device_by_substring(&names, Some("XX_NONEXISTENT_DEVICE_XX")),
            None
        );
    }

    #[test]
    fn match_device_handles_empty_list() {
        let names: Vec<String> = Vec::new();
        assert_eq!(match_device_by_substring(&names, Some("anything")), None);
        assert_eq!(match_device_by_substring(&names, None), None);
    }

    #[test]
    fn match_device_returns_first_of_multiple_matches() {
        let names: Vec<String> = vec![
            "BlackHole 2ch".into(),
            "BlackHole 16ch".into(),
            "BlackHole 64ch".into(),
        ];
        assert_eq!(
            match_device_by_substring(&names, Some("blackhole")),
            Some(0)
        );
    }

    // The following 3 tests hit live cpal enumeration. CoreAudio is not
    // reliably safe to call from a cargo unit-test thread on some macOS
    // hosts (observed SIGSEGV on enumerate). Gated `#[ignore]` so they're
    // available for manual verification but don't break the matrix. The
    // pure-function tests above cover the selection logic itself.

    #[test]
    #[ignore = "cpal enumeration segfaults in macOS unit-test threads; run via --ignored on dev box"]
    fn pick_output_device_returns_default_for_none_substring() {
        let host = cpal::default_host();
        if host.default_output_device().is_none() {
            return;
        }
        let (_dev, sel) = pick_output_device(&host, None).expect("pick");
        assert_eq!(sel, OutputDeviceSelection::Default);
    }

    #[test]
    #[ignore = "cpal enumeration segfaults in macOS unit-test threads; run via --ignored on dev box"]
    fn pick_output_device_fallback_on_no_match() {
        let host = cpal::default_host();
        if host.default_output_device().is_none() {
            return;
        }
        let (_dev, sel) =
            pick_output_device(&host, Some("XX_NONEXISTENT_DEVICE_XX_42")).expect("pick");
        assert_eq!(sel, OutputDeviceSelection::Fallback);
    }

    #[test]
    #[ignore = "cpal enumeration segfaults in macOS unit-test threads; run via --ignored on dev box"]
    fn enumerate_output_devices_returns_vec_without_error() {
        let host = cpal::default_host();
        let names = enumerate_output_devices(&host);
        for n in &names {
            assert!(!n.is_empty());
        }
    }

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
