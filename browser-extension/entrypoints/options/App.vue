<script lang="ts" setup>
import { computed, onMounted, ref } from 'vue';
import { defaultConfig, getAll, isValidPort, normalizePort, setAll } from '@/utils/config';
import { MAX_ALLOWED_PORT, MIN_ALLOWED_PORT } from '@/utils/constants';
import { sendTestPort } from '@/utils/messaging';

const portText = ref(String(defaultConfig.port));
const portError = ref('');
const token = ref('');
const autoCaptureLinks = ref(true);
const popupEnabled = ref(true);
const mediaOverlayEnabled = ref(true);
const sendHeaders = ref(true);
const fileTypesText = ref('');
const blacklistText = ref('');
const allowPass = ref(true);
const closeNewTab = ref(true);
const silentAdd = ref(false);
const silentStart = ref(false);
const minSizeText = ref('0');
const bypassShortcut = ref('Delete');

const connectionStatus = ref('');
const connectionStatusClass = computed(() => {
  if (connectionStatus.value.startsWith('Connected — Basic')) return 'status-basic';
  if (connectionStatus.value.startsWith('Connected')) return 'status-ok';
  if (connectionStatus.value.startsWith('Server is down') || connectionStatus.value.includes('rejected')) return 'status-error';
  return '';
});
const testing = ref(false);
const saveStatus = ref<'saving' | 'saved' | ''>('');
const isLoaded = ref(false);
let saveTimer: ReturnType<typeof setTimeout> | undefined;

function normalizeFileTypes(raw: string): string[] {
  const out = new Set<string>();
  for (const part of raw.split(',')) {
    const cleaned = part.trim().toLowerCase().replace(/^\.+/, '');
    if (cleaned) out.add(cleaned);
  }
  return [...out];
}

function normalizeBlacklist(raw: string): string[] {
  const out = new Set<string>();
  for (const line of raw.split('\n')) {
    const cleaned = line.trim();
    if (cleaned) out.add(cleaned);
  }
  return [...out];
}

async function load(): Promise<void> {
  const config = await getAll();
  portText.value = String(config.port);
  token.value = config.token;
  autoCaptureLinks.value = config.autoCaptureLinks;
  popupEnabled.value = config.popupEnabled;
  mediaOverlayEnabled.value = config.mediaOverlayEnabled;
  sendHeaders.value = config.sendHeaders;
  fileTypesText.value = config.registeredFileTypes.join(', ');
  blacklistText.value = config.blacklistedUrls.join('\n');
  allowPass.value = config.allowPassDownloadIfAppNotRespond;
  closeNewTab.value = config.closeNewTabIfItWasCaptured;
  silentAdd.value = config.silentAddDownload;
  silentStart.value = config.silentStartDownload;
  minSizeText.value = String(config.captureFileSizeMinimumKb);
  bypassShortcut.value = config.bypassShortcut;
  isLoaded.value = true;
}

async function save(): Promise<void> {
  if (!isLoaded.value) return;

  const rawPort = Number.parseInt(portText.value, 10);
  if (!Number.isFinite(rawPort) || !isValidPort(rawPort)) {
    portError.value = `Port must be ${MIN_ALLOWED_PORT}–${MAX_ALLOWED_PORT}`;
    return;
  }
  portError.value = '';
  const port = normalizePort(rawPort);
  portText.value = String(port);

  const minSize = Math.max(0, Math.floor(Number(minSizeText.value) || 0));
  minSizeText.value = String(minSize);

  if (!token.value) {
    silentAdd.value = false;
    silentStart.value = false;
  }

  saveStatus.value = 'saving';
  if (saveTimer) clearTimeout(saveTimer);

  await setAll({
    port,
    token: token.value,
    autoCaptureLinks: autoCaptureLinks.value,
    popupEnabled: popupEnabled.value,
    mediaOverlayEnabled: mediaOverlayEnabled.value,
    sendHeaders: sendHeaders.value,
    registeredFileTypes: normalizeFileTypes(fileTypesText.value),
    blacklistedUrls: normalizeBlacklist(blacklistText.value),
    allowPassDownloadIfAppNotRespond: allowPass.value,
    closeNewTabIfItWasCaptured: closeNewTab.value,
    silentAddDownload: silentAdd.value,
    silentStartDownload: silentAdd.value ? silentStart.value : false,
    captureFileSizeMinimumKb: minSize,
    bypassShortcut: bypassShortcut.value.trim() || 'Delete',
  });

  saveStatus.value = 'saved';
  saveTimer = setTimeout(() => {
    saveStatus.value = '';
  }, 2000);
}

