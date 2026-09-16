export const DEFAULT_PORT = 35343;
export const MIN_ALLOWED_PORT = 1024;
export const MAX_ALLOWED_PORT = 65535;
export const PING_TIMEOUT_MS = 2000;
export const ADD_TIMEOUT_MS = 8000;
export const DEFAULT_BYPASS_KEY = 'Delete';

// How long a downloads.onCreated item may lag behind its network request.
export const DOWNLOAD_CREATED_MAX_AGE_MS = 10_000;

// Lifetime of cached webRequest entries before they are dropped.
export const PENDING_REQUEST_TTL_MS = 20_000;
