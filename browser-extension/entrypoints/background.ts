import { browser } from 'wxt/browser';
import { defineBackground } from 'wxt/utils/define-background';
import { ApiError, BackendApi, NetworkError, UnauthorizedError } from '@/utils/backend';
import { getAll } from '@/utils/config';
import {
  ADD_TIMEOUT_MS,
  DOWNLOAD_CREATED_MAX_AGE_MS,
  DOWNLOAD_METADATA_WAIT_MS,
  MAX_PENDING_DOWNLOADS,
  PENDING_REQUEST_TTL_MS,
} from '@/utils/constants';
import { captureDecision, getFileExtension, isBlacklisted, responseMetadata } from '@/utils/capture';
import { handleInBackground, sendCheckSelectedToTab } from '@/utils/messaging';
import type {
  AddMediaFailure,
  DownloadRequestItem,
  TestPortResult,
} from '@/utils/types';

const MENU_LINK_ID = 'raijin-download-link';
const MENU_SELECTION_ID = 'raijin-download-selected';

interface PendingEntry {
  url: string;
  urls: Set<string>;
  downloadId?: number;
  handoff?: Promise<boolean | null>;
  browserFallback?: boolean;
  method: string;
  headers: Record<string, string>;
  tabId: number;
  timer: ReturnType<typeof setTimeout> | null;
}

const pending = new Map<string, PendingEntry>();
const tabUrls = new Map<number, string>();
const newTabs = new Map<number, string>();
let bypassPressed = false;

