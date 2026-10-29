import test from 'node:test';
import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import { runInNewContext } from 'node:vm';
import ts from 'typescript';

function load(source, modules = {}, globals = {}) {
  const { outputText } = ts.transpileModule(source, {
    compilerOptions: { target: ts.ScriptTarget.ES2022, module: ts.ModuleKind.CommonJS },
  });
  const exports = {};
  runInNewContext(outputText, {
    exports, URL, Date, Error,
    require: (id) => {
      assert.ok(Object.hasOwn(modules, id), `unexpected import: ${id}`);
      return modules[id];
    },
    ...globals,
  });
  return exports;
}

const capture = load(await readFile(new URL('./capture.ts', import.meta.url), 'utf8'));
const constants = load(await readFile(new URL('./constants.ts', import.meta.url), 'utf8'));
const configSource = await readFile(new URL('./config.ts', import.meta.url), 'utf8');
const { defaultConfig } = load(configSource.slice(
  configSource.indexOf('export const defaultConfig'), configSource.indexOf('const portItem'),
), {}, { DEFAULT_PORT: 35343, DEFAULT_BYPASS_KEY: 'Delete' });
const backgroundSource = await readFile(new URL('../entrypoints/background.ts', import.meta.url), 'utf8');
const settle = () => new Promise((resolve) => setImmediate(resolve));
const candidate = (changes = {}) => ({
  url: 'https://code.visualstudio.com/sha/download?build=insider&os=win32-x64-user',
  downloadPage: null, fileSizeBytes: null, contentType: 'application/octet-stream',
  suggestedName: null, method: 'GET', ...changes,
});

test('capture waits for a filename and unknown minimum size instead of rejecting prematurely', () => {
  assert.equal(capture.captureDecision(candidate(), defaultConfig, false), 'pending');
  assert.equal(capture.captureDecision(candidate({ suggestedName: 'download' }), defaultConfig, false), 'pending');
  assert.equal(capture.captureDecision(candidate({ suggestedName: 'setup.exe' }), defaultConfig, false), 'capture');
  assert.equal(capture.captureDecision(candidate({ suggestedName: 'setup.exe' }), {
    ...defaultConfig, captureFileSizeMinimumKb: 100,
  }, false), 'pending');
});

test('capture preserves definitive policy rejections', () => {
  for (const [changes, config, bypass] of [
    [{ method: 'POST' }, {}, false],
    [{ contentType: 'text/html' }, {}, false],
    [{ suggestedName: 'file.html' }, {}, false],
    [{ url: 'blob:https://example.com/id' }, {}, false],
    [{ fileSizeBytes: 100 }, { captureFileSizeMinimumKb: 1 }, false],
    [{}, { autoCaptureLinks: false }, false],
    [{}, { blacklistedUrls: ['https://code.visualstudio.com/*'] }, false],
    [{ downloadPage: 'https://blocked.test/' }, { blacklistedUrls: ['https://blocked.test/*'] }, false],
    [{}, {}, true],
  ]) {
    assert.equal(capture.captureDecision(candidate(changes), { ...defaultConfig, ...config }, bypass), 'ignore');
  }
});

function event() {
  const listeners = [];
  return {
    addListener: (listener) => listeners.push(listener),
    emit: (...args) => listeners.forEach((listener) => listener(...args)),
    listeners,
  };
}

function background() {
  const config = { ...defaultConfig, registeredFileTypes: [...defaultConfig.registeredFileTypes] };
  const created = event();
  const changed = event();
  const erased = event();
  const calls = [];
  const logs = [];
  const timers = new Map();
  let timerId = 0;
  let submit = async () => {};
  let readConfig = async () => ({ ...config });
  let handlers;
  class NetworkError extends Error {}
  class UnauthorizedError extends Error {}
  class ApiError extends Error {}
  class BackendApi {
    async add(request) {
      calls.push(['add', JSON.parse(JSON.stringify(request))]);
      await submit();
    }
  }
  const browser = {
    contextMenus: { removeAll: () => new Promise(() => {}), onClicked: event() },
    tabs: { onCreated: event(), onUpdated: event(), onRemoved: event() },
    downloads: {
      onCreated: created, onChanged: changed, onErased: erased,
      cancel: async (id) => { calls.push(['cancel', id]); },
      erase: async ({ id }) => { calls.push(['erase', id]); },
      removeFile: async (id) => { calls.push(['removeFile', id]); },
    },
    cookies: { getAll: async ({ url }) => { calls.push(['cookies', url]); return []; } },
  };
  const definition = load(backgroundSource, {
    'wxt/browser': { browser },
    'wxt/utils/define-background': { defineBackground: (definition) => definition },
    '@/utils/backend': { BackendApi, NetworkError, UnauthorizedError, ApiError },
    '@/utils/config': { getAll: () => readConfig() },
    '@/utils/constants': constants,
    '@/utils/capture': capture,
    '@/utils/messaging': { handleInBackground: (value) => { handlers = value; } },
  }, {
    console: Object.fromEntries(['debug', 'warn', 'error'].map((level) => [level, (...args) => logs.push([level, ...args])])),
    setTimeout: (callback, delay) => { timers.set(++timerId, { callback, delay }); return timerId; },
    clearTimeout: (id) => timers.delete(id),
  }).default;
  definition.main();
  const snapshot = (changes = {}) => ({
    id: 1, url: candidate().url, finalUrl: 'https://cdn.example.com/setup',
    filename: '', mime: 'application/octet-stream', fileSize: -1, totalBytes: -1,
    referrer: 'https://code.visualstudio.com/', startTime: new Date().toISOString(),
    state: 'in_progress', ...changes,
  });
  return {
    config, created, changed, erased, calls, logs, timers, snapshot,
    handlers: () => handlers,
    failSubmission: () => { submit = async () => { throw new NetworkError(); }; },
    setSubmit: (callback) => { submit = callback; },
    setReadConfig: (callback) => { readConfig = callback; },
    expire: () => { for (const timer of [...timers.values()]) timer.callback(); },
    additions: () => calls.filter(([kind]) => kind === 'add'),
  };
}

