<script lang="ts" setup>
import { onMounted, ref } from 'vue';
import { defaultConfig, getAll, isValidPort, normalizePort, setAll, setConfigItem } from '@/utils/config';
import { MAX_ALLOWED_PORT, MIN_ALLOWED_PORT } from '@/utils/constants';
import { sendTestPort } from '@/utils/messaging';

const portText = ref(String(defaultConfig.port));
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
const status = ref('');
const testing = ref(false);
const saving = ref(false);

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
}

async function save(): Promise<void> {
  saving.value = true;
  status.value = '';
  const port = normalizePort(Number.parseInt(portText.value, 10));
  if (!isValidPort(port)) {
    status.value = `Port must be ${MIN_ALLOWED_PORT}–${MAX_ALLOWED_PORT}`;
    saving.value = false;
    return;
  }
  portText.value = String(port);
  const minSize = Math.max(0, Math.floor(Number(minSizeText.value) || 0));
  minSizeText.value = String(minSize);
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
  fileTypesText.value = normalizeFileTypes(fileTypesText.value).join(', ');
  status.value = 'Saved';
  saving.value = false;
}

async function test(): Promise<void> {
  testing.value = true;
  status.value = '';
  const port = normalizePort(Number.parseInt(portText.value, 10));
  try {
    const result = await sendTestPort(port, token.value);
    if (result.reason === 'ok') status.value = `Connected on port ${port}`;
    else if (result.reason === 'unauthorized') status.value = 'Server is up, but the token was rejected';
    else status.value = 'Server is down. Start Raijin and enable browser integration';
  } catch {
    status.value = 'Server is down. Start Raijin and enable browser integration';
  } finally {
    testing.value = false;
  }
}

async function clearToken(): Promise<void> {
  token.value = '';
  await setConfigItem('token', '');
  status.value = 'Token cleared';
}

function resetDefaults(): void {
  fileTypesText.value = defaultConfig.registeredFileTypes.join(', ');
  blacklistText.value = '';
}

onMounted(() => {
  void load();
});
</script>

<template>
  <div class="options">
    <header class="header">
      <img src="/icon/48.png" alt="Raijin" />
      <h1>Raijin Options</h1>
    </header>

    <section>
      <h2>Connection</h2>
      <label>Port ({{ MIN_ALLOWED_PORT }}–{{ MAX_ALLOWED_PORT }})
        <input v-model="portText" type="number" :min="MIN_ALLOWED_PORT" :max="MAX_ALLOWED_PORT" />
      </label>
      <label>Token
        <input v-model="token" type="password" autocomplete="off" placeholder="Paste desktop token" />
      </label>
      <div class="row">
        <button type="button" :disabled="saving" @click="save">{{ saving ? 'Saving…' : 'Save' }}</button>
        <button type="button" :disabled="testing" @click="test">{{ testing ? 'Testing…' : 'Test' }}</button>
        <button type="button" @click="clearToken">Clear token</button>
      </div>
      <p v-if="status" class="status">{{ status }}</p>
    </section>

    <section>
      <h2>Capture</h2>
      <label class="check"><input v-model="autoCaptureLinks" type="checkbox" /> Auto capture links</label>
      <label class="check"><input v-model="popupEnabled" type="checkbox" /> Show selection popups</label>
      <label class="check"><input v-model="mediaOverlayEnabled" type="checkbox" /> Show download button over videos</label>
      <label class="check"><input v-model="sendHeaders" type="checkbox" /> Send headers (cookies go to localhost only)</label>
      <label>File types (comma separated)
        <textarea v-model="fileTypesText" rows="3"></textarea>
      </label>
      <label>Ignored URL patterns (one per line, * wildcard)
        <textarea v-model="blacklistText" rows="4"></textarea>
      </label>
      <button type="button" @click="resetDefaults">Reset lists to defaults</button>
      <label>Minimum file size (KB, 0 = capture all)
        <input v-model="minSizeText" type="number" min="0" />
      </label>
      <label>Bypass key (hold to skip capture once)
        <input v-model="bypassShortcut" type="text" />
      </label>
    </section>

    <section>
      <h2>Behavior</h2>
      <label class="check"><input v-model="allowPass" type="checkbox" /> Let browser continue when Raijin is down</label>
      <label class="check"><input v-model="closeNewTab" type="checkbox" /> Close new tab after capture</label>
      <label class="check"><input v-model="silentAdd" type="checkbox" /> Add downloads silently</label>
      <label class="check"><input v-model="silentStart" type="checkbox" :disabled="!silentAdd" /> Start downloads silently</label>
      <div class="row">
        <button type="button" :disabled="saving" @click="save">{{ saving ? 'Saving…' : 'Save' }}</button>
      </div>
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
  gap: 12px;
  align-items: center;
}
.header img {
  width: 36px;
  height: 36px;
}
.header h1 {
  font-size: 20px;
  margin: 0;
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
.row {
  display: flex;
  gap: 8px;
}
.status {
  font-size: 13px;
}
</style>
