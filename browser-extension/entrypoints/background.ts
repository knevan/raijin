import { browser } from 'wxt/browser';
import { defineBackground } from 'wxt/utils/define-background';
import { BackendApi, NetworkError, UnauthorizedError } from '@/utils/backend';
import { getAll } from '@/utils/config';
import { DOWNLOAD_CREATED_MAX_AGE_MS, PENDING_REQUEST_TTL_MS } from '@/utils/constants';
import { getFileFromUrl, shouldCapture } from '@/utils/capture';
import { handleInBackground, sendCheckSelectedToTab } from '@/utils/messaging';
import type { DownloadRequestItem, TestPortResult } from '@/utils/types';

const MENU_LINK_ID = 'raijin-download-link';
const MENU_SELECTION_ID = 'raijin-download-selected';

interface PendingEntry {
  url: string;
  method: string;
  headers: Record<string, string>;
  tabId: number;
  handled: boolean;
  timer: ReturnType<typeof setTimeout> | null;
}

const pending = new Map<string, PendingEntry>();
const tabUrls = new Map<number, string>();
const newTabs = new Map<number, string>();
let bypassPressed = false;

function t(key: string, fallback: string): string {
  try {
    const getMessage = browser.i18n.getMessage as unknown as (name: string) => string;
    const value = getMessage(key);
    return value || fallback;
  } catch {
    return fallback;
  }
}

function setPending(id: string, entry: Omit<PendingEntry, 'timer'>): void {
  const prev = pending.get(id);
  if (prev?.timer) clearTimeout(prev.timer);
  const timer = setTimeout(() => {
    pending.delete(id);
  }, PENDING_REQUEST_TTL_MS);
  // Avoid leaking timers in service workers that may be suspended.
  const maybeUnref = (timer as unknown as { unref?: () => void }).unref;
  if (typeof maybeUnref === 'function') {
    try {
      maybeUnref.call(timer);
    } catch {
      // Ignore; unref is optional.
    }
  }
  pending.set(id, { ...entry, timer });
}

function dropPending(id: string): void {
  const prev = pending.get(id);
  if (prev?.timer) clearTimeout(prev.timer);
  // Keep the entry briefly so downloads.onCreated can still match it.
  setTimeout(() => {
    pending.delete(id);
  }, PENDING_REQUEST_TTL_MS).unref?.();
}

function findPendingByUrl(url: string): PendingEntry | undefined {
  for (const entry of pending.values()) {
    if (entry.url === url) return entry;
  }
  return undefined;
}

// Build request headers for a third-party URL. Values are only ever sent to
// localhost via BackendApi, never to third parties, and never logged.
async function getHeadersForUrl(url: string): Promise<Record<string, string> | null> {
  const config = await getAll();
  if (!config.sendHeaders) return null;
  try {
    const parsed = new URL(url);
    if (parsed.protocol !== 'http:' && parsed.protocol !== 'https:') return null;
    const headers: Record<string, string> = {};
    try {
      const cookies = await browser.cookies.getAll({ url });
      if (cookies.length > 0) {
        headers['Cookie'] = cookies.map((c) => `${c.name}=${c.value}`).join('; ');
      }
    } catch {
      // Cookies are best effort (e.g. permission denied).
    }
    headers['Host'] = parsed.host;
    try {
      if (typeof navigator !== 'undefined' && navigator.userAgent) {
        headers['User-Agent'] = navigator.userAgent;
      }
    } catch {
      // Ignore.
    }
    return headers;
  } catch {
    return null;
  }
}

async function withHeaders(items: DownloadRequestItem[]): Promise<DownloadRequestItem[]> {
  const config = await getAll();
  if (!config.sendHeaders) {
    return items.map((item) => ({ ...item, headers: item.headers ?? null }));
  }
  return Promise.all(
    items.map(async (item) => {
      if (item.headers) return item;
      try {
        const headers = await getHeadersForUrl(item.link);
        return { ...item, headers };
      } catch {
        return { ...item, headers: null };
      }
    }),
  );
}

async function addItems(items: DownloadRequestItem[]): Promise<boolean> {
  if (items.length === 0) return false;
  const config = await getAll();
  const api = new BackendApi(config.port, config.token);
  const options = {
    silentAdd: config.silentAddDownload,
    silentStart: config.silentAddDownload ? config.silentStartDownload : false,
  };
  try {
    if (options.silentAdd && options.silentStart) {
      await api.startHeadless({ items, options });
    } else {
      await api.add({ items, options });
    }
  } catch (err) {
    if (err instanceof NetworkError || err instanceof UnauthorizedError) return false;
    return false;
  }
  if (config.silentAddDownload && items.length === 1) {
    try {
      await browser.notifications.create({
        type: 'basic',
        iconUrl: browser.runtime.getURL('/icon/128.png'),
        title: t('notificationTitle', 'Raijin'),
        message: t('notificationSilentCapture', 'Download captured silently'),
      });
    } catch {
      // Notifications are best effort.
    }
  }
  return true;
}

