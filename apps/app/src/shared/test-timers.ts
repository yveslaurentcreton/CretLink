// Test helper: the timers a test leaves behind (ST-M009-008, ST-M009-009).
//
// The controller tests load the controller anew for every test
// (`vi.resetModules()`), but the instance of an earlier test keeps running.
// Its pending timers (the audit refresh, the retry for a session summary, the
// once-a-second ticker) fire during a later test and call the backend fake
// that all tests of a file share, so a later test now and then counted a call
// it did not cause.
//
// `trackTimers()` in `beforeEach` records every timer started from then on;
// `clear()` in `afterEach` cancels them all, so a test ends with nothing left
// to fire.

import { vi } from "vitest";

type TimerId = ReturnType<typeof setTimeout>;

export interface TimerTracker {
  /** Cancels every timer started since `trackTimers()` and stops recording. */
  clear(): void;
}

export function trackTimers(): TimerTracker {
  const started: TimerId[] = [];
  const realTimeout = globalThis.setTimeout;
  const realInterval = globalThis.setInterval;

  const record =
    (real: (...args: unknown[]) => TimerId) =>
    (...args: unknown[]): TimerId => {
      const id = real(...args);
      started.push(id);
      return id;
    };

  const timeout = vi
    .spyOn(globalThis, "setTimeout")
    .mockImplementation(
      record(realTimeout as unknown as (...args: unknown[]) => TimerId) as unknown as typeof setTimeout,
    );
  const interval = vi
    .spyOn(globalThis, "setInterval")
    .mockImplementation(
      record(realInterval as unknown as (...args: unknown[]) => TimerId) as unknown as typeof setInterval,
    );

  return {
    clear() {
      timeout.mockRestore();
      interval.mockRestore();
      for (const id of started) {
        clearTimeout(id);
        clearInterval(id);
      }
      started.length = 0;
    },
  };
}