test('listeners register synchronously even while context menu initialization is pending', () => {
  const bg = background();
  assert.equal(bg.created.listeners.length, 1);
  assert.equal(bg.changed.listeners.length, 1);
  assert.ok(bg.handlers());
});

test('redirect download waits for browser filename, submits its basename once and then cancels', async () => {
  const bg = background();
  bg.created.emit(bg.snapshot());
  await settle();
  assert.equal(bg.additions().length, 0);
  bg.changed.emit({ id: 1, filename: { current: 'C:\\Users\\user\\Downloads\\VSCodeUserSetup-x64-1.145.0-insider.exe' } });
  await settle();
  const request = bg.additions()[0][1];
  assert.equal(request.items[0].suggestedName, 'VSCodeUserSetup-x64-1.145.0-insider.exe');
  assert.equal(request.items[0].link, candidate().url);
  assert.equal(request.options.silentAdd, false);
  assert.equal(request.options.silentStart, false);
  assert.equal(bg.calls.find(([kind]) => kind === 'cookies')[1], candidate().url);
  assert.deepEqual(bg.calls.filter(([kind]) => ['add', 'cancel', 'erase', 'removeFile'].includes(kind)).map(([kind]) => kind), ['add', 'cancel', 'erase', 'removeFile']);
  bg.changed.emit({ id: 1, filename: { current: 'setup.exe' } });
  bg.created.emit(bg.snapshot());
  await settle();
  assert.equal(bg.additions().length, 1);
  assert.ok(!JSON.stringify(bg.logs).includes('visualstudio'));
});

test('an empty filename waits even when the URL already has an extension', async () => {
  const bg = background();
  bg.created.emit(bg.snapshot({ url: 'https://example.com/file.exe' }));
  await settle();
  assert.equal(bg.additions().length, 0);
  bg.changed.emit({ id: 1, filename: { current: '/Downloads/actual-name.exe' } });
  await settle();
  assert.equal(bg.additions()[0][1].items[0].suggestedName, 'actual-name.exe');
});

test('an endpoint extension does not reject the download before its actual filename arrives', async () => {
  const bg = background();
  bg.created.emit(bg.snapshot({ url: 'https://example.com/download.php' }));
  await settle();
  assert.equal(bg.additions().length, 0);
  bg.changed.emit({ id: 1, filename: { current: 'installer.exe' } });
  await settle();
  assert.equal(bg.additions()[0][1].items[0].suggestedName, 'installer.exe');
});

test('a filename without an extension can become an executable on a later update', async () => {
  const bg = background();
  bg.created.emit(bg.snapshot({ filename: 'download' }));
  await settle();
  assert.equal(bg.additions().length, 0);
  bg.changed.emit({ id: 1, filename: { current: '/Downloads/setup.exe' } });
  await settle();
  assert.equal(bg.additions().length, 1);
});

test('events during submission cannot duplicate a request or cancel before acceptance', async () => {
  const bg = background();
  let accept;
  bg.setSubmit(() => new Promise((resolve) => { accept = resolve; }));
  bg.created.emit(bg.snapshot({ filename: 'setup.exe' }));
  await settle();
  bg.changed.emit({ id: 1, totalBytes: { current: 1024 } });
  bg.changed.emit({ id: 1, mime: { current: 'application/octet-stream' } });
  await settle();
  assert.equal(bg.additions().length, 1);
  assert.ok(!bg.calls.some(([kind]) => kind === 'cancel'));
  accept();
  await settle();
  assert.equal(bg.additions().length, 1);
  assert.equal(bg.calls.filter(([kind]) => kind === 'cancel').length, 1);
});

