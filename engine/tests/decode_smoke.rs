//! End-to-end smoke test: symphonia decode → rubato resample → ring →
//! audio-thread-style read.
//!
//! Generates a 1-second 440 Hz sine PCM-16 WAV in memory, opens it
//! through `SymphoniaDecodeService::open`, and asserts that
//! `read` produces a sustained non-zero audio signal at the target
//! sample rate.
//!
//! We deliberately do NOT assert exact sample counts at this level
//! (rubato latency + decoder packet boundaries make that brittle);
//! instead we assert structural properties:
//!
//! * Stereo: L and R columns are present.
//! * Energy: cumulative RMS above the silence threshold.
//! * Amplitude: peak magnitude within `[0.05, 1.0]` (PCM-16 was
//!   scaled by 0.5 in the synthesizer; allow padding/headroom).
//!
//! Per ADR-004: this test only runs the read path on the calling
//! thread, not a real cpal stream — so we can validate correctness
//! deterministically without requiring an audio device in CI.

use std::time::Duration;

use hypehouse_engine::audio::{DecodeService, SymphoniaDecodeService};
use hypehouse_engine::state::TrackRef;

const TARGET_SR: u32 = 48_000;

fn build_wav_pcm16(channels: u16, sample_rate: u32, samples: &[i16]) -> Vec<u8> {
    let bits_per_sample = 16u16;
    let byte_rate = sample_rate * channels as u32 * bits_per_sample as u32 / 8;
    let block_align = channels * bits_per_sample / 8;
    let data_bytes = (samples.len() * 2) as u32;
    let mut v = Vec::with_capacity(44 + samples.len() * 2);
    v.extend_from_slice(b"RIFF");
    v.extend_from_slice(&(36 + data_bytes).to_le_bytes());
    v.extend_from_slice(b"WAVE");
    v.extend_from_slice(b"fmt ");
    v.extend_from_slice(&16u32.to_le_bytes());
    v.extend_from_slice(&1u16.to_le_bytes()); // PCM
    v.extend_from_slice(&channels.to_le_bytes());
    v.extend_from_slice(&sample_rate.to_le_bytes());
    v.extend_from_slice(&byte_rate.to_le_bytes());
    v.extend_from_slice(&block_align.to_le_bytes());
    v.extend_from_slice(&bits_per_sample.to_le_bytes());
    v.extend_from_slice(b"data");
    v.extend_from_slice(&data_bytes.to_le_bytes());
    for s in samples {
        v.extend_from_slice(&s.to_le_bytes());
    }
    v
}

fn sine_pcm16(freq: f32, sr: u32, secs: f32, channels: u16) -> Vec<i16> {
    let n = (sr as f32 * secs) as usize;
    let mut out = Vec::with_capacity(n * channels as usize);
    let tau = std::f32::consts::TAU;
    for i in 0..n {
        let s = (tau * freq * (i as f32 / sr as f32)).sin();
        let v = (s * 0.5 * i16::MAX as f32) as i16;
        for _ in 0..channels {
            out.push(v);
        }
    }
    out
}

#[test]
fn smoke_decode_440hz_wav_yields_audio_signal() {
    let svc = SymphoniaDecodeService::new();
    // 1 s of 440 Hz mono @ 48 kHz — no resampler, decoder only.
    let wav = build_wav_pcm16(1, TARGET_SR, &sine_pcm16(440.0, TARGET_SR, 1.0, 1));
    svc.register_inline_source("sine", wav);

    let track = TrackRef {
        id: "sine".to_string(),
        path: "mem://sine".to_string(),
    };
    let handle = svc
        .open(&track, TARGET_SR)
        .expect("open should succeed on a valid WAV");

    // Let the decoder thread run. Decoder + ring push runs at full
    // CPU speed; 1 s of PCM-16 mono decodes in ~milliseconds even on
    // slow boxes.
    std::thread::sleep(Duration::from_millis(800));

    // Pull 0.5 s of stereo audio (48k frames * 2 = 96k samples).
    // We pull in chunks so the audio-thread-side read pattern is
    // accurate.
    let mut samples: Vec<f32> = Vec::with_capacity(96_000);
    let mut chunk = [0.0_f32; 4096];
    while samples.len() < 96_000 {
        svc.read(handle, &mut chunk);
        samples.extend_from_slice(&chunk);
    }
    samples.truncate(96_000);

    // --- Assertions ----------------------------------------------------
    // 1. RMS energy is non-trivial.
    let rms: f32 = (samples.iter().map(|s| s * s).sum::<f32>() / samples.len() as f32).sqrt();
    assert!(
        rms > 0.05,
        "decoded RMS energy too low: {rms} — decoder may have produced silence"
    );

    // 2. Peak magnitude is bounded (no clipping, no NaN).
    let peak = samples.iter().fold(0.0_f32, |acc, s| acc.max(s.abs()));
    assert!(
        peak <= 1.0 + 1e-3,
        "decoded peak exceeds 1.0 — clipping or scaling bug: {peak}"
    );
    assert!(peak >= 0.1, "decoded peak too low: {peak}");

    // 3. L = R for a mono source duplicated to stereo.
    let mismatches = (0..samples.len())
        .step_by(2)
        .filter(|i| (samples[*i] - samples[i + 1]).abs() > 1e-6)
        .count();
    assert_eq!(mismatches, 0, "mono → stereo duplicate should yield L=R");

    svc.close(handle);
}

#[test]
fn smoke_decode_22050_resample_to_48k() {
    let svc = SymphoniaDecodeService::new();
    // 1.0 s of 440 Hz mono @ 22.05 kHz — exercises the rubato path.
    let wav = build_wav_pcm16(1, 22_050, &sine_pcm16(440.0, 22_050, 1.0, 1));
    svc.register_inline_source("rs", wav);

    let track = TrackRef {
        id: "rs".to_string(),
        path: "mem://rs".to_string(),
    };
    let handle = svc.open(&track, TARGET_SR).expect("open ok");

    // Resampler has more latency — give it extra wall time.
    std::thread::sleep(Duration::from_millis(1200));

    let mut samples: Vec<f32> = Vec::with_capacity(48_000);
    let mut chunk = [0.0_f32; 2048];
    for _ in 0..24 {
        svc.read(handle, &mut chunk);
        samples.extend_from_slice(&chunk);
    }
    let rms: f32 = (samples.iter().map(|s| s * s).sum::<f32>() / samples.len() as f32).sqrt();
    assert!(rms > 0.02, "resampled signal too quiet: {rms}");
    svc.close(handle);
}
