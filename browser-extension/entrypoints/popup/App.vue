<script lang="ts" setup>
import { onMounted, ref } from 'vue';
import { browser } from 'wxt/browser';
import { getAll, normalizePort, setConfigItem } from '@/utils/config';
import { MAX_ALLOWED_PORT, MIN_ALLOWED_PORT } from '@/utils/constants';
import { sendAddDownload, sendTestPort } from '@/utils/messaging';
import type { ConnectionStatus } from '@/utils/types';

const portText = ref(String(35343));
const token = ref('');
const autoCapture = ref(true);
const sendHeaders = ref(true);
const status = ref<ConnectionStatus>('unknown');
const statusMessage = ref('Not checked yet');
const testing = ref(false);
const adding = ref(false);
const actionMessage = ref('');

function statusLabel(): string {
  if (status.value === 'basic') return 'Connected — Basic access';
  if (status.value === 'ok') return 'Connected — Authenticated';
  if (status.value === 'down') return 'Server down';
  if (status.value === 'unauthorized') return 'Token rejected';
  return 'Checking…';
}

async function load(): Promise<void> {
  const config = await getAll();
  portText.value = String(config.port);
  token.value = config.token;
  autoCapture.value = config.autoCaptureLinks;
  sendHeaders.value = config.sendHeaders;
}

function currentPort(): number {
  const parsed = Number.parseInt(portText.value, 10);
  if (Number.isNaN(parsed)) return 35343;
  return normalizePort(parsed);
}

async function test(): Promise<void> {
  testing.value = true;
  actionMessage.value = '';
  const port = currentPort();
  portText.value = String(port);
  await setConfigItem('port', port);
  await setConfigItem('token', token.value);
  try {
    const result = await sendTestPort(port, token.value);
    if (result.reason === 'basic') {
      status.value = 'basic';
      statusMessage.value = 'Downloads require confirmation in Raijin. Add a token to enable silent downloads';
    } else if (result.reason === 'ok') {
      status.value = 'ok';
      statusMessage.value = `Connected on port ${port}`;
    } else if (result.reason === 'unauthorized') {
      status.value = 'unauthorized';
      statusMessage.value = 'Server is up, but the token was rejected';
    } else {
      status.value = 'down';
      statusMessage.value = 'Server is down. Start Raijin and enable browser integration';
    }
  } catch {
    status.value = 'down';
    statusMessage.value = 'Server is down. Start Raijin and enable browser integration';
  } finally {
    testing.value = false;
  }
}

async function clearToken(): Promise<void> {
  token.value = '';
  await setConfigItem('token', '');
  status.value = 'unknown';
  statusMessage.value = 'Token cleared';
}

async function toggleAutoCapture(): Promise<void> {
  autoCapture.value = !autoCapture.value;
  await setConfigItem('autoCaptureLinks', autoCapture.value);
}

async function toggleSendHeaders(): Promise<void> {
  sendHeaders.value = !sendHeaders.value;
  await setConfigItem('sendHeaders', sendHeaders.value);
}

async function openOptions(): Promise<void> {
  try {
    await browser.runtime.openOptionsPage();
  } catch {
    // Best effort.
  }
}

async function addActiveTab(): Promise<void> {
  adding.value = true;
  actionMessage.value = '';
  try {
    const tabs = await browser.tabs.query({ active: true, currentWindow: true });
    const tab = tabs[0];
    const url = tab?.url ?? '';
    if (!url.startsWith('http://') && !url.startsWith('https://')) {
      actionMessage.value = 'Active tab has no downloadable http(s) URL';
      return;
    }
    const ok = await sendAddDownload([
      {
        link: url,
        downloadPage: url,
        headers: null,
        description: tab?.title ?? null,
        suggestedName: null,
        type: 'http',
      },
    ]);
    actionMessage.value = ok ? 'Sent to Raijin' : 'Could not reach Raijin';
  } catch {
    actionMessage.value = 'Could not reach Raijin';
  } finally {
    adding.value = false;
  }
}

onMounted(() => {
  void load().then(() => {
    void test();
  });
});
</script>

<template>
  <div class="popup">
    <header class="header">
      <img src="/icon/48.png" alt="Raijin" class="icon" />
      <div>
        <div class="title">Raijin</div>
        <div class="status" :data-state="status">{{ statusLabel() }}</div>
      </div>
    </header>
    <p class="hint">{{ statusMessage }}</p>

    <label class="field">
      <span>Port ({{ MIN_ALLOWED_PORT }}–{{ MAX_ALLOWED_PORT }})</span>
      <input v-model="portText" type="number" :min="MIN_ALLOWED_PORT" :max="MAX_ALLOWED_PORT" />
    </label>
    <label class="field">
      <span>API token (optional)</span>
      <input v-model="token" type="password" autocomplete="off" placeholder="Paste desktop token" />
    </label>
    <div class="row">
      <button type="button" :disabled="testing" @click="test">
        {{ testing ? 'Testing…' : 'Test' }}
      </button>
      <button type="button" @click="clearToken">Clear token</button>
    </div>

    <div class="toggles">
      <button type="button" class="toggle" @click="toggleAutoCapture">
        <span>Auto capture</span>
        <span class="pill" :data-on="autoCapture">{{ autoCapture ? 'ON' : 'OFF' }}</span>
      </button>
      <button type="button" class="toggle" @click="toggleSendHeaders">
        <span>Send headers</span>
        <span class="pill" :data-on="sendHeaders">{{ sendHeaders ? 'ON' : 'OFF' }}</span>
      </button>
      <p v-if="sendHeaders" class="headers-on">Headers active: cookies are sent to localhost only</p>
    </div>

    <div class="row">
      <button type="button" @click="openOptions">Options</button>
      <button type="button" :disabled="adding" @click="addActiveTab">
        {{ adding ? 'Sending…' : 'Add active tab' }}
      </button>
    </div>
    <p v-if="actionMessage" class="hint">{{ actionMessage }}</p>
  </div>
</template>

<style scoped>
.popup {
  width: 300px;
  padding: 12px;
  font-size: 13px;
  display: flex;
  flex-direction: column;
  gap: 8px;
}
.header {
  display: flex;
  gap: 10px;
  align-items: center;
}
.icon {
  width: 32px;
  height: 32px;
}
.title {
  font-weight: 700;
  font-size: 15px;
}
.status {
  font-size: 12px;
  color: #888;
}
.status[data-state='ok'] {
  color: #2da44e;
}
.status[data-state='down'],
.status[data-state='unauthorized'] {
  color: #cf222e;
}
.hint {
  margin: 0;
  color: #888;
  font-size: 12px;
}
.field {
  display: flex;
  flex-direction: column;
  gap: 4px;
}
.field input {
  padding: 6px 8px;
  border-radius: 6px;
  border: 1px solid #444;
  background: inherit;
  color: inherit;
}
.row {
  display: flex;
  gap: 8px;
}
.row button {
  flex: 1;
}
.toggles {
  display: flex;
  flex-direction: column;
  gap: 6px;
}
.toggle {
  display: flex;
  justify-content: space-between;
  align-items: center;
}
.pill {
  font-size: 11px;
  padding: 2px 8px;
  border-radius: 999px;
  border: 1px solid #444;
}
.pill[data-on='true'] {
  color: #2da44e;
  border-color: #2da44e;
}
.headers-on {
  margin: 0;
  font-size: 11px;
  color: #2da44e;
}
</style>
