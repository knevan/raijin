import { storage } from 'wxt/utils/storage';
import {
  DEFAULT_BYPASS_KEY,
  DEFAULT_PORT,
  MAX_ALLOWED_PORT,
  MIN_ALLOWED_PORT,
} from './constants';
import type { RaijinConfig } from './types';

export const defaultConfig: RaijinConfig = {
  port: DEFAULT_PORT,
  token: '',
  autoCaptureLinks: true,
  popupEnabled: true,
  mediaOverlayEnabled: true,
  sendHeaders: true,
  registeredFileTypes: [
    'zip', 'rar', '7z', 'iso', 'tar', 'gz',
    'exe', 'msi', 'deb', 'jar', 'apk', 'bin',
    'mp3', 'aac',
    'pdf',
    'mp4', '3gp', 'avi', 'mkv', 'wav', 'mpeg',
    'srt',
  ],
  blacklistedUrls: [],
  allowPassDownloadIfAppNotRespond: true,
  closeNewTabIfItWasCaptured: true,
  silentAddDownload: false,
  silentStartDownload: false,
  captureFileSizeMinimumKb: 0,
  bypassShortcut: DEFAULT_BYPASS_KEY,
};

const portItem = storage.defineItem<number>('local:port', { fallback: defaultConfig.port });
const tokenItem = storage.defineItem<string>('local:token', { fallback: defaultConfig.token });
const autoCaptureLinksItem = storage.defineItem<boolean>('local:autoCaptureLinks', {
  fallback: defaultConfig.autoCaptureLinks,
});
const popupEnabledItem = storage.defineItem<boolean>('local:popupEnabled', {
  fallback: defaultConfig.popupEnabled,
});
const mediaOverlayItem = storage.defineItem<boolean>('local:mediaOverlayEnabled', {
  fallback: defaultConfig.mediaOverlayEnabled,
});
const sendHeadersItem = storage.defineItem<boolean>('local:sendHeaders', {
  fallback: defaultConfig.sendHeaders,
});
const registeredFileTypesItem = storage.defineItem<string[]>('local:registeredFileTypes', {
  fallback: defaultConfig.registeredFileTypes,
});
const blacklistedUrlsItem = storage.defineItem<string[]>('local:blacklistedUrls', {
  fallback: defaultConfig.blacklistedUrls,
});
const allowPassItem = storage.defineItem<boolean>('local:allowPassDownloadIfAppNotRespond', {
  fallback: defaultConfig.allowPassDownloadIfAppNotRespond,
});
const closeNewTabItem = storage.defineItem<boolean>('local:closeNewTabIfItWasCaptured', {
  fallback: defaultConfig.closeNewTabIfItWasCaptured,
});
const silentAddItem = storage.defineItem<boolean>('local:silentAddDownload', {
  fallback: defaultConfig.silentAddDownload,
});
const silentStartItem = storage.defineItem<boolean>('local:silentStartDownload', {
  fallback: defaultConfig.silentStartDownload,
});
const minSizeItem = storage.defineItem<number>('local:captureFileSizeMinimumKb', {
  fallback: defaultConfig.captureFileSizeMinimumKb,
});
const bypassItem = storage.defineItem<string>('local:bypassShortcut', {
  fallback: defaultConfig.bypassShortcut,
});

export function isValidPort(port: number): boolean {
  return Number.isInteger(port) && port >= MIN_ALLOWED_PORT && port <= MAX_ALLOWED_PORT;
}

// Clamp user input into the allowed range; fall back to default on NaN.
export function normalizePort(port: number): number {
  if (!Number.isFinite(port)) return DEFAULT_PORT;
  const floored = Math.floor(port);
  if (floored < MIN_ALLOWED_PORT) return MIN_ALLOWED_PORT;
  if (floored > MAX_ALLOWED_PORT) return MAX_ALLOWED_PORT;
  return floored;
}