async function testPort(port?: number, token?: string): Promise<TestPortResult> {
  const config = await getAll();
  const targetPort = port ?? config.port;
  const targetToken = token ?? config.token;
  const pingApi = new BackendApi(targetPort);
  try {
    await pingApi.ping();
  } catch {
    return { ok: false, reason: 'down' };
  }
  if (!targetToken) return { ok: false, reason: 'unauthorized' };
  try {
    await new BackendApi(targetPort, targetToken).probeAuth();
    return { ok: true, reason: 'ok' };
  } catch (err) {
    if (err instanceof UnauthorizedError) return { ok: false, reason: 'unauthorized' };
    // Auth probe endpoints may not exist on older desktop builds; a
    // successful ping is enough to report reachable in that case.
    if (err instanceof NetworkError) return { ok: false, reason: 'down' };
    return { ok: true, reason: 'ok' };
  }
}

async function cancelDownload(id: number): Promise<void> {
  try {
    await browser.downloads.cancel(id);
  } catch {
    // Best effort.
  }
  try {
    await browser.downloads.erase({ id });
  } catch {
    // Best effort.
  }
  try {
    const removeFile = (browser.downloads as unknown as { removeFile?: (id: number) => Promise<void> })
      .removeFile;
    if (typeof removeFile === 'function') await removeFile.call(browser.downloads, id);
  } catch {
    // Best effort; partial files may remain.
  }
}

async function closeNewTabIfCaptured(tabId: number): Promise<void> {
  const config = await getAll();
  if (!config.closeNewTabIfItWasCaptured) return;
  if (!newTabs.has(tabId)) return;
  newTabs.delete(tabId);
  try {
    await browser.tabs.remove(tabId);
  } catch {
    // Tab may already be gone.
  }
}

async function ensureContextMenus(): Promise<void> {
  try {
    await browser.contextMenus.removeAll();
  } catch {
    // Ignore when nothing exists.
  }
  try {
    browser.contextMenus.create({
      id: MENU_SELECTION_ID,
      title: t('contextMenuDownloadSelected', 'Download selected links with Raijin'),
      contexts: ['selection'],
    });
    browser.contextMenus.create({
      id: MENU_LINK_ID,
      title: t('contextMenuDownloadLink', 'Download with Raijin'),
      contexts: ['link', 'audio', 'video', 'image'],
    });
  } catch {
    // Menu creation is best effort.
  }
}

function trackTabs(): void {
  try {
    browser.tabs.onCreated.addListener((tab) => {
      if (tab.id !== undefined) {
        const url = tab.url ?? tab.pendingUrl ?? '';
        if (url) newTabs.set(tab.id, url);
        if (tab.url) tabUrls.set(tab.id, tab.url);
      }
    });
    browser.tabs.onUpdated.addListener((tabId, changeInfo, tab) => {
      if (typeof changeInfo.url === 'string') newTabs.delete(tabId);
      const url = tab.url ?? (typeof changeInfo.url === 'string' ? changeInfo.url : undefined);
      if (url) tabUrls.set(tabId, url);
    });
    browser.tabs.onRemoved.addListener((tabId) => {
      newTabs.delete(tabId);
      tabUrls.delete(tabId);
    });
  } catch {
    // Tabs tracking is best effort.
  }
}

// Best-effort non-blocking observers. No blocking/webRequestBlocking is used;
// when the permission is absent the map simply stays empty and the
// downloads.onCreated fallback still works via getHeadersForUrl.
function observeWebRequests(): void {
  try {
    // eslint-disable-next-line @typescript-eslint/no-explicit-any
    const webRequest = (browser as any).webRequest as
      | {
          // eslint-disable-next-line @typescript-eslint/no-explicit-any
          onSendHeaders?: { addListener: (...args: any[]) => void };
          // eslint-disable-next-line @typescript-eslint/no-explicit-any
          onCompleted?: { addListener: (...args: any[]) => void };
          // eslint-disable-next-line @typescript-eslint/no-explicit-any
          onErrorOccurred?: { addListener: (...args: any[]) => void };
        }
      | undefined;
    if (!webRequest?.onSendHeaders) return;
    const filter = { urls: ['*://*/*'] };
    webRequest.onSendHeaders.addListener(
      // eslint-disable-next-line @typescript-eslint/no-explicit-any
      (details: any) => {
        const requestId = details?.requestId as string | undefined;
        const url = details?.url as string | undefined;
        if (!requestId || !url) return;
        const headers: Record<string, string> = {};
        const rawHeaders = details?.requestHeaders as
          | Array<{ name?: string; value?: string }>
          | undefined;
        for (const h of rawHeaders ?? []) {
          if (h.name && h.value) headers[h.name] = h.value;
        }
        setPending(requestId, {
          url,
          method: (details?.method as string | undefined) ?? 'GET',
          headers,
          tabId: (details?.tabId as number | undefined) ?? -1,
          handled: false,
        });
      },
      filter,
      ['requestHeaders'],
    );
    // eslint-disable-next-line @typescript-eslint/no-explicit-any
    const cleanup = (details: any): void => {
      const requestId = details?.requestId as string | undefined;
      if (requestId) dropPending(requestId);
    };
    webRequest.onCompleted?.addListener(cleanup, filter);
    webRequest.onErrorOccurred?.addListener(cleanup, filter);
  } catch {
    // WebRequest observers are optional.
  }
}

