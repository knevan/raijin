import test from 'node:test';
import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import { runInNewContext } from 'node:vm';
import ts from 'typescript';

const component = await readFile(new URL('../entrypoints/popup/App.vue', import.meta.url), 'utf8');
const script = component.match(/<script[^>]*>([\s\S]*?)<\/script>/)?.[1];
assert.ok(script, 'popup setup script must exist');
const { outputText } = ts.transpileModule(script, {
  compilerOptions: { target: ts.ScriptTarget.ES2022, module: ts.ModuleKind.CommonJS },
});
const settle = () => new Promise((resolve) => setImmediate(resolve));

function popup() {
  let mounted;
  let unmounted;
  let timerId = 0;
  const timers = new Map();
  const calls = [];
  const errors = [];
  const config = { port: 35343, token: '', autoCaptureLinks: true, sendHeaders: true };
  let probe = async () => ({ ok: false, reason: 'down' });
  let readConfig = async () => ({ ...config });
  const modules = {
    vue: {
      ref: (value) => ({ value }),
      computed: (getter) => ({ get value() { return getter(); } }),
      onMounted: (callback) => { mounted = callback; },
      onUnmounted: (callback) => { unmounted = callback; },
    },
    'wxt/browser': { browser: {} },
    '@/utils/config': { getAll: () => readConfig(), setConfigItem: async () => {} },
    '@/utils/messaging': {
      sendTestPort: (port, token) => {
        calls.push({ port, token });
        return probe();
      },
      sendAddDownload: async () => true,
    },
  };
  const exports = {};
  runInNewContext(`${outputText}\nexports.state = { status, statusMessage, actionMessage };`, {
    exports,
    require: (id) => {
      assert.ok(Object.hasOwn(modules, id), `unexpected import: ${id}`);
      return modules[id];
    },
    console: { error: (...args) => errors.push(args) },
    setTimeout: (callback, delay) => {
      timers.set(++timerId, { callback, delay });
      return timerId;
    },
    clearTimeout: (id) => timers.delete(id),
  });
  return {
    ...exports.state,
    config, calls, timers, errors,
    setProbe: (callback) => { probe = callback; },
    setReadConfig: (callback) => { readConfig = callback; },
    async open() {
      await mounted();
      await settle();
    },
    close() { unmounted(); },
    async tick() {
      assert.equal(timers.size, 1, 'exactly one check must be scheduled');
      const [id, timer] = timers.entries().next().value;
      assert.equal(timer.delay, 250);
      timers.delete(id);
      timer.callback();
      await settle();
    },
  };
}

test('an open popup discovers the app and preserves action messages without reopening', async () => {
  const view = popup();
  await view.open();
  assert.equal(view.status.value, 'down');
  assert.equal(view.calls.length, 1);
  view.actionMessage.value = 'Sent to Raijin';
  view.setProbe(async () => ({ ok: true, reason: 'basic' }));
  await view.tick();
  assert.equal(view.status.value, 'basic');
  assert.equal(view.actionMessage.value, 'Sent to Raijin');
  view.setProbe(async () => ({ ok: false, reason: 'down' }));
  await view.tick();
  assert.equal(view.status.value, 'down');
  view.close();
  assert.equal(view.timers.size, 0);
});

test('each check uses the latest port and token and keeps rejected credentials distinct', async () => {
  const view = popup();
  await view.open();
  view.config.port = 35344;
  view.config.token = 'updated-token';
  view.setProbe(async () => ({ ok: true, reason: 'ok' }));
  await view.tick();
  assert.deepEqual(view.calls.at(-1), { port: 35344, token: 'updated-token' });
  assert.equal(view.status.value, 'ok');
  assert.equal(view.statusMessage.value, 'Connected on port 35344');
  view.setProbe(async () => ({ ok: false, reason: 'unauthorized' }));
  await view.tick();
  assert.equal(view.status.value, 'unauthorized');
  assert.equal(view.config.token, 'updated-token');
  view.close();
});

test('a pending check never overlaps and its completion after close cannot restart polling', async () => {
  const view = popup();
  await view.open();
  let finish;
  view.setProbe(() => new Promise((resolve) => { finish = resolve; }));
  await view.tick();
  assert.equal(view.calls.length, 2);
  assert.equal(view.timers.size, 0);
  assert.equal(view.status.value, 'down');
  view.close();
  finish({ ok: true, reason: 'ok' });
  await settle();
  assert.equal(view.status.value, 'down');
  assert.equal(view.timers.size, 0);
  assert.equal(view.calls.length, 2);
});

test('closing during a settings read prevents a new background connection check', async () => {
  const view = popup();
  await view.open();
  let finish;
  view.setReadConfig(() => new Promise((resolve) => { finish = resolve; }));
  await view.tick();
  view.close();
  finish({ ...view.config });
  await settle();
  assert.equal(view.calls.length, 1);
  assert.equal(view.timers.size, 0);
});

test('a failed check keeps polling and records the error context', async () => {
  const view = popup();
  const failure = new Error('Background request failed');
  view.setProbe(async () => { throw failure; });
  await view.open();
  assert.equal(view.status.value, 'down');
  assert.equal(view.errors[0][1], failure);
  view.setProbe(async () => ({ ok: true, reason: 'basic' }));
  await view.tick();
  assert.equal(view.status.value, 'basic');
  view.close();
});
