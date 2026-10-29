import type { RaijinConfig } from './types';

export interface CaptureCandidate {
  url: string;
  downloadPage: string | null;
  fileSizeBytes: number | null;
  contentType: string | null;
  suggestedName: string | null;
  method: string;
  frameType?: string;
  statusCode?: number;
}

export function getFileExtension(name: string): string {
  const base = name.split(/[?#]/)[0] ?? name;
  const trimmed = base.trim();
  const slash = Math.max(trimmed.lastIndexOf('/'), trimmed.lastIndexOf('\\'));
  const file = slash >= 0 ? trimmed.slice(slash + 1) : trimmed;
  const dot = file.lastIndexOf('.');
  if (dot <= 0 || dot === file.length - 1) return '';
  return file.slice(dot + 1).toLowerCase();
}

export function getFileFromUrl(url: string): string | null {
  try {
    const pathname = new URL(url, 'http://127.0.0.1').pathname;
    const last = pathname.split('/').pop() ?? '';
    const decoded = safeDecode(last).trim();
    return decoded ? decoded : null;
  } catch {
    return null;
  }
}

function safeDecode(value: string): string {
  try {
    return decodeURIComponent(value);
  } catch {
    return value;
  }
}

// Minimal `*` wildcard matcher (no external dep). `*` spans any characters.
export function matchWildcard(pattern: string, value: string): boolean {
  const trimmed = pattern.trim();
  if (!trimmed) return false;
  if (trimmed === '*') return true;
  const escaped = trimmed
    .split('*')
    .map((part) => part.replace(/[.+?^${}()|[\]\\]/g, '\\$&'));
  try {
    return new RegExp(`^${escaped.join('.*')}$`).test(value);
  } catch {
    return false;
  }
}

export function isBlacklisted(url: string, patterns: string[]): boolean {
  for (const pattern of patterns) {
    if (pattern && matchWildcard(pattern, url)) return true;
  }
  return false;
}

function isSuccessStatus(statusCode: number): boolean {
  return statusCode >= 200 && statusCode <= 299;
}

// Pure auto-capture decision shared by the background interceptor.
export function captureDecision(
  candidate: CaptureCandidate,
  config: RaijinConfig,
  bypassPressed: boolean,
): 'capture' | 'pending' | 'ignore' {
  if (!config.autoCaptureLinks) return 'ignore';
  if (bypassPressed) return 'ignore';
  if (candidate.method.toUpperCase() !== 'GET') return 'ignore';
  if (
    candidate.frameType !== undefined &&
    candidate.frameType !== 'main_frame' &&
    candidate.frameType !== 'sub_frame'
  ) {
    return 'ignore';
  }
  if (candidate.statusCode !== undefined && !isSuccessStatus(candidate.statusCode)) return 'ignore';

  let url: URL;
  try {
    url = new URL(candidate.url);
  } catch {
    return 'ignore';
  }
  if (url.protocol !== 'http:' && url.protocol !== 'https:') return 'ignore';

  const contentType = candidate.contentType?.toLowerCase() ?? null;
  if (contentType?.startsWith('text/')) return 'ignore';

  if (isBlacklisted(candidate.url, config.blacklistedUrls)) return 'ignore';
  if (candidate.downloadPage && isBlacklisted(candidate.downloadPage, config.blacklistedUrls)) {
    return 'ignore';
  }

  if (
    candidate.fileSizeBytes !== null &&
    config.captureFileSizeMinimumKb > 0 &&
    candidate.fileSizeBytes < config.captureFileSizeMinimumKb * 1024
  ) {
    return 'ignore';
  }

  const fileName = candidate.suggestedName?.trim();
  if (!fileName) return 'pending';
  const ext = getFileExtension(fileName);
  if (!ext) return 'pending';
  if (!config.registeredFileTypes.some((type) => type.toLowerCase() === ext)) return 'ignore';
  if (candidate.fileSizeBytes === null && config.captureFileSizeMinimumKb > 0) return 'pending';
  return 'capture';
}

// Resolve raw hrefs against a base URL, keep http(s) only, dedupe.
export function normalizeAndFilterLinks(rawHrefs: Array<string | null | undefined>, baseUrl: string): string[] {
  const seen = new Set<string>();
  const out: string[] = [];
  for (const raw of rawHrefs) {
    if (!raw) continue;
    const href = raw.trim();
    if (!href || href.startsWith('#') || href.toLowerCase().startsWith('javascript:')) continue;
    let absolute: string;
    try {
      absolute = new URL(href, baseUrl).toString();
    } catch {
      continue;
    }
    if (!absolute.startsWith('http://') && !absolute.startsWith('https://')) continue;
    if (seen.has(absolute)) continue;
    seen.add(absolute);
    out.push(absolute);
  }
  return out;
}
