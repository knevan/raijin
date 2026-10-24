<script lang="ts" setup>
import { computed, onMounted, ref } from 'vue';
import { browser } from 'wxt/browser';
import { getAll, setConfigItem } from '@/utils/config';
import { sendAddDownload, sendTestPort } from '@/utils/messaging';
import type { ConnectionStatus } from '@/utils/types';

const autoCapture = ref(true);
const sendHeaders = ref(true);
const status = ref<ConnectionStatus>('unknown');
const statusMessage = ref('Checking…');
const testing = ref(false);
const adding = ref(false);
const actionMessage = ref('');

let currentPort = 35343;
let currentToken = '';

const isConnected = computed(() => status.value === 'basic' || status.value === 'ok');

const statusBadge = computed(() => {
  if (status.value === 'basic') {
    return {
      label: 'Basic',
      tooltip: 'Downloads require desktop confirmation. Get token in Raijin App: Settings → Browser Integration.',
    };
  }
  if (status.value === 'ok') {
    return {
      label: 'Full',
      tooltip: 'Silent downloads enabled via authenticated desktop token.',
    };
  }
  return null;
});

function statusMainLabel(): string {
  if (isConnected.value) return 'Connected';
  if (status.value === 'down') return 'Server down';
  if (status.value === 'unauthorized') return 'Token rejected';
  return 'Checking…';
}

async function load(): Promise<void> {
  const config = await getAll();
  currentPort = config.port;
  currentToken = config.token;
  autoCapture.value = config.autoCaptureLinks;
  sendHeaders.value = config.sendHeaders;
}

async function checkConnection(): Promise<void> {
  testing.value = true;
  actionMessage.value = '';
  try {
    const result = await sendTestPort(currentPort, currentToken);
    if (result.reason === 'basic') {
      status.value = 'basic';
      statusMessage.value = 'Downloads require confirmation in Raijin. Add a token in Options to enable silent downloads.';
    } else if (result.reason === 'ok') {
      status.value = 'ok';
      statusMessage.value = `Connected on port ${currentPort}`;
    } else if (result.reason === 'unauthorized') {
      status.value = 'unauthorized';
      statusMessage.value = 'Server is up, but the token was rejected. Update token in Options.';
    } else {
      status.value = 'down';
      statusMessage.value = 'Server is down. Start Raijin and enable browser integration.';
    }
  } catch {
    status.value = 'down';
    statusMessage.value = 'Server is down. Start Raijin and enable browser integration.';
  } finally {
    testing.value = false;
  }
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
    void checkConnection();
  });
});
</script>

<template>
  <div class="popup">
    <header class="header">
      <img src="/icon/48.png" alt="Raijin" class="icon" />
      <div class="header-text">
        <div class="title">Raijin</div>
        <div class="status" :data-state="status">
          <span class="status-dot"></span>
          <span class="status-text">{{ statusMainLabel() }}</span>
          <span
            v-if="statusBadge"
            class="badge"
            :class="statusBadge.label === 'Full' ? 'badge-full' : 'badge-basic'"
            :title="statusBadge.tooltip"
          >
            {{ statusBadge.label }}
          </span>
        </div>
      </div>
    </header>

    <p v-if="status === 'down' || status === 'unauthorized'" class="hint hint-error">
      {{ statusMessage }}
    </p>

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
    <p v-if="actionMessage" class="action-msg">{{ actionMessage }}</p>
  </div>
</template>

<style scoped>
.popup {
  width: 300px;
  padding: 14px;
  font-size: 13px;
  display: flex;
  flex-direction: column;
  gap: 12px;
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
.header-text {
  display: flex;
  flex-direction: column;
  gap: 2px;
}
.title {
  font-weight: 700;
  font-size: 15px;
  line-height: 1.2;
}
.status {
  display: flex;
  align-items: center;
  gap: 6px;
  font-size: 12px;
  font-weight: 500;
  color: #888;
}
.status-dot {
  width: 7px;
  height: 7px;
  border-radius: 50%;
  background-color: #888;
  flex-shrink: 0;
}
.status-text {
  line-height: 1;
}

/* Connected states: green dot and green 'Connected' text */
.status[data-state='ok'],
.status[data-state='basic'] {
  color: #2da44e;
}
.status[data-state='ok'] .status-dot,
.status[data-state='basic'] .status-dot {
  background-color: #2da44e;
}

/* Error states */
.status[data-state='down'],
.status[data-state='unauthorized'] {
  color: #cf222e;
}
.status[data-state='down'] .status-dot,
.status[data-state='unauthorized'] .status-dot {
  background-color: #cf222e;
}

/* Badges / Pills */
.badge {
  font-size: 10px;
  font-weight: 600;
  padding: 1px 8px;
  border-radius: 999px;
  cursor: help;
  user-select: none;
  line-height: 1.4;
  letter-spacing: 0.02em;
}
.badge-basic {
  color: #d29922;
  background-color: rgba(210, 153, 34, 0.15);
  border: 1px solid rgba(210, 153, 34, 0.4);
}
.badge-full {
  color: #2da44e;
  background-color: rgba(45, 164, 78, 0.15);
  border: 1px solid rgba(45, 164, 78, 0.4);
}

.hint {
  margin: 0;
  font-size: 12px;
  line-height: 1.4;
}
.hint-error {
  color: #cf222e;
}

.toggles {
  display: flex;
  flex-direction: column;
  gap: 8px;
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
.row {
  display: flex;
  gap: 8px;
}
.row button {
  flex: 1;
}
.action-msg {
  margin: 0;
  font-size: 12px;
  color: #888;
  text-align: center;
}
</style>
