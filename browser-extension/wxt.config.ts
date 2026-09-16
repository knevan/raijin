import { defineConfig } from 'wxt';

// See https://wxt.dev/api/config.html
export default defineConfig({
  modules: ['@wxt-dev/module-vue'],
  manifest: {
    name: 'Raijin Browser Integration',
    description: 'Capture browser downloads and send them to the Raijin download manager.',
    default_locale: 'en',
    permissions: ['contextMenus', 'storage', 'tabs', 'downloads', 'cookies', 'notifications'],
    host_permissions: ['*://*/*'],
    icons: {
      16: 'icon/16.png',
      32: 'icon/32.png',
      48: 'icon/48.png',
      96: 'icon/96.png',
      128: 'icon/128.png',
    },
  },
});