test('failed submission leaves the browser download intact and does not retry on updates', async () => {
  const bg = background();
  bg.failSubmission();
  bg.created.emit(bg.snapshot({ filename: 'setup.exe' }));
  await settle();
  bg.changed.emit({ id: 1, totalBytes: { current: 1024 } });
  await settle();
  assert.equal(bg.additions().length, 1);
  assert.ok(!bg.calls.some(([kind]) => ['cancel', 'erase', 'removeFile'].includes(kind)));
  assert.equal(bg.logs.at(-1)[0], 'warn');
});

test('size threshold waits for known size and sendHeaders false never collects cookies', async () => {
  const bg = background();
  bg.config.captureFileSizeMinimumKb = 1;
  bg.config.sendHeaders = false;
  bg.created.emit(bg.snapshot({ filename: 'setup.exe' }));
  await settle();
  assert.equal(bg.additions().length, 0);
  bg.changed.emit({ id: 1, totalBytes: { current: 1024 } });
  await settle();
  assert.equal(bg.additions()[0][1].items[0].headers, null);
  assert.ok(!bg.calls.some(([kind]) => kind === 'cookies'));
});

test('expired, erased, terminal and historical downloads are never revived by later updates', async () => {
  for (const cleanup of ['expired', 'erased', 'complete', 'interrupted', 'historical']) {
    const bg = background();
    bg.created.emit(bg.snapshot(cleanup === 'historical' ? {
      startTime: new Date(Date.now() - 60_000).toISOString(),
    } : {}));
    await settle();
    if (cleanup === 'expired') bg.expire();
    else if (cleanup === 'erased') bg.erased.emit(1);
    else if (cleanup !== 'historical') bg.changed.emit({ id: 1, state: { current: cleanup } });
    bg.changed.emit({ id: 1, filename: { current: 'setup.exe' } });
    await settle();
    assert.equal(bg.additions().length, 0, cleanup);
    assert.equal(bg.timers.size, 0, cleanup);
  }
});

test('metadata updated during a settings read is evaluated before submission', async () => {
  const bg = background();
  let release;
  bg.setReadConfig(() => new Promise((resolve) => { release = resolve; }));
  bg.created.emit(bg.snapshot({ filename: 'placeholder.exe' }));
  bg.changed.emit({ id: 1, filename: { current: 'actual.exe' } });
  bg.setReadConfig(async () => ({ ...bg.config }));
  release({ ...bg.config });
  await settle();
  assert.equal(bg.additions()[0][1].items[0].suggestedName, 'actual.exe');
});

test('bypass, disabled capture and redirect blacklist prevent capture', async () => {
  for (const mode of ['bypass', 'disabled', 'blacklisted']) {
    const bg = background();
    if (mode === 'bypass') bg.handlers().setBypass(true);
    if (mode === 'disabled') bg.config.autoCaptureLinks = false;
    if (mode === 'blacklisted') bg.config.blacklistedUrls = ['https://cdn.example.com/*'];
    bg.created.emit(bg.snapshot({ filename: 'setup.exe' }));
    await settle();
    assert.equal(bg.additions().length, 0, mode);
  }
});

test('distinct download IDs for the same URL are independent submissions', async () => {
  const bg = background();
  bg.created.emit(bg.snapshot({ id: 1, filename: 'setup.exe' }));
  bg.created.emit(bg.snapshot({ id: 2, filename: 'setup.exe' }));
  await settle();
  assert.equal(bg.additions().length, 2);
  assert.deepEqual(bg.calls.filter(([kind]) => kind === 'cancel').map(([, id]) => id).sort(), [1, 2]);
});

test('terminal events while reading settings prevent a late submission', async () => {
  const bg = background();
  let release;
  bg.setReadConfig(() => new Promise((resolve) => { release = resolve; }));
  bg.created.emit(bg.snapshot({ filename: 'setup.exe' }));
  bg.changed.emit({ id: 1, state: { current: 'complete' } });
  release({ ...bg.config });
  await settle();
  assert.equal(bg.additions().length, 0);
  assert.equal(bg.timers.size, 0);
});

test('unexpected processing failures are contained and diagnosed without request data', async () => {
  const bg = background();
  bg.setReadConfig(async () => { throw new Error('private URL or credentials'); });
  bg.created.emit(bg.snapshot({ filename: 'setup.exe' }));
  await settle();
  assert.equal(bg.additions().length, 0);
  assert.equal(bg.logs.at(-1)[0], 'error');
  assert.ok(!JSON.stringify(bg.logs).includes('private URL'));
});

test('pending downloads are bounded and expiry releases tracking slots', async () => {
  const bg = background();
  for (let id = 0; id <= constants.MAX_PENDING_DOWNLOADS; id++) bg.created.emit(bg.snapshot({ id }));
  await settle();
  assert.equal(bg.timers.size, constants.MAX_PENDING_DOWNLOADS);
  bg.expire();
  assert.equal(bg.timers.size, 0);
  bg.created.emit(bg.snapshot({ id: 1000, filename: 'setup.exe' }));
  await settle();
  assert.equal(bg.additions().length, 1);
});