function onSilentAddChange(): void {
  if (!silentAdd.value) {
    silentStart.value = false;
  }
  void save();
}

async function test(): Promise<void> {
  testing.value = true;
  connectionStatus.value = '';
  const port = normalizePort(Number.parseInt(portText.value, 10));
  try {
    const result = await sendTestPort(port, token.value);
    if (result.reason === 'basic') {
      connectionStatus.value = 'Connected — Basic access. Downloads require desktop confirmation';
    } else if (result.reason === 'ok') {
      connectionStatus.value = `Connected — Authenticated on port ${port}`;
    } else if (result.reason === 'unauthorized') {
      connectionStatus.value = 'Server is up, but the token was rejected';
    } else {
      connectionStatus.value = 'Server is down. Start Raijin and enable browser integration';
    }
  } catch {
    connectionStatus.value = 'Server is down. Start Raijin and enable browser integration';
  } finally {
    testing.value = false;
  }
}

async function clearToken(): Promise<void> {
  token.value = '';
  connectionStatus.value = 'Token cleared';
  await save();
}

async function resetDefaults(): Promise<void> {
  fileTypesText.value = defaultConfig.registeredFileTypes.join(', ');
  blacklistText.value = '';
  await save();
}

onMounted(() => {
  void load();
});
</script>

<template>
  <div class="options">
    <header class="header">
      <div class="brand">
        <img src="/icon/48.png" alt="Raijin" />
        <h1>Raijin Options</h1>
      </div>
      <div class="save-status" :class="{ visible: saveStatus, saved: saveStatus === 'saved' }">
        <span v-if="saveStatus === 'saving'">Saving…</span>
        <span v-else-if="saveStatus === 'saved'">Saved ✓</span>
      </div>
    </header>

    <section>
      <h2>Capture</h2>
      <label class="check">
        <input v-model="autoCaptureLinks" type="checkbox" @change="save" />
        Auto capture links
      </label>
      <label class="check">
        <input v-model="popupEnabled" type="checkbox" @change="save" />
        Show selection popups
      </label>
      <label class="check">
        <input v-model="mediaOverlayEnabled" type="checkbox" @change="save" />
        Show download button over videos
      </label>
      <label class="check">
        <input v-model="sendHeaders" type="checkbox" @change="save" />
        Send headers (cookies go to localhost only)
      </label>
      <label>File types (comma separated)
        <textarea v-model="fileTypesText" rows="3" @change="save" @blur="save"></textarea>
      </label>
      <label>Ignored URL patterns (one per line, * wildcard)
        <textarea v-model="blacklistText" rows="4" @change="save" @blur="save"></textarea>
      </label>
      <button type="button" class="btn-action" @click="resetDefaults">Reset lists to defaults</button>
      <label>Minimum file size (KB, 0 = capture all)
        <input v-model="minSizeText" type="number" min="0" @change="save" @blur="save" />
      </label>
      <label>Bypass key (hold to skip capture once)
        <input v-model="bypassShortcut" type="text" @change="save" @blur="save" />
      </label>
    </section>

    <section>
      <h2>Behavior</h2>
      <label class="check">
        <input v-model="allowPass" type="checkbox" @change="save" />
        Let browser continue when Raijin is down
      </label>
      <label class="check">
        <input v-model="closeNewTab" type="checkbox" @change="save" />
        Close new tab after capture
      </label>
      <label
        class="check"
        :class="{ disabled: !token }"
        :title="!token ? 'Requires API token. Copy it from Raijin Desktop: Settings → Browser Integration' : ''"
      >
        <input v-model="silentAdd" type="checkbox" :disabled="!token" @change="onSilentAddChange" />
        <span class="label-text">Add downloads silently</span>
        <span v-if="!token" class="badge-lock">Requires API token</span>
      </label>
      <label
        class="check"
        :class="{ disabled: !token || !silentAdd }"
        :title="!token ? 'Requires API token. Copy it from Raijin Desktop: Settings → Browser Integration' : (!silentAdd ? 'Requires &quot;Add downloads silently&quot; to be enabled' : '')"
      >
        <input v-model="silentStart" type="checkbox" :disabled="!token || !silentAdd" @change="save" />
        <span class="label-text">Start downloads silently</span>
        <span v-if="!token" class="badge-lock">Requires API token</span>
        <span v-else-if="!silentAdd" class="badge-lock">Requires silent add</span>
      </label>
    </section>

    <section>
      <h2>Connection</h2>
      <label>Port ({{ MIN_ALLOWED_PORT }}–{{ MAX_ALLOWED_PORT }})
        <input v-model="portText" type="number" :min="MIN_ALLOWED_PORT" :max="MAX_ALLOWED_PORT" @change="save" @blur="save" />
      </label>
      <p v-if="portError" class="field-error">{{ portError }}</p>
      <p class="hint">Without a token, each download requires confirmation in Raijin. Add a token for silent downloads and access to download lists.</p>
      <div class="guide-box">
        <span class="guide-icon">💡</span>
        <span>Get API token in Raijin Desktop: <strong>Settings &rarr; Browser Integration</strong></span>
      </div>
      <label>API token (optional)
        <input v-model="token" type="password" autocomplete="off" placeholder="Paste token from Raijin App (Settings → Browser Integration)" @change="save" @blur="save" />
      </label>
      <div class="row">
        <button type="button" class="btn-action" :disabled="testing" @click="test">
          {{ testing ? 'Testing…' : 'Test' }}
        </button>
        <button type="button" class="btn-action" @click="clearToken">Clear token</button>
      </div>
      <p v-if="connectionStatus" class="status" :class="connectionStatusClass">{{ connectionStatus }}</p>
    </section>
  </div>