export async function getAll(): Promise<RaijinConfig> {
  const [
    port,
    token,
    autoCaptureLinks,
    popupEnabled,
    mediaOverlayEnabled,
    sendHeaders,
    registeredFileTypes,
    blacklistedUrls,
    allowPassDownloadIfAppNotRespond,
    closeNewTabIfItWasCaptured,
    silentAddDownload,
    silentStartDownload,
    captureFileSizeMinimumKb,
    bypassShortcut,
  ] = await Promise.all([
    portItem.getValue(),
    tokenItem.getValue(),
    autoCaptureLinksItem.getValue(),
    popupEnabledItem.getValue(),
    mediaOverlayItem.getValue(),
    sendHeadersItem.getValue(),
    registeredFileTypesItem.getValue(),
    blacklistedUrlsItem.getValue(),
    allowPassItem.getValue(),
    closeNewTabItem.getValue(),
    silentAddItem.getValue(),
    silentStartItem.getValue(),
    minSizeItem.getValue(),
    bypassItem.getValue(),
  ]);
  return {
    port: normalizePort(port),
    token: token ?? '',
    autoCaptureLinks,
    popupEnabled,
    mediaOverlayEnabled,
    sendHeaders,
    registeredFileTypes,
    blacklistedUrls,
    allowPassDownloadIfAppNotRespond,
    closeNewTabIfItWasCaptured,
    silentAddDownload,
    silentStartDownload,
    captureFileSizeMinimumKb: Math.max(0, Math.floor(captureFileSizeMinimumKb)),
    bypassShortcut: bypassShortcut || DEFAULT_BYPASS_KEY,
  };
}

export async function setAll(partial: Partial<RaijinConfig>): Promise<void> {
  const tasks: Promise<void>[] = [];
  if (partial.port !== undefined) tasks.push(portItem.setValue(normalizePort(partial.port)));
  if (partial.token !== undefined) tasks.push(tokenItem.setValue(partial.token));
  if (partial.autoCaptureLinks !== undefined)
    tasks.push(autoCaptureLinksItem.setValue(partial.autoCaptureLinks));
  if (partial.popupEnabled !== undefined) tasks.push(popupEnabledItem.setValue(partial.popupEnabled));
  if (partial.mediaOverlayEnabled !== undefined)
    tasks.push(mediaOverlayItem.setValue(partial.mediaOverlayEnabled));
  if (partial.sendHeaders !== undefined) tasks.push(sendHeadersItem.setValue(partial.sendHeaders));
  if (partial.registeredFileTypes !== undefined)
    tasks.push(registeredFileTypesItem.setValue(partial.registeredFileTypes));
  if (partial.blacklistedUrls !== undefined)
    tasks.push(blacklistedUrlsItem.setValue(partial.blacklistedUrls));
  if (partial.allowPassDownloadIfAppNotRespond !== undefined)
    tasks.push(allowPassItem.setValue(partial.allowPassDownloadIfAppNotRespond));
  if (partial.closeNewTabIfItWasCaptured !== undefined)
    tasks.push(closeNewTabItem.setValue(partial.closeNewTabIfItWasCaptured));
  if (partial.silentAddDownload !== undefined)
    tasks.push(silentAddItem.setValue(partial.silentAddDownload));
  if (partial.silentStartDownload !== undefined)
    tasks.push(silentStartItem.setValue(partial.silentStartDownload));
  if (partial.captureFileSizeMinimumKb !== undefined)
    tasks.push(minSizeItem.setValue(Math.max(0, Math.floor(partial.captureFileSizeMinimumKb))));
  if (partial.bypassShortcut !== undefined)
    tasks.push(bypassItem.setValue(partial.bypassShortcut || DEFAULT_BYPASS_KEY));
  await Promise.all(tasks);
}

export async function setConfigItem<K extends keyof RaijinConfig>(
  key: K,
  value: RaijinConfig[K],
): Promise<void> {
  await setAll({ [key]: value } as Partial<RaijinConfig>);
}

export const configItems = {
  portItem,
  tokenItem,
  autoCaptureLinksItem,
  popupEnabledItem,
  mediaOverlayItem,
  sendHeadersItem,
  registeredFileTypesItem,
  blacklistedUrlsItem,
  allowPassItem,
  closeNewTabItem,
  silentAddItem,
  silentStartItem,
  minSizeItem,
  bypassItem,
};
