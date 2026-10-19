import { browser } from 'wxt/browser';
import type {
  AddMediaResult,
  BackgroundMessage,
  ContentMessage,
  DownloadRequestItem,
} from './types';
import type { TestPortResult } from './types';

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === 'object' && value !== null;
}

export function isBackgroundMessage(value: unknown): value is BackgroundMessage {
  if (!isRecord(value) || typeof value['type'] !== 'string') return false;
  const type = value['type'] as string;
  return (
    type === 'raijin:add_download' ||
    type === 'raijin:media_add' ||
    type === 'raijin:test_port' ||
    type === 'raijin:get_headers' ||
    type === 'raijin:set_bypass'
  );
}

export async function sendAddDownload(items: DownloadRequestItem[]): Promise<boolean> {
  return (await browser.runtime.sendMessage({
    type: 'raijin:add_download',
    items,
  })) as boolean;
}

// Unlike sendAddDownload, this reports *why* a send failed so the overlay can
// tell "server down" apart from "token rejected".
export async function sendAddMedia(item: DownloadRequestItem): Promise<AddMediaResult> {
  const res = (await browser.runtime.sendMessage({
    type: 'raijin:media_add',
    item,
  })) as AddMediaResult | undefined;
  if (res && typeof res === 'object' && typeof res.ok === 'boolean') return res;
  return { ok: false, reason: 'error' };
}

export async function sendTestPort(port?: number, token?: string): Promise<TestPortResult> {
  const res = (await browser.runtime.sendMessage({
    type: 'raijin:test_port',
    port,
    token,
  })) as TestPortResult | boolean;
  // Accept legacy boolean replies (ping only) for forward compatibility.
  if (typeof res === 'boolean') return res ? { ok: true, reason: 'ok' } : { ok: false, reason: 'down' };
  return res;
}

export async function sendGetHeaders(urls: string[]): Promise<Array<Record<string, string> | null>> {
  return (await browser.runtime.sendMessage({
    type: 'raijin:get_headers',
    urls,
  })) as Array<Record<string, string> | null>;
}

export async function sendSetBypass(pressed: boolean): Promise<void> {
  await browser.runtime.sendMessage({ type: 'raijin:set_bypass', pressed });
}

export async function sendCheckSelectedToTab(tabId: number): Promise<void> {
  const msg: ContentMessage = { type: 'raijin:check_selected_text_for_links' };
  await browser.tabs.sendMessage(tabId, msg);
}

export interface BackgroundHandlers {
  addDownload: (items: DownloadRequestItem[]) => Promise<boolean>;
  addMedia: (item: DownloadRequestItem) => Promise<AddMediaResult>;
  testPort: (port?: number, token?: string) => Promise<TestPortResult>;
  getHeaders: (urls: string[]) => Promise<Array<Record<string, string> | null>>;
  setBypass: (pressed: boolean) => void;
}

// Single runtime.onMessage dispatcher owned by the background.
export function handleInBackground(handlers: BackgroundHandlers): () => void {
  const listener = (
    message: unknown,
  ):
    | boolean
    | Promise<
        | boolean
        | AddMediaResult
        | TestPortResult
        | Array<Record<string, string> | null>
        | undefined
      > => {
    if (!isRecord(message) || typeof message['type'] !== 'string') return false;
    switch (message['type']) {
      case 'raijin:add_download':
        return handlers.addDownload(
          Array.isArray(message['items']) ? (message['items'] as DownloadRequestItem[]) : [],
        );
      case 'raijin:media_add':
        return handlers.addMedia(message['item'] as DownloadRequestItem);
      case 'raijin:test_port':
        return handlers.testPort(
          typeof message['port'] === 'number' ? (message['port'] as number) : undefined,
          typeof message['token'] === 'string' ? (message['token'] as string) : undefined,
        );
      case 'raijin:get_headers':
        return handlers.getHeaders(
          Array.isArray(message['urls']) ? (message['urls'] as string[]) : [],
        );
      case 'raijin:set_bypass':
        handlers.setBypass(message['pressed'] === true);
        return Promise.resolve(undefined);
      default:
        return false;
    }
  };
  browser.runtime.onMessage.addListener(listener);
  return () => {
    try {
      browser.runtime.onMessage.removeListener(listener);
    } catch {
      // Best effort on shutdown.
    }
  };
}