function t(key: Parameters<typeof browser.i18n.getMessage>[0], fallback: string): string {
  try {
    const value = browser.i18n.getMessage(key);
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
  pending.set(id, { ...entry, timer });
}

function dropPending(id: string): void {
  const entry = pending.get(id);
  if (!entry) return;
  if (entry.timer) clearTimeout(entry.timer);
  entry.timer = setTimeout(() => pending.delete(id), PENDING_REQUEST_TTL_MS);
}

function findPendingByUrl(url: string, downloadId: number): PendingEntry | undefined {
  const entries = [...pending.values()].reverse();
  const bound = entries.find((entry) => entry.downloadId === downloadId);
  if (bound) return bound;
  const entry = entries.find((entry) => entry.urls.has(url) && entry.downloadId === undefined);
  if (entry) entry.downloadId = downloadId;
  return entry;
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
  return (await submitItems(items)).ok;
}

async function submitItems(
  items: DownloadRequestItem[],
  isActive?: () => boolean,
): Promise<{ ok: true } | { ok: false; reason: AddMediaFailure }> {
  if (items.length === 0) return { ok: false, reason: 'error' };
  const config = await getAll();
  if (isActive && !isActive()) return { ok: false, reason: 'error' };
  const api = new BackendApi(config.port, config.token);
  const options = {
    silentAdd: Boolean(config.token) && config.silentAddDownload,
    silentStart: Boolean(config.token) && config.silentAddDownload && config.silentStartDownload,
  };
  try {
    if (options.silentAdd && options.silentStart) {
      await api.startHeadless({ items, options });
    } else {
      await api.add({ items, options });
    }
  } catch (err) {
    if (err instanceof NetworkError) return { ok: false, reason: 'down' };
    if (err instanceof UnauthorizedError) return { ok: false, reason: 'unauthorized' };
    if (err instanceof ApiError) return { ok: false, reason: 'error' };
    throw err;
  }
  if (options.silentAdd && items.length === 1) {
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
  return { ok: true };
}

async function testPort(port?: number, token?: string): Promise<TestPortResult> {
  const config = await getAll();
  const targetPort = port ?? config.port;
  const targetToken = token ?? config.token;
  const pingApi = new BackendApi(targetPort);
  let basicAccess: boolean;
  try {
    basicAccess = await pingApi.ping();
  } catch {
    return { ok: false, reason: 'down' };
  }
  if (!targetToken) return basicAccess
    ? { ok: true, reason: 'basic' }
    : { ok: false, reason: 'unauthorized' };
  try {
    await new BackendApi(targetPort, targetToken).probeAuth();
    return { ok: true, reason: 'ok' };
  } catch (err) {
    if (err instanceof UnauthorizedError) return { ok: false, reason: 'unauthorized' };
    if (err instanceof NetworkError || err instanceof ApiError) return { ok: false, reason: 'down' };
    throw err;
  }
}

async function cancelDownload(id: number): Promise<boolean> {
  try {
    await browser.downloads.cancel(id);
  } catch (error) {
    console.warn('Raijin browser cancellation failed', {
      downloadId: id,
      errorName: error instanceof Error ? error.name : 'UnknownError',
    });
    return false;
  }
  trackedDownloads.get(id)?.releaseFilename?.();
  try {
    await browser.downloads.erase({ id });
  } catch {
    // Best effort.
  }
  try {
    await browser.downloads.removeFile?.(id);
  } catch {
    // Best effort; partial files may remain.
  }
  return true;
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

function observeWebRequests(): void {
  try {
    const webRequest = browser.webRequest;
    if (!webRequest?.onSendHeaders) return;
    const filter = { urls: ['*://*/*'] };
    webRequest.onSendHeaders.addListener(
      (details) => {
        const { requestId, url } = details;
        const headers: Record<string, string> = {};
        for (const h of details.requestHeaders ?? []) {
          if (h.name && h.value) headers[h.name] = h.value;
        }
        const previous = pending.get(requestId);
        if (!previous && pending.size >= MAX_PENDING_DOWNLOADS) {
          const disposable = [...pending].find(([, entry]) => !entry.handoff && entry.downloadId === undefined);
          if (!disposable) return;
          if (disposable[1].timer) clearTimeout(disposable[1].timer);
          pending.delete(disposable[0]);
        }
        if (previous) {
          previous.urls.add(url);
          return;
        }
        setPending(requestId, {
          url,
          urls: new Set([url]),
          method: details.method,
          headers,
          tabId: details.tabId,
        });
      },
      filter,
      ['requestHeaders'],
    );
    webRequest.onHeadersReceived.addListener(
      (details): undefined => {
        if (details.type !== 'main_frame' && details.type !== 'sub_frame') return;
        const entry = pending.get(details.requestId);
        if (!entry || entry.handoff || details.statusCode < 200 || details.statusCode >= 300) return;
        entry.urls.add(details.url);
        entry.handoff = captureResponse(entry, details);
        for (const [id, tracked] of trackedDownloads) {
          if (entry.urls.has(tracked.snapshot.url)) void processDownload(id, tracked);
        }
      },
      filter,
      ['responseHeaders'],
    );
    const cleanup = (details: { requestId: string }): void => dropPending(details.requestId);
    webRequest.onCompleted.addListener(cleanup, filter);
    webRequest.onErrorOccurred.addListener(cleanup, filter);
  } catch (error) {
    console.warn('Raijin response capture unavailable', {
      errorName: error instanceof Error ? error.name : 'UnknownError',
    });
  }
}

async function captureResponse(
  entry: PendingEntry,
  response: { url: string; type: string; statusCode: number; responseHeaders?: Array<{ name: string; value?: string }> },
): Promise<boolean | null> {
  try {
    const config = await getAll();
    const metadata = responseMetadata(response.url, response.statusCode, response.responseHeaders ?? []);
    const page = tabUrls.get(entry.tabId) ?? null;
    const extension = getFileExtension(metadata.suggestedName ?? '');
    const nameIsSupported = config.registeredFileTypes.some((type) => type.toLowerCase() === extension);
    const decision = captureDecision({
      url: entry.url,
      downloadPage: page,
      suggestedName: metadata.fromHeader || nameIsSupported ? metadata.suggestedName : null,
      fileSizeBytes: metadata.totalBytes,
      contentType: metadata.contentType,
      method: entry.method,
      frameType: response.type,
      statusCode: response.statusCode,
    }, config, bypassPressed);
    console.debug('Raijin response capture decision', { decision, hasName: Boolean(metadata.suggestedName), hasSize: metadata.totalBytes !== null });
    if (isBlacklisted(response.url, config.blacklistedUrls) || decision === 'ignore') return false;
    if (decision === 'pending') return null;
    const headers = config.sendHeaders
      ? { ...await getHeadersForUrl(entry.url), ...entry.headers }
      : null;
    if (bypassPressed || entry.browserFallback) return false;
    const result = await submitItems([{
      link: entry.url,
      downloadPage: page,
      headers,
      description: null,
      suggestedName: metadata.suggestedName,
      totalBytes: metadata.totalBytes,
      type: 'http',
    }], () => !entry.browserFallback);
    if (!result.ok) console.warn('Raijin response handoff failed', { reason: result.reason });
    return result.ok;
  } catch (error) {
    console.error('Raijin response handoff failed', {
      errorName: error instanceof Error ? error.name : 'UnknownError',
    });
    return false;
  }
}

type DownloadSnapshot = Parameters<Parameters<typeof browser.downloads.onCreated.addListener>[0]>[0];

interface TrackedDownload {
  snapshot: DownloadSnapshot;
  running: boolean;
  dirty: boolean;
  done: boolean;
  timer: ReturnType<typeof setTimeout>;
  releaseFilename?: () => void;
  browserFallback?: boolean;
}

const trackedDownloads = new Map<number, TrackedDownload>();

function forgetDownload(id: number): void {
  const entry = trackedDownloads.get(id);
  if (entry) {
    clearTimeout(entry.timer);
    entry.done = true;
    entry.releaseFilename?.();
  }
  trackedDownloads.delete(id);
}

function downloadAgeMs(startTime: string): number | null {
  const parsed = Date.parse(startTime);
  return Number.isNaN(parsed) ? null : Date.now() - parsed;
}

async function evaluateDownload(d: DownloadSnapshot): Promise<'pending' | 'done'> {
  const config = await getAll();
  const tracked = trackedDownloads.get(d.id);
  if (!tracked || tracked.browserFallback) return 'done';
  if (tracked.snapshot !== d) return 'pending';
  const url = d.url;
  if (!url.startsWith('http://') && !url.startsWith('https://')) return 'done';
  if (d.byExtensionId || d.state !== 'in_progress' || d.endTime) return 'done';

  const intercepted = findPendingByUrl(url, d.id);
  if (intercepted?.handoff) {
    const accepted = await intercepted.handoff;
    if (accepted !== null) {
      if (accepted) await completeHandoff(d.id, intercepted);
      return 'done';
    }
  }
  if (!config.autoCaptureLinks || bypassPressed) return 'done';
  if (intercepted && intercepted.method.toUpperCase() !== 'GET') return 'done';

  const downloadPage = d.referrer || null;
  const fileSizeBytes = d.fileSize >= 0 ? d.fileSize : d.totalBytes >= 0 ? d.totalBytes : null;
  if (d.finalUrl && isBlacklisted(d.finalUrl, config.blacklistedUrls)) return 'done';
  const suggestedName = d.filename.split(/[\\/]/).pop()?.trim() || null;
  const decision = captureDecision(
    {
      url,
      downloadPage,
      fileSizeBytes,
      contentType: d.mime || null,
      suggestedName,
      method: intercepted?.method ?? 'GET',
    },
    config,
    bypassPressed,
  );
  console.debug('Raijin auto-capture decision', { downloadId: d.id, decision });
  if (decision === 'pending') return 'pending';
  if (decision === 'ignore') return 'done';
  const headers = config.sendHeaders
    ? intercepted && Object.keys(intercepted.headers).length > 0
      ? { ...intercepted.headers }
      : await getHeadersForUrl(url)
    : null;
  const entry = trackedDownloads.get(d.id);
  if (!entry || entry.browserFallback) return 'done';
  if (entry.snapshot !== d) return 'pending';
  if (intercepted?.handoff) {
    const accepted = await intercepted.handoff;
    if (accepted !== null) {
      if (accepted) await completeHandoff(d.id, intercepted);
      return 'done';
    }
  }
  if (bypassPressed) return 'done';
  const submission = submitItems([{
    link: url,
    downloadPage,
    headers,
    description: null,
    suggestedName,
    totalBytes: fileSizeBytes,
    type: 'http',
  }], () => !entry.browserFallback && trackedDownloads.get(d.id) === entry);
  if (intercepted) intercepted.handoff = submission.then((result) => result.ok);
  const result = await submission;
  if (!result.ok) {
    console.warn('Raijin auto-capture submission failed', { downloadId: d.id, reason: result.reason });
    return 'done';
  }
  await completeHandoff(d.id, intercepted);
  return 'done';
}

async function completeHandoff(id: number, intercepted?: PendingEntry): Promise<void> {
  const entry = trackedDownloads.get(id);
  if (!entry || entry.browserFallback) return;
  if (await cancelDownload(id)) {
    if (intercepted && intercepted.tabId >= 0) await closeNewTabIfCaptured(intercepted.tabId);
  }
}

async function processDownload(id: number, entry: TrackedDownload): Promise<void> {
  entry.dirty = true;
  if (entry.running || entry.done) return;
  entry.running = true;
  try {
    while (entry.dirty && !entry.done && trackedDownloads.get(id) === entry) {
      entry.dirty = false;
      const snapshot = entry.snapshot;
      const result = await evaluateDownload(snapshot);
      entry.done = entry.done || result === 'done';
    }
  } catch (error) {
    entry.done = true;
    console.error('Raijin auto-capture processing failed', {
      downloadId: id,
      errorName: error instanceof Error ? error.name : 'UnknownError',
    });
  } finally {
    entry.running = false;
    entry.releaseFilename?.();
  }
}

function trackDownload(snapshot: DownloadSnapshot): void {
  if (trackedDownloads.has(snapshot.id)) return;
  const age = downloadAgeMs(snapshot.startTime);
  if (age !== null && age > DOWNLOAD_CREATED_MAX_AGE_MS) return;
  if (trackedDownloads.size >= MAX_PENDING_DOWNLOADS) return;
  const entry: TrackedDownload = {
    snapshot,
    running: false,
    dirty: false,
    done: false,
    timer: setTimeout(() => forgetDownload(snapshot.id), DOWNLOAD_METADATA_WAIT_MS),
  };
  trackedDownloads.set(snapshot.id, entry);
  void processDownload(snapshot.id, entry);
}

function handleContextMenu(): void {
  try {
    browser.contextMenus.onClicked.addListener((info) => {
      void (async () => {
        if (info.menuItemId === MENU_LINK_ID) {
          const link = (info.linkUrl || info.srcUrl || '').trim();
          if (!link) return;
          const headers = await getHeadersForUrl(link);
          const linkText = 'linkText' in info ? info.linkText : undefined;
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

function handleFilenameGate(): void {
  browser.downloads.onDeterminingFilename?.addListener((snapshot, suggest) => {
    const age = downloadAgeMs(snapshot.startTime);
    if (snapshot.byExtensionId || snapshot.state !== 'in_progress' || snapshot.endTime
      || !/^https?:\/\//.test(snapshot.url)
      || (age !== null && age > DOWNLOAD_CREATED_MAX_AGE_MS)) {
      suggest();
      return false;
    }
    if (!trackedDownloads.has(snapshot.id)) trackDownload(snapshot);
    const entry = trackedDownloads.get(snapshot.id);
    if (!entry || entry.done || entry.releaseFilename) {
      suggest();
      return false;
    }
    const release = () => {
      if (entry.releaseFilename !== release) return;
      entry.releaseFilename = undefined;
      entry.done = true;
      entry.browserFallback = true;
      const intercepted = findPendingByUrl(snapshot.url, snapshot.id);
      if (intercepted) intercepted.browserFallback = true;
      clearTimeout(timer);
      console.debug('Raijin filename gate released', { downloadId: snapshot.id });
      try {
        suggest();
      } catch (error) {
        console.warn('Raijin filename gate release failed', {
          downloadId: snapshot.id,
          errorName: error instanceof Error ? error.name : 'UnknownError',
        });
      }
    };
    const timer = setTimeout(() => {
      console.warn('Raijin filename gate timed out', { downloadId: snapshot.id });
      release();
    }, ADD_TIMEOUT_MS + 1000);
    entry.releaseFilename = release;
    console.debug('Raijin filename gate held', { downloadId: snapshot.id });
    entry.snapshot = snapshot;
    void processDownload(snapshot.id, entry);
    return true;
  });
}

function handleDownloads(): void {
  try {
    handleFilenameGate();
  } catch (error) {
    console.warn('Raijin filename gate unavailable', {
      errorName: error instanceof Error ? error.name : 'UnknownError',
    });
  }
  browser.downloads.onCreated.addListener(trackDownload);
  browser.downloads.onChanged.addListener((delta) => {
    const entry = trackedDownloads.get(delta.id);
    if (!entry) return;
    if (delta.state?.current === 'complete' || delta.state?.current === 'interrupted') {
      forgetDownload(delta.id);
      return;
    }
    if (entry.done) return;
    const snapshot = entry.snapshot;
    entry.snapshot = {
      ...snapshot,
      filename: delta.filename?.current ?? snapshot.filename,
      mime: delta.mime?.current ?? snapshot.mime,
      fileSize: delta.fileSize?.current ?? snapshot.fileSize,
      totalBytes: delta.totalBytes?.current ?? snapshot.totalBytes,
      url: delta.url?.current ?? snapshot.url,
      finalUrl: delta.finalUrl?.current ?? snapshot.finalUrl,
    };
    void processDownload(delta.id, entry);
  });
  browser.downloads.onErased.addListener(forgetDownload);
}

function boot(): void {
  void ensureContextMenus();
  trackTabs();
  observeWebRequests();
  handleContextMenu();
  handleDownloads();
  handleInBackground({
    addDownload: async (items) => addItems(await withHeaders(items)),
    addMedia: async (item) => {
      const [prepared] = await withHeaders([item]);
      if (!prepared) return { ok: false, reason: 'error' };
      return submitItems([prepared]);
    },
    testPort: (port, token) => testPort(port, token),
    getHeaders: async (urls) => Promise.all(urls.map((u) => getHeadersForUrl(u))),
    setBypass: (pressed) => {
      bypassPressed = pressed;
    },
  });
}

export default defineBackground({
  main() {
    boot();
  },
});
