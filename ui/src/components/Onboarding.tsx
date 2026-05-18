// Onboarding.tsx — first-launch 3-step wizard for ingesting a library.
//
// Mount conditions (decided by `App.tsx`):
//   1. `useOnboarding().complete === false` (localStorage flag missing).
//   2. `library.list_tracks` returned `total === 0`.
//
// Step flow:
//   1. Welcome — intro copy + "Next" (or "Skip" link → close without
//      flag-set → reopens next launch).
//   2. Pick directory — text input + optional <input type="file"
//      webkitdirectory> fallback (chromium-only). User types/pastes a
//      server-resolvable absolute path; "Scan" disabled until non-empty.
//   3. Ingest progress — fires `library.add_track_from_directory`. While
//      that's running, polls `library.list_tracks` every 1 s with
//      `limit:1` (the `total` field is the catalog size — see
//      `copilot/library_rpc.py::_list_tracks`) so the progress label
//      ticks up. When the RPC resolves the final count is read off
//      the response's `total` field and the "Done!" state shows.
//
// All steps share the same dim modal overlay + max-600px card. Cancel
// (top-right X) at any step closes the modal without setting the flag.
// Styles live in `Onboarding.styles.ts` so this file fits the 250-line
// per-component budget the repo enforces.

import { useEffect, useRef, useState } from "react";
import type { JSX } from "react";
import type { JsonRpcWS } from "../ws/client";
import { setOnboardingComplete } from "../store/onboarding";
import {
  bodyStyle,
  cardStyle,
  closeBtnStyle,
  dotStyle,
  dotsStyle,
  errorTextStyle,
  footerStyle,
  headerStyle,
  inputLabelStyle,
  inputStyle,
  linkStyle,
  overlayStyle,
  primaryBtnStyle,
  progressBarInner,
  progressBarOuter,
  secondaryBtnStyle,
  sectionHeadingStyle,
} from "./Onboarding.styles";

export interface OnboardingProps {
  readonly client: JsonRpcWS;
  /** Called whenever the user closes the wizard. `completed=true` when
   * the user finished step 3; `false` for Skip/Cancel. App.tsx uses
   * this to unmount the modal. The localStorage flag is written inside
   * the component (only on `completed=true`) so the caller doesn't
   * need to import the store. */
  readonly onClose: (completed: boolean) => void;
  /** Polling interval for `library.list_tracks` during step 3. Tests
   * override (e.g. to 0) for instant assertions. */
  readonly pollIntervalMs?: number;
}

type Step = 1 | 2 | 3;
type IngestPhase = "idle" | "scanning" | "done" | "error";

interface AddResult {
  readonly added_count: number;
  readonly total: number;
}

interface ListResult {
  readonly total: number;
}

