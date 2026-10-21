# Raijin Browser Extension — Porting Spec (wxt + Vue)

Sumber acuan: ABDM `ab-download-manager-browser-integration` (REFERENCE saja, bukan COPY 1:1).
Target: `wxt` + Vue 3 + TypeScript, satu codebase Chromium + Firefox.
Status server: `crates/core/src/integration/mod.rs` masih stub; kontrak di bawah ini mengikat kedua sisi.

## 1. Locked Decisions

- Scaffold: `wxt@^0.21.3` + `@wxt-dev/module-vue`, Vue `^3.5`. Config tulis format MV3 saja; `wxt` auto-convert ke MV2 saat target Firefox butuh.
- Default port Raijin: `35343`. Valid: `1024 <= 35343 <= 65535`, di luar well-known `0-1023` dan di luar ephemeral `49152-65535`. Berbeda dari default ABDM `15151`; jangan hardcode ganda — satu konstanta di tiap sisi + bisa diubah di Settings/options.
- Auth: satu cara saja — header `Authorization: Bearer <token>` untuk semua route mutasi. `POST /ping` terbuka tanpa auth (health check). Tanpa token di URL/query. Token salah/hilang → `401` JSON, tanpa bocor detail.
- Token format: random 256-bit, encode base64url (43 char) atau hex (64 char). Generate first-run di desktop, tampil sekali, bisa regenerate/revoke. Extension simpan di `browser.storage.local` saja (jangan `sync`).
- State: `wxt/storage` sebagai source of truth config. Tanpa Pinia untuk MVP; tambah Pinia hanya bila state transient popup/options terbukti kompleks.
- Messaging: native `browser.runtime` via `wxt` (typed wrapper tipis sendiri). Tanpa `webext-bridge` untuk MVP.
- Cookie/headers: default `sendHeaders: true` ikut ABDM (demi unduhan login/session), dengan toggle global + blacklist per-site + indikator di popup. Hanya kirim ke `http://127.0.0.1:35343`, tidak ke pihak ketiga. Jangan log nilai header.
- Min size default: `captureFileSizeMinimumKb: 0` ikut ABDM (tangkap semua). Filter naikkan belakangan bila noise.
- Validasi port di UI: `1024-65535` (ikut `MIN_ALLOWED_PORT`/`MAX_ALLOWED_PORT` ABDM).

## 2. Template Saat Ini vs Target

Ada (starter):

```text
wxt.config.ts            # hanya modules: ['@wxt-dev/module-vue']
entrypoints/background.ts # hello-world
entrypoints/content.ts    # matches google.com saja
entrypoints/popup/        # App.vue + index.html + main.ts
package.json scripts      # dev, dev:firefox, build, build:firefox, zip, zip:firefox
```

Kurang (harus dibangun):

```text
entrypoints/background.ts      # orchestrator: menu, intercept, backend calls
entrypoints/content.ts         # detector/delegate: ekstrak link + seleksi + bypass key
entrypoints/popup/             # status koneksi, port+token, toggle cepat, tes ping
entrypoints/options/           # port, token, filetypes, blacklist, silent flags, min size
utils/backend.ts               # fetch wrapper + timeout + error mapping
utils/config.ts                # wxt/storage schema + defaults + migrasi
utils/capture.ts               # shouldHandle: filetype/blacklist/min-size/autoCapture
assets/icons/*, public/_locales/* # branding + i18n
```

## 3. Kontrak API Desktop (mengikat `core::integration` + `axum`)

Base: `http://127.0.0.1:35343/`. Bind `127.0.0.1` saja, jangan `0.0.0.0`.

| Method + Path | Auth | Body | Perilaku |
|---|---|---|---|
| `POST /ping` | tidak | `null` | health check, selalu `200` bila server hidup |
| `POST /add` | Bearer wajib | `AddDownloadRequest` | angkat GPUI confirm dialog; `silentAdd` lewati dialog |
| `POST /start-headless-download` | Bearer wajib | sama dengan `/add` | langsung enqueue tanpa dialog |
| `GET /queues` | Bearer wajib | — | daftar queue (Main `id 0` saat M3 aktif) |
| `GET /downloads` | Bearer wajib | — | ekstensi MVP, list ringkas untuk popup |

Payload (serde harus cocok kedua sisi):

```ts
interface AddDownloadRequest {
  items: DownloadRequestItem[];
  options: DownloadRequestOptions;
}
interface DownloadRequestItem {
  link: string;
  downloadPage: string | null;
  headers: Record<string, string> | null;
  description: string | null;
  suggestedName: string | null;
  type: "http" | "hls"; // kirim "hls" hanya bila server capai M9
}
interface DownloadRequestOptions {
  silentAdd: boolean;
  silentStart: boolean;
}
```

Aturan server:

- Timeout client: `ping` 2000ms (ikut ABDM), `add` 8000ms.
- `401` untuk token hilang/salah. `400` untuk payload buruk. `500` tanpa stack trace.
- `POST /add` non-silent wajib dialog GPUI; jangan auto-start diam-diam.
- Server default OFF; aktif via Settings desktop. Extension tampilkan "server mati" bukan "token salah" bila `fetch` network-error.

## 4. Storage Schema (`wxt/storage`, `local:` semua)

