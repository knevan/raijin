export interface DownloadRequestItem {
  link: string;
  downloadPage: string | null;
  headers: Record<string, string> | null;
  description: string | null;
  suggestedName: string | null;
  type: 'http' | 'hls';
}

export interface DownloadRequestOptions {
  silentAdd: boolean;
  silentStart: boolean;
}

export interface AddDownloadRequest {
  items: DownloadRequestItem[];
  options: DownloadRequestOptions;
}

export interface RaijinConfig {
  port: number;
  token: string;
  autoCaptureLinks: boolean;
  popupEnabled: boolean;
  sendHeaders: boolean;
  registeredFileTypes: string[];
  blacklistedUrls: string[];
  allowPassDownloadIfAppNotRespond: boolean;
  closeNewTabIfItWasCaptured: boolean;
  silentAddDownload: boolean;
  silentStartDownload: boolean;
  captureFileSizeMinimumKb: number;
  bypassShortcut: string;
}

export type ConnectionStatus = 'unknown' | 'ok' | 'down' | 'unauthorized';

export type TestPortReason = 'ok' | 'down' | 'unauthorized';

export interface TestPortResult {
  ok: boolean;
  reason: TestPortReason;
}

// Messaging envelopes carried over browser.runtime.
export type BackgroundMessage =
  | { type: 'raijin:add_download'; items: DownloadRequestItem[] }
  | { type: 'raijin:test_port'; port?: number; token?: string }
  | { type: 'raijin:get_headers'; urls: string[] }
  | { type: 'raijin:set_bypass'; pressed: boolean };

export type ContentMessage = { type: 'raijin:check_selected_text_for_links' };

export type ExtensionMessage = BackgroundMessage | ContentMessage;
