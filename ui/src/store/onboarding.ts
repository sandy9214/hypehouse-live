// Onboarding flag store.
//
// First-launch onboarding wizard (see `components/Onboarding.tsx`) is
// gated by a localStorage flag so the wizard only fires when:
//
//   1. The flag is missing (genuine first launch or "Skip"/Cancel chosen
//      on a prior run — both leave the flag absent so the wizard
//      reappears next launch, per spec).
//   2. The library is empty (zero tracks). The library check is done
//      by the caller (`App.tsx`) via `library.list_tracks` because
//      having tracks ingested via some other path (CLI, sidecar) is
//      effectively a "user is past onboarding" signal even without the
//      flag.
//
// The hook deliberately reads localStorage only once at mount and
// returns a stable boolean — onboarding state doesn't need to react
// to cross-tab storage events because the Tauri desktop shell is
// single-window.

import { useEffect, useState } from "react";

/** localStorage key — versioned so we can re-onboard users when the
 * wizard's contract changes meaningfully (e.g. adds a controller setup
 * step). */
export const ONBOARDING_FLAG_KEY = "hypehouse-onboarded-v1";

/** Read the flag synchronously. Safe to call during render; returns
 * `false` if `window`/`localStorage` is unavailable (SSR / tests with
 * a stub environment). */
export const readOnboardingFlag = (): boolean => {
  try {
    if (typeof window === "undefined" || !window.localStorage) return false;
    return window.localStorage.getItem(ONBOARDING_FLAG_KEY) === "1";
  } catch {
    // Private-mode browsers can throw on access — treat as "not done".
    return false;
  }
};

/** Persist the "onboarding complete" flag. Idempotent. */
export const setOnboardingComplete = (): void => {
  try {
    if (typeof window === "undefined" || !window.localStorage) return;
    window.localStorage.setItem(ONBOARDING_FLAG_KEY, "1");
  } catch {
    // Best-effort: if localStorage is unavailable the user will just
    // see the wizard again next launch, which is the safe failure
    // mode (better than silently swallowing first-launch UX).
  }
};

/** Clear the flag — exposed for tests + a future "Run onboarding again"
 * menu item. Not currently wired in v0.1. */
export const clearOnboardingFlag = (): void => {
  try {
    if (typeof window === "undefined" || !window.localStorage) return;
    window.localStorage.removeItem(ONBOARDING_FLAG_KEY);
  } catch {
    // Ignore.
  }
};

export interface OnboardingState {
  /** True when the wizard has already been completed. */
  readonly complete: boolean;
  /** Mark onboarding done + flip `complete` locally so the wizard
   * unmounts without a full re-render of the consumer. */
  readonly markComplete: () => void;
}

/**
 * React hook returning the current onboarding state. Reads the flag at
 * mount; `markComplete` writes the flag and updates local state so the
 * caller can unmount `<Onboarding />` without a page reload.
 */
export const useOnboarding = (): OnboardingState => {
  const [complete, setComplete] = useState<boolean>(
    (): boolean => readOnboardingFlag(),
  );
  // Keep state in sync if another tab/process changes the flag — rare
  // but cheap. The Tauri shell is single-window so this mostly matters
  // for dev (vite hot reload).
  useEffect((): (() => void) => {
    const handler = (ev: StorageEvent): void => {
      if (ev.key !== ONBOARDING_FLAG_KEY) return;
      setComplete(readOnboardingFlag());
    };
    if (typeof window !== "undefined") {
      window.addEventListener("storage", handler);
    }
    return (): void => {
      if (typeof window !== "undefined") {
        window.removeEventListener("storage", handler);
      }
    };
  }, []);
  return {
    complete,
    markComplete: (): void => {
      setOnboardingComplete();
      setComplete(true);
    },
  };
};
