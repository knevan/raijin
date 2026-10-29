import { defineConfig } from 'wxt';

const bravePath =
  process.platform === 'win32'
    ? `${process.env.PROGRAMFILES}/BraveSoftware/Brave-Browser-Beta/Application/brave.exe`
    : '/usr/bin/brave-browser-beta';

// See https://wxt.dev/api/config.html
export default defineConfig({
  modules: ['@wxt-dev/module-vue'],
  webExt: {
    binaries: {
      chrome: bravePath
    }
  },
  manifest: ({ browser }) => ({
    name: 'Raijin Browser Integration',
    description: 'Capture browser downloads and send them to the Raijin download manager.',
    default_locale: 'en',
    permissions: ['contextMenus', 'storage', 'tabs', 'downloads', 'webRequest', 'cookies', 'notifications'],
    host_permissions: ['*://*/*'],
    icons: {
      16: 'icon/16.png',
      32: 'icon/32.png',
      48: 'icon/48.png',
      96: 'icon/96.png',
      128: 'icon/128.png',
    },
    // Firefox needs a stable add-on ID for signing and declares the data it
    // forwards to the local Raijin app for built-in consent on AMO.
    ...(browser === 'firefox'
      ? {
          browser_specific_settings: {
            gecko: {
              id: 'raijin-browser@raijin.app',
              strict_min_version: '140.0',
              data_collection_permissions: {
                required: ['browsingActivity', 'websiteContent'],
              },
            },
            gecko_android: {
              strict_min_version: '142.0',
            },
          },
        }
      : {}),
  }),
});
