import { ADD_TIMEOUT_MS, PING_TIMEOUT_MS } from './constants';
import type { AddDownloadRequest } from './types';

export class NetworkError extends Error {
  constructor() {
    super('Cannot reach the Raijin desktop app');
    this.name = 'NetworkError';
  }
}

export class UnauthorizedError extends Error {
  constructor() {
    super('Invalid or missing token');
    this.name = 'UnauthorizedError';
  }
}

export class ApiError extends Error {
  readonly status: number;
  constructor(status: number) {
    super(`Raijin app returned status ${status}`);
    this.name = 'ApiError';
    this.status = status;
  }
}

function isAbortOrNetwork(err: unknown): boolean {
  if (err instanceof DOMException && err.name === 'AbortError') return true;
  if (err instanceof TypeError) return true;
  return false;
}

async function fetchWithTimeout(
  url: string,
  init: RequestInit,
  timeoutMs: number,
): Promise<Response> {
  const controller = new AbortController();
  const timer = setTimeout(() => controller.abort(), timeoutMs);
  try {
    return await fetch(url, { ...init, signal: controller.signal });
  } catch (err) {
    if (isAbortOrNetwork(err) || controller.signal.aborted) throw new NetworkError();
    throw err;
  } finally {
    clearTimeout(timer);
  }
}

// Thin client for the Raijin desktop integration API. Only talks to localhost.
export class BackendApi {
  private readonly baseUrl: string;
  private readonly token: string;

  constructor(port: number, token = '') {
    this.baseUrl = `http://127.0.0.1:${port}`;
    this.token = token;
  }

  private authHeaders(): Record<string, string> {
    const headers: Record<string, string> = { 'Content-Type': 'application/json' };
    if (this.token) headers['Authorization'] = `Bearer ${this.token}`;
    return headers;
  }

  // Health check, intentionally unauthenticated.
  async ping(): Promise<boolean> {
    const res = await fetchWithTimeout(
      `${this.baseUrl}/ping`,
      { method: 'POST', headers: { 'Content-Type': 'application/json' }, body: 'null' },
      PING_TIMEOUT_MS,
    );
    if (!res.ok) throw new ApiError(res.status);
    const body = await res.text();
    if (!body.trim()) return false;
    const status: unknown = JSON.parse(body);
    return typeof status === 'object' && status !== null
      && 'basicAccess' in status && status.basicAccess === true;
  }

  async add(request: AddDownloadRequest): Promise<void> {
    await this.postAuthenticated('/add', request);
  }

  async startHeadless(request: AddDownloadRequest): Promise<void> {
    await this.postAuthenticated('/start-headless-download', request);
  }

  // Lightweight authenticated probe used to tell "server down" apart from
  // "token rejected" without triggering a download side effect.
  async probeAuth(): Promise<void> {
    const res = await fetchWithTimeout(
      `${this.baseUrl}/downloads`,
      { method: 'GET', headers: this.authHeaders() },
      PING_TIMEOUT_MS,
    );
    if (res.status === 401) throw new UnauthorizedError();
    if (!res.ok) throw new ApiError(res.status);
  }

  private async postAuthenticated(path: string, body: unknown): Promise<void> {
    const res = await fetchWithTimeout(
      `${this.baseUrl}${path}`,
      { method: 'POST', headers: this.authHeaders(), body: JSON.stringify(body) },
      ADD_TIMEOUT_MS,
    );
    if (res.status === 401) throw new UnauthorizedError();
    if (!res.ok) throw new ApiError(res.status);
  }
}