export const Onboarding = ({
  client,
  onClose,
  pollIntervalMs = 1000,
}: OnboardingProps): JSX.Element => {
  const [step, setStep] = useState<Step>(1);
  const [dir, setDir] = useState<string>("");
  const [phase, setPhase] = useState<IngestPhase>("idle");
  const [count, setCount] = useState<number>(0);
  const [errMsg, setErrMsg] = useState<string>("");
  const pollTimerRef = useRef<ReturnType<typeof setInterval> | null>(null);

  const stopPolling = (): void => {
    if (pollTimerRef.current !== null) {
      clearInterval(pollTimerRef.current);
      pollTimerRef.current = null;
    }
  };

  useEffect((): (() => void) => {
    return (): void => stopPolling();
  }, []);

  const closeWithoutFlag = (): void => {
    stopPolling();
    onClose(false);
  };

  const startScan = (): void => {
    if (!dir.trim()) return;
    setPhase("scanning");
    setErrMsg("");
    setCount(0);
    setStep(3);
    if (pollIntervalMs > 0) {
      pollTimerRef.current = setInterval((): void => {
        void client
          .call<unknown>("library.list_tracks", { limit: 1, offset: 0 })
          .then((r: unknown): void => {
            if (r && typeof r === "object" && "total" in r) {
              const t = (r as ListResult).total;
              if (typeof t === "number") setCount(t);
            }
          })
          .catch((): void => undefined);
      }, pollIntervalMs);
    }
    void client
      .call<unknown>("library.add_track_from_directory", { path: dir.trim() })
      .then((r: unknown): void => {
        stopPolling();
        if (r && typeof r === "object" && "total" in r) {
          const final = (r as AddResult).total;
          if (typeof final === "number") setCount(final);
        }
        setPhase("done");
      })
      .catch((e: unknown): void => {
        stopPolling();
        setErrMsg(e instanceof Error ? e.message : "scan failed");
        setPhase("error");
      });
  };

  const handleFinish = (): void => {
    stopPolling();
    setOnboardingComplete();
    onClose(true);
  };

  const heading =
    phase === "done"
      ? "Done!"
      : phase === "error"
        ? "Scan failed"
        : "Scanning your library…";

  return (
    <div role="dialog" aria-label="Onboarding wizard" data-testid="onboarding-modal" style={overlayStyle}>
      <div style={cardStyle}>
        <header style={headerStyle}>
          <div style={dotsStyle} aria-label={`Step ${step} of 3`}>
            <span data-testid="dot-1" style={dotStyle(step >= 1)} />
            <span data-testid="dot-2" style={dotStyle(step >= 2)} />
            <span data-testid="dot-3" style={dotStyle(step >= 3)} />
          </div>
          <button type="button" aria-label="Close" data-testid="onboarding-cancel" onClick={closeWithoutFlag} style={closeBtnStyle}>×</button>
        </header>
        <div style={bodyStyle} data-testid={`onboarding-step-${step}`}>
          {step === 1 && (
            <div>
              <h2 style={sectionHeadingStyle}>Welcome to hypehouse-live</h2>
              <p>Live multi-deck DJ player with an optional AI co-pilot.</p>
              <p>Let&apos;s set up your library — point us at a folder of tracks and we&apos;ll analyze BPM, key, and beats so the decks are ready to mix.</p>
            </div>
          )}
          {step === 2 && (
            <div>
              <h2 style={sectionHeadingStyle}>Pick a music folder</h2>
              <p>Paste the absolute path to a folder on this machine. We&apos;ll scan every audio file in it.</p>
              <label htmlFor="onboarding-dir-input" style={inputLabelStyle}>Directory path</label>
              <input id="onboarding-dir-input" data-testid="onboarding-dir-input" type="text" value={dir} onChange={(e): void => setDir(e.target.value)} placeholder="/Users/you/Music" style={inputStyle} autoFocus />
            </div>
          )}
          {step === 3 && (
            <div>
              <h2 style={sectionHeadingStyle}>{heading}</h2>
              {phase === "scanning" && (
                <>
                  <p>Analyzing tracks in <code>{dir}</code> — this can take a minute for large folders.</p>
                  <p data-testid="onboarding-progress-count">{count} track{count === 1 ? "" : "s"} analyzed</p>
                  <div style={progressBarOuter}><div style={progressBarInner(Math.min(95, count))} /></div>
                </>
              )}
              {phase === "done" && (
                <>
                  <p data-testid="onboarding-done">Ingested <strong>{count}</strong> track{count === 1 ? "" : "s"}. You&apos;re ready to mix.</p>
                  <div style={progressBarOuter}><div style={progressBarInner(100)} /></div>
                </>
              )}
              {phase === "error" && (
                <p role="alert" data-testid="onboarding-error" style={errorTextStyle}>{errMsg}</p>
              )}
            </div>
          )}
        </div>
        <footer style={footerStyle}>
          <div>
            {step === 1 && (
              <button type="button" data-testid="onboarding-skip" onClick={closeWithoutFlag} style={linkStyle}>Skip for now</button>
            )}
            {step === 2 && (
              <button type="button" data-testid="onboarding-back" onClick={(): void => setStep(1)} style={secondaryBtnStyle}>Back</button>
            )}
          </div>
          <div>
            {step === 1 && (
              <button type="button" data-testid="onboarding-next" onClick={(): void => setStep(2)} style={primaryBtnStyle(false)}>Next</button>
            )}
            {step === 2 && (
              <button type="button" data-testid="onboarding-scan" disabled={!dir.trim()} onClick={startScan} style={primaryBtnStyle(!dir.trim())}>Scan</button>
            )}
            {step === 3 && phase === "done" && (
              <button type="button" data-testid="onboarding-finish" onClick={handleFinish} style={primaryBtnStyle(false)}>Start mixing</button>
            )}
            {step === 3 && phase === "error" && (
              <button type="button" data-testid="onboarding-retry" onClick={(): void => setStep(2)} style={primaryBtnStyle(false)}>Back</button>
            )}
          </div>
        </footer>
      </div>
    </div>
  );
};
