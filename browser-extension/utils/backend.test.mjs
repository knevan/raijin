import test, { afterEach, mock } from 'node:test';
import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import ts from 'typescript';

function moduleUrl(source) {
  const { outputText } = ts.transpileModule(source, {
    compilerOptions: { target: ts.ScriptTarget.ES2022, module: ts.ModuleKind.ES2022 },
  });
  return `data:text/javascript;base64,${Buffer.from(outputText).toString('base64')}`;
}

const constants = moduleUrl(await readFile(new URL('./constants.ts', import.meta.url), 'utf8'));
const source = (await readFile(new URL('./backend.ts', import.meta.url), 'utf8'))
  .replace("'./constants'", JSON.stringify(constants));
const { BackendApi, UnauthorizedError, ApiError } = await import(moduleUrl(source));
afterEach(() => mock.restoreAll());

const request = { items: [], options: { silentAdd: false, silentStart: false } };

test('ping advertises Basic access only when the desktop explicitly supports it', async () => {
  for (const [body, supported] of [['{"basicAccess":true}', true], ['', false], ['{}', false]]) {
    mock.method(globalThis, 'fetch', async () => new Response(body));
    assert.equal(await new BackendApi(35343).ping(), supported);
    mock.restoreAll();
  }
});

test('Basic add sends no credential and targets only the loopback add endpoint', async () => {
  mock.method(globalThis, 'fetch', async (url, init) => {
    assert.equal(url, 'http://127.0.0.1:35343/add');
    assert.equal(init.headers.Authorization, undefined);
    assert.equal(init.headers['Content-Type'], 'application/json');
    assert.deepEqual(JSON.parse(init.body), request);
    return new Response('', { status: 202 });
  });
  await new BackendApi(35343).add(request);
});

test('a rejected token fails without retrying as Basic access', async () => {
  const fetch = mock.method(globalThis, 'fetch', async (_, init) => {
    assert.equal(init.headers.Authorization, 'Bearer expired');
    return new Response('', { status: 401 });
  });
  await assert.rejects(new BackendApi(35343, 'expired').add(request), UnauthorizedError);
  assert.equal(fetch.mock.callCount(), 1);
});

test('authentication probe must not accept an unsupported endpoint', async () => {
  mock.method(globalThis, 'fetch', async () => new Response('', { status: 404 }));
  await assert.rejects(new BackendApi(35343, 'secret').probeAuth(), ApiError);
});
