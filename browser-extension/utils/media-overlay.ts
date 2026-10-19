import type { AddMediaResult, MediaUrlState } from './types';
import { classifyMediaUrl, pickPrimaryVideo } from './media';

const HOST_ID = 'raijin-media-overlay';
const BOX_SIZE = 32;
const EDGE_MARGIN = 8;
const OFFSET_FROM_VIDEO = 8;
const HIDE_GRACE_MS = 2500;
const SENT_FEEDBACK_MS = 1200;
const EMPTY_SRC_KEY = '\u0000no-source';

/**
 * Visual states. `detecting` is deliberately animation-free: a permanent
 * spinner would promise work that never happens on MSE (`blob:`) players.
 */
export type OverlayState = 'hidden' | 'detecting' | 'ready' | 'unavailable' | 'offline' | 'sent';

export interface OverlayHost {
  addMedia: (item: { link: string }) => Promise<AddMediaResult>;
  isEnabled: () => boolean;
}

const BASE_STYLE = `
:host { all: initial; }
.root {
  all: initial;
  position: relative;
  display: block;
  width: ${BOX_SIZE}px;
  height: ${BOX_SIZE}px;
  font-family: "Segoe UI", Roboto, Helvetica, Arial, sans-serif;
  font-size: 11px;
  line-height: 1;
  box-sizing: border-box;
}
.btn {
  all: initial;
  position: absolute;
  inset: 0;
  display: flex;
  align-items: center;
  justify-content: center;
  gap: 2px;
  box-sizing: border-box;
  border-radius: 8px;
  border: 1px solid rgba(255, 255, 255, 0.28);
  background: rgba(23, 24, 32, 0.92);
  color: #f2f2f2;
  cursor: pointer;
  transition: opacity 120ms ease, background 120ms ease;
}
.btn:focus-visible { outline: 2px solid #6aa9ff; outline-offset: 2px; }
.btn svg { width: 16px; height: 16px; display: block; }
.root[data-state="detecting"] .btn { opacity: 0.55; }
.root[data-state="detecting"] .btn svg { color: #d0d0d0; }
.root[data-state="ready"] .btn { opacity: 1; border-color: rgba(106, 169, 255, 0.65); }
/* A 1px border alone is too weak a clickability signal on some page backgrounds. */
.root[data-state="ready"] .btn svg { color: #6aa9ff; }
.root[data-state="ready"] .btn:hover { background: rgba(45, 68, 110, 0.96); }
.root[data-state="sent"] .btn { opacity: 1; border-color: rgba(80, 190, 120, 0.7); color: #7fe0a0; }
.root[data-state="unavailable"] .btn { opacity: 0.4; cursor: default; }
.root[data-state="offline"] .btn { opacity: 1; border-color: rgba(207, 34, 46, 0.6); }
.root[data-state="offline"] .dot {
  position: absolute;
  top: 3px;
  right: 3px;
  width: 6px;
  height: 6px;
  border-radius: 50%;
  background: #cf222e;
}
.root[data-state="busy"] .btn { cursor: progress; }
.spinner {
  width: 14px;
  height: 14px;
  border-radius: 50%;
  border: 2px solid rgba(255, 255, 255, 0.25);
  border-top-color: #f2f2f2;
  animation: raijin-spin 700ms linear infinite;
}
@keyframes raijin-spin { to { transform: rotate(360deg); } }
@media (prefers-reduced-motion: reduce) {
  .spinner { animation: none; border-top-color: rgba(255, 255, 255, 0.6); }
  .btn { transition: none; }
}
.close {
  all: initial;
  position: absolute;
  top: -5px;
  right: -5px;
  width: 14px;
  height: 14px;
  display: none;
  align-items: center;
  justify-content: center;
  border-radius: 50%;
  background: rgba(23, 24, 32, 0.95);
  border: 1px solid rgba(255, 255, 255, 0.3);
  color: #d0d0d0;
  font-size: 10px;
  line-height: 1;
  cursor: pointer;
  box-sizing: border-box;
}
.root:hover .close { display: flex; }
.close:hover { color: #fff; }
`;

const SVG_NS = 'http://www.w3.org/2000/svg';

// Built with createElementNS so no markup is ever parsed.
function iconSvg(paths: readonly string[]): SVGSVGElement {
  const svg = document.createElementNS(SVG_NS, 'svg');
  svg.setAttribute('viewBox', '0 0 16 16');
  svg.setAttribute('fill', 'none');
  svg.setAttribute('stroke', 'currentColor');
  svg.setAttribute('stroke-width', '1.6');
  svg.setAttribute('stroke-linecap', 'round');
  svg.setAttribute('stroke-linejoin', 'round');
  svg.setAttribute('aria-hidden', 'true');
  for (const d of paths) {
    const path = document.createElementNS(SVG_NS, 'path');
    path.setAttribute('d', d);
    svg.appendChild(path);
  }
  return svg;
}

