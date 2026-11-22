// ============================================================
// FILE: shell-preload.js
// What this file does: bridges the tab bar (shell.html) to the
// main process — tab switching, closing, and toggling the
// floating Settings view, plus live updates when tabs or
// settings change.
// ============================================================
const { contextBridge, ipcRenderer } = require('electron');

contextBridge.exposeInMainWorld('electronAPI', {
  switchTab: (id) => ipcRenderer.send('switch-tab', id),
  setSidebarWidth: (w) => ipcRenderer.send('set-sidebar-width', w),
  closeTab: (id) => ipcRenderer.send('close-tab', id),
  getOpenTabs: () => ipcRenderer.invoke('get-open-tabs'),

  // Settings is its own floating view above the tabs now —
  // the gear opens Settings, the + opens the tab list.
  toggleSettings: (mode) => ipcRenderer.send('toggle-settings', mode),

  // Main process tells the tab bar when settings change
  // (theme, sidebar position) or when a tab opens/closes.
  onApplySettings: (cb) => ipcRenderer.on('apply-settings', (e, data) => cb(data)),
  onTabOpened: (cb) => ipcRenderer.on('tab-opened', (e, data) => cb(data)),
  onTabClosed: (cb) => ipcRenderer.on('tab-closed', (e, id) => cb(id)),
});
