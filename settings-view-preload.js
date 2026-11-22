// ============================================================
// FILE: settings-view-preload.js
// What this file does: bridges the floating Settings view
// (settings-view.html) to the main process.
// ============================================================
const { contextBridge, ipcRenderer } = require('electron');

contextBridge.exposeInMainWorld('electronAPI', {
  getSettings: () => ipcRenderer.invoke('get-settings'),
  addTab: (id) => ipcRenderer.send('settings-add-tab', id),
  removeTab: (id) => ipcRenderer.send('settings-remove-tab', id),
  addCustomTab: (name, url) => ipcRenderer.send('settings-add-custom-tab', { name, url }),
  removeCustomTab: (id) => ipcRenderer.send('settings-remove-custom-tab', id),
  setSidebarPosition: (pos) => ipcRenderer.send('settings-set-sidebar-position', pos),
  setTheme: (theme) => ipcRenderer.send('settings-set-theme', theme),
  close: () => ipcRenderer.send('settings-close'),

  // Main process sends a fresh settings payload whenever
  // something changes (tab opened/closed, theme, position).
  onSettingsUpdated: (cb) => ipcRenderer.on('settings-updated', (e, data) => cb(data)),
});