function downloadAgeMs(startTime: unknown): number | null {
  if (typeof startTime !== 'string' || !startTime) return null;
  const parsed = Date.parse(startTime);
  if (Number.isNaN(parsed)) return null;
  return Date.now() - parsed;
}

async function handleDownloadCreated(details: unknown): Promise<void> {
  const d = details as {
    id?: number;
    url?: string;
    referrer?: string;
    filename?: string;
    mime?: string;
    fileSize?: number;
    totalBytes?: number;
    startTime?: string;
    endTime?: string;
    state?: string;
    byExtensionId?: string;
  };
  const config = await getAll();
  if (!config.autoCaptureLinks) return;
  const id = d.id;
  const url = d.url ?? '';
  if (id === undefined || !url) return;
  if (!url.startsWith('http://') && !url.startsWith('https://')) return;
  if (d.byExtensionId) return;
  if (typeof d.state === 'string' && d.state === 'complete') return;
  if (typeof d.endTime === 'string' && d.endTime) return;
  const age = downloadAgeMs(d.startTime);
  if (age !== null && age > DOWNLOAD_CREATED_MAX_AGE_MS) return;

  const intercepted = findPendingByUrl(url);
  if (intercepted?.handled) {
    await cancelDownload(id);
    return;
  }
  if (intercepted && intercepted.method.toUpperCase() !== 'GET') return;

  const downloadPage = d.referrer || null;
  const fileSizeBytes =
    typeof d.fileSize === 'number'
      ? d.fileSize
      : typeof d.totalBytes === 'number' && d.totalBytes >= 0
        ? d.totalBytes
        : null;
  const ok = shouldCapture(
    {
      url,
      downloadPage,
      fileSizeBytes,
      contentType: typeof d.mime === 'string' ? d.mime : null,
      suggestedName: typeof d.filename === 'string' ? d.filename : getFileFromUrl(url),
      method: intercepted?.method ?? 'GET',
    },
    config,
    bypassPressed,
  );
  if (!ok) return;

  let headers: Record<string, string> | null = null;
  if (intercepted && Object.keys(intercepted.headers).length > 0) {
    headers = { ...intercepted.headers };
  } else {
    headers = await getHeadersForUrl(url);
  }
  const item: DownloadRequestItem = {
    link: url,
    downloadPage,
    headers,
    description: null,
    suggestedName: typeof d.filename === 'string' ? d.filename : null,
    type: 'http',
  };
  const accepted = await addItems([item]);
  if (!accepted) {
    // Server down and pass-through allowed: leave the browser download alone.
    if (config.allowPassDownloadIfAppNotRespond) return;
    return;
  }
  if (intercepted) intercepted.handled = true;
  await cancelDownload(id);
  if (intercepted && intercepted.tabId >= 0) await closeNewTabIfCaptured(intercepted.tabId);
}

function handleContextMenu(): void {
  try {
    browser.contextMenus.onClicked.addListener((info) => {
      void (async () => {
        if (info.menuItemId === MENU_LINK_ID) {
          const link = (info.linkUrl || info.srcUrl || '').trim();
          if (!link) return;
          const headers = await getHeadersForUrl(link);
          const linkText = (info as unknown as { linkText?: unknown }).linkText;
          await addItems([
            {
              link,
              downloadPage: info.pageUrl ?? null,
              headers,
              description: typeof linkText === 'string' ? linkText : null,
              suggestedName: null,
              type: 'http',
            },
          ]);
        } else if (info.menuItemId === MENU_SELECTION_ID) {
          try {
            const tabs = await browser.tabs.query({ active: true, currentWindow: true });
            const tabId = tabs[0]?.id;
            if (tabId !== undefined) await sendCheckSelectedToTab(tabId);
          } catch {
            // Best effort.
          }
        }
      })();
    });
  } catch {
    // Context menus are optional.
  }
}

function handleDownloads(): void {
  try {
    const onCreated = browser.downloads?.onCreated;
    if (!onCreated) return;
    onCreated.addListener((details) => {
      void handleDownloadCreated(details as unknown);
    });
  } catch {
    // downloads.onCreated is optional (e.g. some Firefox builds);
    // menu + selection flows keep working.
  }
}

async function boot(): Promise<void> {
  try {
    await ensureContextMenus();
  } catch {
    // Continue without menus.
  }
  trackTabs();
  observeWebRequests();
  handleContextMenu();
  handleDownloads();
  handleInBackground({
    addDownload: async (items) => addItems(await withHeaders(items)),
    testPort: (port, token) => testPort(port, token),
    getHeaders: async (urls) => Promise.all(urls.map((u) => getHeadersForUrl(u))),
    setBypass: (pressed) => {
      bypassPressed = pressed;
    },
  });
}

export default defineBackground({
  main() {
    void boot();
  },
});
