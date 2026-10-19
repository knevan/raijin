import type { MediaElementLike, MediaUrlState } from './types';

// Smaller boxes are almost always thumbnails, previews or decorative players.
export const MIN_VIDEO_WIDTH = 200;
export const MIN_VIDEO_HEIGHT = 120;

export function hasUsableBox(element: MediaElementLike): boolean {
  return element.clientWidth >= MIN_VIDEO_WIDTH && element.clientHeight >= MIN_VIDEO_HEIGHT;
}

/**
 * Classify the source of a <video> element without touching the network.
 *
 * `detecting` is a real state, not a transient loading flag: the element exists
 * but the browser has not picked a source yet. `unavailable` is the honest
 * answer for MSE pipelines (`blob:`), whose media cannot be fetched directly.
 */
export function classifyMediaUrl(currentSrc: string, baseUrl?: string): MediaUrlState {
  const raw = currentSrc.trim();
  if (!raw) return 'detecting';
  let parsed: URL;
  try {
    parsed = baseUrl === undefined ? new URL(raw) : new URL(raw, baseUrl);
  } catch {
    return 'unavailable';
  }
  if (parsed.protocol === 'http:' || parsed.protocol === 'https:') return 'ready';
  return 'unavailable';
}

/** Largest qualifying video wins, so overlay anchors on the primary player. */
export function pickPrimaryVideo<T extends MediaElementLike>(elements: Iterable<T>): T | null {
  let best: T | null = null;
  let bestArea = 0;
  for (const element of elements) {
    if (!hasUsableBox(element)) continue;
    const area = element.clientWidth * element.clientHeight;
    if (area > bestArea) {
      best = element;
      bestArea = area;
    }
  }
  return best;
}
