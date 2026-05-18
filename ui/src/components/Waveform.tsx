// Waveform placeholder.
//
// ADR-001 keeps the audio path inside the Rust engine; the UI only
// visualises. This component intentionally does NOT decode audio. It
// just exposes a `<canvas>` and a `useWaveform(audioBuffer | null)`
// hook that — once real visualization lands — will pump samples from
// the engine's analyser tap (planned in a later PR) into the canvas.
//
// For v0.1 we draw a flat midline so the layout reserves space.

import { useEffect, useRef } from "react";

export interface WaveformProps {
  height?: number;
  width?: number;
}

/** Hook stub — accepts an AudioBuffer-shaped payload but is a no-op. */
export const useWaveform = (audioBuffer: AudioBuffer | null): void => {
  // Reserved for the real visualization pipeline. Today: no-op.
  // The reference is kept so callers can be wired now and lit later.
  void audioBuffer;
};

export const Waveform = ({
  height = 96,
  width = 480,
}: WaveformProps): JSX.Element => {
  const canvasRef = useRef<HTMLCanvasElement | null>(null);

  useEffect((): void => {
    const canvas = canvasRef.current;
    if (!canvas) return;
    const ctx = canvas.getContext("2d");
    if (!ctx) return;
    ctx.clearRect(0, 0, canvas.width, canvas.height);
    ctx.strokeStyle = "#666";
    ctx.lineWidth = 1;
    ctx.beginPath();
    ctx.moveTo(0, canvas.height / 2);
    ctx.lineTo(canvas.width, canvas.height / 2);
    ctx.stroke();
  }, [width, height]);

  return (
    <canvas
      ref={canvasRef}
      width={width}
      height={height}
      data-testid="waveform-canvas"
      style={{ display: "block", background: "#111" }}
    />
  );
};