```ts
port: number = 35343
token: string = ""              // Bearer, local only
autoCaptureLinks = true
popupEnabled = true
sendHeaders = true
registeredFileTypes: string[] = [zip,rar,7z,iso,tar,gz,exe,msi,deb,jar,apk,bin,mp3,aac,pdf,mp4,3gp,avi,mkv,wav,mpeg,srt]
blacklistedUrls: string[] = []
allowPassDownloadIfAppNotRespond = true
closeNewTabIfItWasCaptured = true
silentAddDownload = false
silentStartDownload = false
captureFileSizeMinimumKb = 0
bypassShortcut = "Delete"
```

Aturan: satu penulis per key (options UI), background/popup baca reaktif. Migrasi versi tambah `configVersion` bila schema berubah.

## 5. Messaging Internal (native, typed)

Background owns I/O. Content/popup tidak `fetch` langsung ke desktop.

```ts
"add_download"(items: DownloadRequestItem[]) => boolean
"test_port"(port: number) => boolean
"get_headers"(urls: string[]) => (Record<string,string> | null)[]
```

`get_headers` bangun per URL: `Cookie` dari `browser.cookies.getAll({url})`, `User-Agent` dari `navigator.userAgent`, `Referer`/`Host` bila perlu. Hormati `sendHeaders=false` → kembalikan `null` tanpa baca cookies.

## 6. Auto-Capture Delegate (MVP termasuk)

Peran tegas:

- `content.ts` = detector: kumpulkan `<a href>`, link terpilih, media kandidat; kirim ke background + dukung tombol bypass (`Delete` default, tahan untuk lewati capture sekali).
- `background.ts` = orchestrator: cek `autoCaptureLinks`, `blacklistedUrls` (wildcard match), ekstensi vs `registeredFileTypes`, ukuran minimum (bila diketahui), `popupEnabled`; `ping` dulu bila perlu; panggil `add_download`; putuskan cancel download browser vs biarkan lanjut.

Fallback MV3 (tanpa `webRequestBlocking`):

- Jangan janji block sinkron seperti MV2 Firefox lama.
- Pola: dengar `browser.downloads.onCreated`, cocokkan URL pending, `cancel` + `erase` bila akan ditangani Raijin; bila `ping` gagal dan `allowPassDownloadIfAppNotRespond=true`, biarkan browser lanjut + beri notifikasi sekali.
- `closeNewTabIfItWasCaptured=true` → tutup tab kosong sisa capture.

Yang tidak ikut MVP: HLS/media registry, queue picker (M3 ON HOLD), PAC/proxy, checksum.

## 7. Permissions (minimal dulu, perluas bila perlu)

Mulai: `contextMenus`, `storage`, `tabs`, `downloads`, `cookies` (wajib untuk `sendHeaders`), `notifications` (hanya silent-capture). Host: `*://*/*` hanya bila auto-capture global aktif; jika review keberatan, turunkan ke `activeTab` + opt-in host. Tulis justifikasi tiap permission untuk store + teks privasi sebelum publish (gate M5/M7).

## 8. Token Provisioning UX

1. Desktop Settings → Integration: tampilkan `port`, tombol `Regenerate`, `Copy`, status server ON/OFF.
2. Popup ext: field `port` + `token` (password input) + tombol `Test` (`test_port`) + status hijau/merah + pesan bedakan "server mati" vs "token ditolak".
3. Jangan simpan token di `sync`, log, atau screenshot. Sediakan `Clear token`.

## 9. Build, Test, Rilis

- Dev: `pnpm dev` (Chrome), `pnpm dev:firefox`.
- Build: `pnpm build`, `pnpm build:firefox`. Zip: `pnpm zip`, `pnpm zip:firefox`.
- `vue-tsc --noEmit` (script `compile`) wajib hijau sebelum zip.
- Matrix uji: Chrome MV3 + Firefox (MV2/MV3 ikut default `wxt`) × server ON/OFF × token benar/salah × `sendHeaders` on/off × blacklist × bypass key.
- E2E: ext → `127.0.0.1:35343` → dialog GPUI muncul → download jalan → pause/resume/remove.
- Store publish hanya setelah M5 polish + privacy text. Native-messaging manifest installer tunda setelah publish (ikut `plan.md` M7).

## 10. Checklist Porting dari ABDM (selektif)

Bawa: kontrak `AddDownloadRequest`, semantik `ping/add`, defaults config, aturan `DuplicateFilter`/nama file sebagai spec desktop, UX context-menu + selected-links popup, `HeaderUtils`/`URLUtils`/`LinkExtractor` logika murni.

Buang: React/mobx/daisyui UI, `@samrum/vite-plugin-web-extension`, cabang manifest MV2/MV3 manual, `webRequestBlocking`, `hls-parser`/media registry (tunda M9), kode komen mati interceptor MV3.

## 11. Open / Risiko

- Port `35343` final? Perlu cek konflik lokal (`netstat`) di 3 OS + dokumentasi migrasi dari `15151`.
- Header final tunggal `Authorization: Bearer` — pastikan middleware `axum` terima format ini persis.
- Bila store tolak `cookies` + host lebar, siap fallback `activeTab`-only untuk rilis pertama.