</template>

<style scoped>
.options {
  display: flex;
  flex-direction: column;
  gap: 20px;
}
.header {
  display: flex;
  align-items: center;
  justify-content: space-between;
}
.brand {
  display: flex;
  gap: 12px;
  align-items: center;
}
.brand img {
  width: 36px;
  height: 36px;
}
.brand h1 {
  font-size: 20px;
  margin: 0;
}
.save-status {
  font-size: 13px;
  font-weight: 500;
  color: #888;
  opacity: 0;
  transition: opacity 0.2s ease;
}
.save-status.visible {
  opacity: 1;
}
.save-status.saved {
  color: #22c55e;
}
section {
  display: flex;
  flex-direction: column;
  gap: 10px;
  border-top: 1px solid #444;
  padding-top: 12px;
}
section h2 {
  font-size: 14px;
  margin: 0;
  text-transform: uppercase;
  letter-spacing: 0.04em;
}
label {
  display: flex;
  flex-direction: column;
  gap: 4px;
  font-size: 13px;
}
label.check {
  flex-direction: row;
  align-items: center;
  gap: 8px;
  cursor: pointer;
}
label.check.disabled {
  cursor: not-allowed;
}
label.check.disabled .label-text {
  color: #777;
}
label.check.disabled input {
  opacity: 0.5;
  cursor: not-allowed;
}
.badge-lock {
  font-size: 11px;
  font-weight: 600;
  color: #f59e0b;
  background-color: rgba(245, 158, 11, 0.15);
  border: 1px solid rgba(245, 158, 11, 0.4);
  padding: 1px 7px;
  border-radius: 4px;
  letter-spacing: 0.01em;
  line-height: 1.4;
}
.guide-box {
  display: flex;
  align-items: center;
  gap: 8px;
  font-size: 12px;
  color: #ccc;
  background-color: rgba(255, 255, 255, 0.04);
  border: 1px solid rgba(255, 255, 255, 0.1);
  border-left: 3px solid #f59e0b;
  padding: 7px 10px;
  border-radius: 4px;
  margin: 2px 0 6px;
}
.guide-box strong {
  color: #fff;
}
.guide-icon {
  font-size: 14px;
  flex-shrink: 0;
}
.hint {
  font-size: 12px;
  margin: 0;
  opacity: 0.8;
}
input,
textarea {
  padding: 6px 8px;
  border-radius: 6px;
  border: 1px solid #555;
  background: inherit;
  color: inherit;
  font: inherit;
}
.btn-action {
  align-self: flex-start;
  padding: 6px 12px;
  border-radius: 6px;
  border: 1px solid #555;
  background: rgba(255, 255, 255, 0.08);
  color: inherit;
  font: inherit;
  cursor: pointer;
}
.btn-action:hover:not(:disabled) {
  background: rgba(255, 255, 255, 0.15);
}
.btn-action:disabled {
  opacity: 0.5;
  cursor: not-allowed;
}
.row {
  display: flex;
  gap: 8px;
}
.status {
  font-size: 13px;
  margin: 0;
}
.status.status-ok {
  color: #22c55e;
}
.status.status-basic {
  color: #d29922;
}
.status.status-error {
  color: #ef4444;
}
.field-error {
  font-size: 12px;
  color: #ef4444;
  margin: -4px 0 0;
}
</style>