const ICON_DOWNLOAD = ['M8 3v7', 'M5 7.5 8 10.5l3-3', 'M3 13h10'];
const ICON_CHECK = ['M3.5 8.5 6.5 11.5l6-6.5'];

/**
 * Floating download button anchored to the primary <video>.
 *
 * Presence is driven by the DOM alone (a qualifying <video> exists), while the
 * enabled state is driven by the media URL. Keeping those two signals apart is
 * what stops the button from flickering as network candidates come and go.
 */
export class MediaOverlay {
  private host: HTMLElement | null = null;
  private root: HTMLElement | null = null;
  private button: HTMLButtonElement | null = null;
  private renderedIcon: 'spinner' | 'check' | 'download' | null = null;
  private renderedDot = false;
  private state: OverlayState = 'hidden';
  private offlineReason: 'down' | 'unauthorized' = 'down';
  private busy = false;
  private dismissedForSrc: string | null = null;
  private lastSrc = '';
  private lastPosition = { left: -1, top: -1 };
  private hideTimer: ReturnType<typeof setTimeout> | null = null;
  private sentTimer: ReturnType<typeof setTimeout> | null = null;
  private positionFrame: number | null = null;
  private video: HTMLVideoElement | null = null;
  private observer: IntersectionObserver | null = null;
  private destroyed = false;

  constructor(private readonly options: OverlayHost) {}

  start(): void {
    document.addEventListener('scroll', this.onViewportChange, { capture: true, passive: true });
    window.addEventListener('resize', this.onViewportChange);
    document.addEventListener('fullscreenchange', this.onFullscreenChange);
    this.sync();
  }

  /** Re-read the DOM; safe to call on every mutation batch. */
  sync(): void {
    if (this.destroyed) return;
    if (!this.options.isEnabled()) {
      this.teardown();
      return;
    }
    const video = pickPrimaryVideo(document.querySelectorAll<HTMLVideoElement>('video'));
    if (!video) {
      this.detachVideo();
      // Grace window: SPA players are often replaced mid-render.
      if (this.hideTimer === null) {
        this.hideTimer = setTimeout(() => {
          this.hideTimer = null;
          this.detachVideo();
          this.apply('hidden');
        }, HIDE_GRACE_MS);
      }
      return;
    }
    if (this.hideTimer !== null) {
      clearTimeout(this.hideTimer);
      this.hideTimer = null;
    }
    if (this.video !== video) this.attachVideo(video);
    this.evaluate();
    this.schedulePosition();
  }

  destroy(): void {
    this.destroyed = true;
    document.removeEventListener('scroll', this.onViewportChange, { capture: true });
    window.removeEventListener('resize', this.onViewportChange);
    document.removeEventListener('fullscreenchange', this.onFullscreenChange);
    if (this.hideTimer !== null) clearTimeout(this.hideTimer);
    if (this.sentTimer !== null) clearTimeout(this.sentTimer);
    if (this.positionFrame !== null) cancelAnimationFrame(this.positionFrame);
    this.detachVideo();
    this.teardown();
  }

  private attachVideo(video: HTMLVideoElement): void {
    this.detachVideo();
    this.video = video;
    // Both events matter: metadata lands when a source is first chosen, emptied
    // fires when a player tears its source down during SPA navigation.
    video.addEventListener('loadedmetadata', this.onMediaEvent);
    video.addEventListener('emptied', this.onMediaEvent);
    const observer = new IntersectionObserver((entries) => {
      const entry = entries[0];
      if (!entry) return;
      if (entry.isIntersecting) {
        this.schedulePosition();
      } else {
        this.setHostVisible(false);
      }
    });
    observer.observe(video);
    this.observer = observer;
  }

  private detachVideo(): void {
    if (this.video !== null) {
      this.video.removeEventListener('loadedmetadata', this.onMediaEvent);
      this.video.removeEventListener('emptied', this.onMediaEvent);
    }
    if (this.observer !== null) {
      this.observer.disconnect();
      this.observer = null;
    }
    this.video = null;
  }

