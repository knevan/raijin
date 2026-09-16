import { defineContentScript } from 'wxt/utils/define-content-script';
import { browser } from 'wxt/browser';
import { storage } from 'wxt/utils/storage';
import { DEFAULT_BYPASS_KEY } from '@/utils/constants';
import { normalizeAndFilterLinks } from '@/utils/capture';
import { sendAddDownload, sendSetBypass } from '@/utils/messaging';
import type { DownloadRequestItem } from '@/utils/types';

const BYPASS_KEY = 'local:bypassShortcut';

async function currentBypassKey(): Promise<string> {
  try {
    const value = await storage.getItem<string>(BYPASS_KEY, { fallback: DEFAULT_BYPASS_KEY });
    return value || DEFAULT_BYPASS_KEY;
  } catch {
    return DEFAULT_BYPASS_KEY;
  }
}

function collectSelectedLinks(): DownloadRequestItem[] {
  const selection = window.getSelection();
  if (!selection || selection.rangeCount === 0) return [];
  const anchors = Array.from(document.querySelectorAll<HTMLAnchorElement>('a[href]'));
  const hrefs: string[] = [];
  const texts = new Map<string, string>();
  for (const anchor of anchors) {
    try {
      if (!selection.containsNode(anchor, true)) continue;
    } catch {
      continue;
    }
    if (!anchor.href) continue;
    hrefs.push(anchor.href);
    const label = (anchor.innerText || anchor.textContent || '').trim();
    if (label && !texts.has(anchor.href)) texts.set(anchor.href, label);
  }
  const links = normalizeAndFilterLinks(hrefs, document.baseURI);
  const page = location.href;
  return links.map((link) => ({
    link,
    downloadPage: page,
    headers: null,
    description: texts.get(link) ?? null,
    suggestedName: null,
    type: 'http' as const,
  }));
}

export default defineContentScript({
  matches: ['*://*/*'],
  runAt: 'document_idle',
  allFrames: false,
  main(ctx) {
    let bypassKey = DEFAULT_BYPASS_KEY;
    let pressed = false;
    let disposed = false;

    void currentBypassKey().then((key) => {
      bypassKey = key;
    });
    const unwatch = storage.watch<string>(BYPASS_KEY, (next) => {
      bypassKey = next || DEFAULT_BYPASS_KEY;
    });

    const setPressed = (next: boolean): void => {
      if (pressed === next || disposed) return;
      pressed = next;
      void sendSetBypass(next).catch(() => {
        // Background may be unavailable; ignore.
      });
    };

    const onKeyDown = (event: KeyboardEvent): void => {
      if (event.key === bypassKey) setPressed(true);
    };
    const onKeyUp = (event: KeyboardEvent): void => {
      if (event.key === bypassKey) setPressed(false);
    };
    const onBlur = (): void => setPressed(false);

    const onMessage = (message: unknown): boolean | Promise<boolean> => {
      if (
        typeof message === 'object' &&
        message !== null &&
        (message as { type?: unknown }).type === 'raijin:check_selected_text_for_links'
      ) {
        const items = collectSelectedLinks();
        if (items.length > 0) {
          return sendAddDownload(items)
            .then(() => true)
            .catch(() => false);
        }
        return Promise.resolve(false);
      }
      return false;
    };

    document.addEventListener('keydown', onKeyDown);
    document.addEventListener('keyup', onKeyUp);
    window.addEventListener('blur', onBlur);
    browser.runtime.onMessage.addListener(onMessage);

    ctx.onInvalidated(() => {
      disposed = true;
      document.removeEventListener('keydown', onKeyDown);
      document.removeEventListener('keyup', onKeyUp);
      window.removeEventListener('blur', onBlur);
      try {
        browser.runtime.onMessage.removeListener(onMessage);
      } catch {
        // Best effort.
      }
      try {
        unwatch();
      } catch {
        // Best effort.
      }
      setPressed(false);
    });
  },
});
