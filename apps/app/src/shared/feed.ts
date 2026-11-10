// Role-neutral helpers for activity feeds and their words (ST-M009-008,
// ST-M009-009, ST-M009-011), shared by both roles' interfaces so neither
// loads the other's code. Pure functions, tested in jsdom.

import type { ActivityEntry, ActivityEvent, ActivityState } from "./contract";

/** The feed keeps at most this many entries; older ones become a gap marker. */
export const FEED_LIMIT = 1000;

/** Whether an unknown event payload is an `ActivityEvent` of the contract. */
export function isActivityEvent(value: unknown): value is ActivityEvent {
  if (typeof value !== "object" || value === null) return false;
  const entry = (value as { entry?: unknown }).entry;
  if (entry === "gap") return typeof (value as { count?: unknown }).count === "number";
  if (entry !== "item") return false;
  const item = (value as { item?: unknown }).item;
  return (
    typeof item === "object" &&
    item !== null &&
    typeof (item as { id?: unknown }).id === "string" &&
    typeof (item as { operation?: unknown }).operation === "string" &&
    typeof (item as { state?: unknown }).state === "object"
  );
}

/**
 * Adds one event to a feed (oldest first): an item with a known id replaces
 * the earlier one in place, anything else is appended. Past `limit` entries,
 * the oldest are folded into one leading gap marker, so nothing disappears
 * silently.
 */
export function upsertActivity(
  feed: readonly ActivityEntry[],
  event: ActivityEvent,
  limit: number = FEED_LIMIT,
): ActivityEntry[] {
  const out = feed.slice();
  if (event.entry === "item") {
    const index = out.findIndex((e) => e.entry === "item" && e.item.id === event.item.id);
    if (index >= 0) {
      out[index] = event;
      return out;
    }
  }
  out.push(event);
  return trimFeed(out, limit);
}

export function trimFeed(feed: ActivityEntry[], limit: number = FEED_LIMIT): ActivityEntry[] {
  if (feed.length <= limit) return feed;
  let shed = 0;
  let at = "";
  const excess = feed.length - limit + 1;
  for (const entry of feed.slice(0, excess)) {
    if (entry.entry === "gap") {
      shed += entry.count;
      at = at || entry.at;
    } else {
      shed += 1;
      at = at || entry.item.startedAt;
    }
  }
  return [{ entry: "gap", count: shed, at }, ...feed.slice(excess)];
}

export function isFailure(state: ActivityState): boolean {
  return state.state === "failed" || state.state === "timed_out";
}

export function formatElapsed(ms: number): string {
  const seconds = Math.floor(ms / 1000);
  if (seconds < 60) return `${seconds} s`;
  const minutes = Math.floor(seconds / 60);
  if (minutes < 60) return `${minutes} min ${seconds % 60} s`;
  return `${Math.floor(minutes / 60)} h ${minutes % 60} min`;
}

function pad(n: number): string {
  return String(n).padStart(2, "0");
}

/** "14:02" in local time. */
export function formatTime(at: number | string | undefined): string {
  if (at === undefined) return "—";
  const date = new Date(at);
  if (Number.isNaN(date.getTime())) return "—";
  return `${pad(date.getHours())}:${pad(date.getMinutes())}`;
}

export function formatClock(at: string): string {
  const date = new Date(at);
  if (Number.isNaN(date.getTime())) return "";
  return `${pad(date.getHours())}:${pad(date.getMinutes())}:${pad(date.getSeconds())}`;
}

export function formatBytes(bytes: number | undefined): string {
  if (bytes === undefined) return "";
  if (bytes < 1024) return `${bytes} B`;
  if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(1)} KB`;
  if (bytes < 1024 * 1024 * 1024) return `${(bytes / (1024 * 1024)).toFixed(1)} MB`;
  return `${(bytes / (1024 * 1024 * 1024)).toFixed(1)} GB`;
}

export function plural(n: number, one: string, many: string = `${one}s`): string {
  return `${n} ${n === 1 ? one : many}`;
}

/** The approval code in two groups (3 + 3 for six digits). */
export function groupCode(code: string): [string, string] {
  const clean = code.replace(/\s+/g, "");
  const half = Math.ceil(clean.length / 2);
  return [clean.slice(0, half), clean.slice(half)];
}