  private evaluate(): void {
    const video = this.video;
    if (!video) return;
    const src = video.currentSrc;
    // Empty source still needs a stable identity so "hide for this video"
    // survives while the player has not picked a source yet.
    const srcKey = src || EMPTY_SRC_KEY;
    if (src !== this.lastSrc) {
      // A new source means a new video (SPA navigation, re-render): previous
      // failures no longer describe this player.
      this.lastSrc = src;
      if (this.dismissedForSrc !== null && this.dismissedForSrc !== srcKey) {
        this.dismissedForSrc = null;
      }
      if (this.state === 'offline' || this.state === 'unavailable' || this.state === 'sent') {
        this.state = 'hidden';
      }
    }
    if (this.dismissedForSrc === srcKey) {
      this.apply('hidden');
      return;
    }
    const urlState: MediaUrlState = classifyMediaUrl(src, location.href);
    if (urlState !== 'ready') {
      this.apply(urlState);
      return;
    }
    // Failures are sticky for as long as the source is unchanged, otherwise a
    // DOM mutation would silently clear the offline indicator.
    if (this.state === 'offline' || this.state === 'sent') {
      this.apply(this.state);
      return;
    }
    this.apply('ready');
  }

  private readonly onMediaEvent = (): void => {
    this.evaluate();
    this.schedulePosition();
  };

  private readonly onViewportChange = (): void => {
    this.schedulePosition();
  };

  private readonly onFullscreenChange = (): void => {
    const fullscreen = document.fullscreenElement;
    const target =
      fullscreen instanceof HTMLElement && !(fullscreen instanceof HTMLVideoElement)
        ? fullscreen
        : document.body;
    if (this.host && this.host.parentElement !== target) target.appendChild(this.host);
    this.schedulePosition();
  };

  private apply(state: OverlayState): void {
    if (state === 'hidden') {
      this.state = 'hidden';
      this.setHostVisible(false);
      return;
    }
    this.ensureMounted();
    if (!this.host || !this.root || !this.button) return;
    this.setHostVisible(true);
    this.state = state;
    // render() is idempotent and mutation-guarded, so repeating the same state
    // still refreshes the busy indicator without thrashing the DOM.
    this.render(state);
  }

  private render(state: OverlayState): void {
    const root = this.root;
    const button = this.button;
    if (!root || !button) return;
    const nextState = this.busy ? 'busy' : state;
    if (root.dataset['state'] !== nextState) root.dataset['state'] = nextState;
    const title = this.titleFor(state);
    button.disabled = state === 'unavailable' || this.busy;
    // Rendering runs on every sync, and the page-wide MutationObserver sees any
    // appended node. Keep each write conditional so idle frames stay silent.
    if (button.getAttribute('title') !== title) {
      button.setAttribute('title', title);
      button.setAttribute('aria-label', title);
    }
    const icon = this.iconFor(state);
    const withDot = !this.busy && state === 'offline';
    if (this.renderedIcon === icon && this.renderedDot === withDot) return;
    this.renderedIcon = icon;
    this.renderedDot = withDot;
    button.replaceChildren();
    if (icon === 'spinner') {
      const spinner = document.createElement('span');
      spinner.className = 'spinner';
      button.appendChild(spinner);
      return;
    }
    button.appendChild(iconSvg(icon === 'check' ? ICON_CHECK : ICON_DOWNLOAD));
    if (withDot) {
      const dot = document.createElement('span');
      dot.className = 'dot';
      button.appendChild(dot);
    }
  }

  private iconFor(state: OverlayState): 'spinner' | 'check' | 'download' {
    if (this.busy) return 'spinner';
    return state === 'sent' ? 'check' : 'download';
  }

  private titleFor(state: OverlayState): string {
    switch (state) {
      case 'ready':
        return 'Download with Raijin';
      case 'detecting':
        return 'Looking for a downloadable video file';
      case 'unavailable':
        return 'No direct video file on this page (streaming formats are not supported yet)';
      case 'offline':
        return this.offlineReason === 'unauthorized'
          ? 'Raijin rejected the token. Update it in the extension popup'
          : 'Raijin is not running';
      case 'sent':
        return 'Sent to Raijin';
      case 'hidden':
        return 'Raijin';
      default:
        return 'Raijin';
    }
  }

  private ensureMounted(): void {
    if (this.host) return;
    if (!document.body) return;
    const host = document.createElement('div');
    host.id = HOST_ID;
    host.style.all = 'initial';
    host.style.position = 'fixed';
    host.style.top = '0';
    host.style.left = '0';
    host.style.width = `${BOX_SIZE}px`;
    host.style.height = `${BOX_SIZE}px`;
    host.style.zIndex = '2147483647';
    host.style.display = 'none';
    host.style.pointerEvents = 'auto';

    const shadow = host.attachShadow({ mode: 'open' });
    const style = document.createElement('style');
    style.textContent = BASE_STYLE;
    shadow.appendChild(style);

    const root = document.createElement('div');
    root.className = 'root';
    root.dataset['state'] = 'detecting';

    const button = document.createElement('button');
    button.className = 'btn';
    button.type = 'button';
    button.addEventListener('click', this.onClick);

    const close = document.createElement('button');
    close.className = 'close';
    close.type = 'button';
    close.textContent = '\u00d7';
    close.setAttribute('title', 'Hide for this video');
    close.setAttribute('aria-label', 'Hide the Raijin download button for this video');
    close.addEventListener('click', this.onDismiss);

    root.append(button, close);
    shadow.appendChild(root);

    const container = document.fullscreenElement;
    const parent =
      container instanceof HTMLElement && !(container instanceof HTMLVideoElement)
        ? container
        : document.body;
    parent.appendChild(host);

    this.host = host;
    this.root = root;
    this.button = button;
  }

  private teardown(): void {
    this.state = 'hidden';
    this.busy = false;
    if (this.sentTimer !== null) {
      clearTimeout(this.sentTimer);
      this.sentTimer = null;
    }
    if (this.hideTimer !== null) {
      clearTimeout(this.hideTimer);
      this.hideTimer = null;
    }
    if (this.positionFrame !== null) {
      cancelAnimationFrame(this.positionFrame);
      this.positionFrame = null;
    }
    this.host?.remove();
    this.host = null;
    this.root = null;
    this.button = null;
    this.renderedIcon = null;
    this.renderedDot = false;
    this.lastPosition = { left: -1, top: -1 };
  }

  private setHostVisible(visible: boolean): void {
    if (!this.host) return;
    const next = visible ? 'block' : 'none';
    if (this.host.style.display !== next) this.host.style.display = next;
  }

  private schedulePosition(): void {
    if (this.positionFrame !== null || this.destroyed) return;
    this.positionFrame = requestAnimationFrame(() => {
      this.positionFrame = null;
      this.positionNow();
    });
  }

  private positionNow(): void {
    const host = this.host;
    const video = this.video;
    if (!host || !video || this.state === 'hidden') return;
    if (document.hidden) return;
    const rect = video.getBoundingClientRect();
    const visible =
      rect.width > 0 &&
      rect.height > 0 &&
      rect.bottom > 0 &&
      rect.right > 0 &&
      rect.top < window.innerHeight &&
      rect.left < window.innerWidth;
    if (!visible) {
      this.setHostVisible(false);
      return;
    }
    const maxLeft = window.innerWidth - BOX_SIZE - EDGE_MARGIN;
    const maxTop = window.innerHeight - BOX_SIZE - EDGE_MARGIN;
    const left = Math.max(EDGE_MARGIN, Math.min(rect.left + OFFSET_FROM_VIDEO, maxLeft));
    const top = Math.max(EDGE_MARGIN, Math.min(rect.top + OFFSET_FROM_VIDEO, maxTop));
    if (left === this.lastPosition.left && top === this.lastPosition.top) {
      this.setHostVisible(true);
      return;
    }
    this.lastPosition = { left, top };
    host.style.left = `${left}px`;
    host.style.top = `${top}px`;
    this.setHostVisible(true);
  }

  private readonly onClick = (): void => {
    void this.submit();
  };

  private readonly onDismiss = (): void => {
    this.dismissedForSrc = this.lastSrc || EMPTY_SRC_KEY;
    this.apply('hidden');
  };

  private async submit(): Promise<void> {
    if (this.busy) return;
    if (this.state !== 'ready' && this.state !== 'offline') return;
    const src = this.video?.currentSrc?.trim() ?? '';
    if (classifyMediaUrl(src, location.href) !== 'ready') return;
    this.busy = true;
    this.render(this.state);
    try {
      const result = await this.options.addMedia({ link: src });
      if (result.ok) {
        this.busy = false;
        this.apply('sent');
        if (this.sentTimer !== null) clearTimeout(this.sentTimer);
        this.sentTimer = setTimeout(() => {
          this.sentTimer = null;
          if (this.state === 'sent') this.apply('ready');
        }, SENT_FEEDBACK_MS);
        return;
      }
      this.busy = false;
      if (result.reason === 'unauthorized') {
        this.offlineReason = 'unauthorized';
        this.apply('offline');
        return;
      }
      this.offlineReason = 'down';
      this.apply('offline');
    } catch (err) {
      this.busy = false;
      this.offlineReason = 'down';
      this.apply('offline');
      // Surface the failure instead of silently swallowing it.
      console.error('raijin: failed to send media download', err);
    }
  }
}

