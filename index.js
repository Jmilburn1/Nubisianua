// ============================================================
// FILE: index.js
// What this file does: starts the app, makes the main window,
// loads the tab bar, creates the tabs, and runs Settings.
// ============================================================

// ---------- SECTION: SETUP ----------
const { app, BrowserWindow, shell, ipcMain, WebContentsView } = require('electron');
const path = require('path');
const fs = require('fs');
const { TabManager } = require('./tabs');

let mainWindow;
let tabs = null;
let settingsView = null;
let settingsVisible = false;
let currentMode = 'settings';

// ---------- SECTION: TAB DEFINITIONS ----------
// Every AI the app knows about. The order here is the order
// they appear in the tab bar on a fresh install.
// Categories: 'assistants' (AI chatbots), 'companions'
// (Companion Chats), 'media' (video & content).
const TAB_DEFS = {
  muse:        { url: 'https://muse.ai',                 name: 'Muse',        category: 'assistants' },
  meta:        { url: 'https://meta.ai',                 name: 'Meta AI',     category: 'assistants' },
  deepseek:    { url: 'https://chat.deepseek.com',       name: 'DeepSeek',    category: 'companions' },
  claude:      { url: 'https://claude.ai',               name: 'Claude',      category: 'assistants' },
  chatgpt:     { url: 'https://chatgpt.com',             name: 'ChatGPT',     category: 'assistants' },
  gemini:      { url: 'https://gemini.google.com',       name: 'Gemini',      category: 'assistants' },
  copilot:     { url: 'https://copilot.microsoft.com',   name: 'Copilot',     category: 'assistants' },
  grok:        { url: 'https://grok.com',                name: 'Grok',        category: 'assistants' },
  perplexity:  { url: 'https://www.perplexity.ai',       name: 'Perplexity',  category: 'assistants' },
  qwen:        { url: 'https://chat.qwen.ai',            name: 'Qwen',        category: 'assistants' },
  kimi:        { url: 'https://kimi.ai',                 name: 'Kimi',        category: 'assistants' },
  lechat:      { url: 'https://chat.mistral.ai',         name: 'Vibe',        category: 'assistants' },
  poe:         { url: 'https://poe.com',                 name: 'Poe',         category: 'assistants' },
  pi:          { url: 'https://pi.ai',                   name: 'Pi',          category: 'assistants' },
  huggingchat: { url: 'https://huggingface.co/chat',     name: 'HuggingChat', category: 'assistants' },
  vidy:        { url: 'https://www.vidy.chat',           name: 'Vidy',        category: 'companions' },
  kindroid:    { url: 'https://kindroid.ai',             name: 'Kindroid',    category: 'companions' },
  nomi:        { url: 'https://nomi.ai',                 name: 'Nomi',        category: 'companions' },
  flixai:      { url: 'https://flixai.com',              name: 'FlixAI',      category: 'media' },
};

const CATEGORY_NAMES = {
  assistants: 'AI Assistants',
  companions: 'Companion Chats',
  media: 'Video & Content',
  custom: 'Custom',
};

// ---------- SECTION: SETTINGS STORAGE ----------
// Settings live in one file: sidebar position, theme,
// which tabs are closed, and any custom tabs the user added.
// Version 2: the Settings panel lives inside the app (no more
// pop-up window), and a fresh start opens Muse only.
const SETTINGS_VERSION = 2;

function settingsPath() {
  return path.join(app.getPath('userData'), 'settings.json');
}

function defaultSettings() {
  return {
    version: SETTINGS_VERSION,
    sidebarPosition: 'right',
    theme: 'dark',
    // Everything except Muse starts closed — Muse only on launch.
    closedTabs: Object.keys(TAB_DEFS).filter((id) => id !== 'muse'),
    customTabs: [],
  };
}

function loadSettings() {
  try {
    const saved = JSON.parse(fs.readFileSync(settingsPath(), 'utf-8'));
    // Only v2 settings count. Anything older (or missing) starts
    // fresh: Muse only, the rest waiting in Settings.
    if (saved.version !== SETTINGS_VERSION) return defaultSettings();
    const settings = defaultSettings();
    if (['right', 'left', 'top'].includes(saved.sidebarPosition)) {
      settings.sidebarPosition = saved.sidebarPosition;
    }
    if (['dark', 'light'].includes(saved.theme)) settings.theme = saved.theme;
    if (Array.isArray(saved.closedTabs)) settings.closedTabs = saved.closedTabs;
    if (Array.isArray(saved.customTabs)) settings.customTabs = saved.customTabs;
    return settings;
  } catch (e) {
    // No settings file yet — fresh install, Muse only.
    return defaultSettings();
  }
}

function saveSettings(settings) {
  try {
    fs.writeFileSync(settingsPath(), JSON.stringify(settings, null, 2));
  } catch (e) {
    // Saving failed — settings just won't stick this time.
  }
}

// All known tab ids: built-ins plus any custom tabs.
function allTabIds(settings) {
  const ids = Object.keys(TAB_DEFS);
  (settings?.customTabs || []).forEach((t) => ids.push(t.id));
  return ids;
}

// Look up a tab's info by id (built-in or custom).
function tabInfo(id, settings) {
  if (TAB_DEFS[id]) return TAB_DEFS[id];
  const custom = (settings?.customTabs || []).find((t) => t.id === id);
  if (custom) return { url: custom.url, name: custom.name, category: 'custom' };
  return null;
}

// ---------- SECTION: MAIN WINDOW ----------
function createWindow() {
  const settings = loadSettings();

  mainWindow = new BrowserWindow({
    width: 1280,
    height: 800,
    title: 'Nubisianua',
    icon: path.join(__dirname, 'icon.png'),
    autoHideMenuBar: true,
    webPreferences: {
      nodeIntegration: false,
      contextIsolation: true,
      preload: path.join(__dirname, 'shell-preload.js'),
    },
  });

  // ----- Load the tab bar UI into THIS window -----
  mainWindow.loadFile('shell.html');

  // ---------- SECTION: TABS ----------
  // Tabs live on this window, not a second one
  tabs = new TabManager(mainWindow, {
    sidebarWidth: 200,
    position: settings.sidebarPosition,
  });

  // Only create the tabs that are still open (closed ones stay closed).
  const openTabs = allTabIds(settings).filter((id) => !settings.closedTabs.includes(id));
  openTabs.forEach((id) => {
    const info = tabInfo(id, settings);
    if (info) tabs.createTab(id, info.url);
  });

  // Open on Muse if it's still there, otherwise the first open tab.
  tabs.switchTo(openTabs.includes('muse') ? 'muse' : openTabs[0]);

  // Tell the tab bar which theme and position to use.
  mainWindow.webContents.on('did-finish-load', () => {
    mainWindow.webContents.send('apply-settings', {
      theme: settings.theme,
      sidebarPosition: settings.sidebarPosition,
    });
  });

  // ---------- SECTION: MESSAGES FROM THE TAB BAR ----------
  // Switch tabs: when you click a tab button
  ipcMain.on('switch-tab', (event, id) => {
    hideSettingsView();
    tabs.switchTo(id);
  });

  // Toggle the panel: the gear button opens Settings (themes and
  // sidebar position), the + button opens the tab list.
  ipcMain.on('toggle-settings', (event, mode) => {
    toggleSettingsView(mode === 'tabs' ? 'tabs' : 'settings');
  });

  // Keep the Settings view placed when the window is resized.
  mainWindow.on('resize', () => {
    if (settingsVisible) layoutSettingsView();
  });

  // Sidebar width: when you drag the resize handle
  ipcMain.on('set-sidebar-width', (event, w) => {
    tabs.setSidebarWidth(w);
    if (settingsVisible) layoutSettingsView();
  });

  // Close tab: when you click the ×, middle-click, or right-click Close
  ipcMain.on('close-tab', (event, id) => {
    closeTabById(id);
  });

  // Open tabs list: the tab bar asks which buttons to show
  ipcMain.handle('get-open-tabs', () => tabs.getOpenTabIds());

  mainWindow.on('closed', () => {
    mainWindow = null;
  });
}

// ---------- SECTION: TAB ADD / REMOVE ----------
// Close a tab and remember it as closed.
// (The panel stays open — no new view is created, so it stays on top.)
function closeTabById(id) {
  if (!tabs) return;
  tabs.closeTab(id);
  const settings = loadSettings();
  if (!settings.closedTabs.includes(id)) settings.closedTabs.push(id);
  saveSettings(settings);
  mainWindow?.webContents.send('tab-closed', id);
  sendSettingsUpdated();
}

// Open (or re-open) a tab by id.
function openTabById(id) {
  if (!tabs) return;
  const settings = loadSettings();
  const info = tabInfo(id, settings);
  if (!info) return;
  if (tabs.getOpenTabIds().includes(id)) {
    hideSettingsView();
    tabs.switchTo(id);
    return;
  }
  tabs.createTab(id, info.url);
  settings.closedTabs = settings.closedTabs.filter((c) => c !== id);
  saveSettings(settings);
  mainWindow?.webContents.send('tab-opened', { id, name: info.name, logo: !!TAB_DEFS[id] });
  // The new tab view was added after the panel — bring the panel
  // back to the front so it stays visible.
  bringSettingsViewToFront();
}

// Add a custom tab (name + URL from Settings).
function addCustomTab(name, url) {
  const settings = loadSettings();
  const cleanName = (name || '').trim() || 'Custom';
  let cleanUrl = (url || '').trim();
  if (!/^https?:\/\//i.test(cleanUrl)) cleanUrl = 'https://' + cleanUrl;
  const id = 'custom-' + cleanName.toLowerCase().replace(/[^a-z0-9]+/g, '-') + '-' + Date.now().toString(36);
  settings.customTabs.push({ id, name: cleanName, url: cleanUrl });
  saveSettings(settings);
  openTabById(id);
  return id;
}

// Remove a custom tab entirely (closes it if open, forgets it).
function removeCustomTab(id) {
  const settings = loadSettings();
  settings.customTabs = settings.customTabs.filter((t) => t.id !== id);
  settings.closedTabs = settings.closedTabs.filter((c) => c !== id);
  saveSettings(settings);
  if (tabs && tabs.getOpenTabIds().includes(id)) tabs.closeTab(id);
  mainWindow?.webContents.send('tab-closed', id);
  sendSettingsUpdated();
}

// ---------- SECTION: SETTINGS VIEW ----------
// Settings floats ABOVE the tabs as its own view — a panel in
// the tab bar itself sits behind the chat tab and can't be seen.
// The view is created once, then shown/hidden. It is always
// re-added on show so it stays on top of any tab views.
function layoutSettingsView() {
  if (!settingsView || !mainWindow) return;
  // The view covers the whole window with a transparent
  // background — the Settings card floats inside it, and a
  // click anywhere outside the card closes the menu.
  // Content bounds, not window bounds: the outer bounds include
  // the title bar, which made the view taller than the visible
  // area and pushed the Close button under the taskbar.
  const win = mainWindow.getContentBounds();
  settingsView.setBounds({ x: 0, y: 0, width: win.width, height: win.height });
}

function showSettingsView() {
  if (!mainWindow) return;
  if (!settingsView) {
    settingsView = new WebContentsView({
      webPreferences: {
        nodeIntegration: false,
        contextIsolation: true,
        preload: path.join(__dirname, 'settings-view-preload.js'),
      },
    });
    // Transparent so the chat shows through around the card.
    settingsView.setBackgroundColor('#00000000');
    settingsView.webContents.loadFile('settings-view.html');
  }
  // Re-add to bring it to the front of the tab views.
  mainWindow.contentView.removeChildView(settingsView);
  mainWindow.contentView.addChildView(settingsView);
  layoutSettingsView();
  settingsView.setVisible(true);
  settingsVisible = true;
}

function hideSettingsView() {
  if (settingsView) settingsView.setVisible(false);
  settingsVisible = false;
}

function toggleSettingsView(mode) {
  // Same button again closes it; the other button switches panels.
  if (settingsVisible && currentMode === mode) {
    hideSettingsView();
    return;
  }
  currentMode = mode;
  showSettingsView();
  sendSettingsUpdated();
}

// Re-add the view so it stays above any tab view created after it
// (adding a tab must not bury the panel behind the new tab).
function bringSettingsViewToFront() {
  if (!settingsView || !mainWindow || !settingsVisible) return;
  mainWindow.contentView.removeChildView(settingsView);
  mainWindow.contentView.addChildView(settingsView);
  layoutSettingsView();
}

// Send a fresh settings payload to the view when it is open.
function sendSettingsUpdated() {
  if (settingsView && settingsVisible) {
    settingsView.webContents.send('settings-updated', buildSettingsPayload());
  }
}

// ---------- SECTION: SETTINGS ----------
function buildSettingsPayload() {
  const settings = loadSettings();
  const openIds = tabs ? tabs.getOpenTabIds() : [];
  const list = [];
  Object.keys(TAB_DEFS).forEach((id) => {
    list.push({ ...TAB_DEFS[id], id, open: openIds.includes(id), custom: false });
  });
  settings.customTabs.forEach((t) => {
    list.push({ id: t.id, name: t.name, url: t.url, category: 'custom', open: openIds.includes(t.id), custom: true });
  });
  return {
    tabs: list,
    categories: CATEGORY_NAMES,
    sidebarPosition: settings.sidebarPosition,
    sidebarWidth: tabs ? tabs.sidebarWidth : 200,
    theme: settings.theme,
    panel: currentMode,
  };
}

// ---------- SECTION: MESSAGES FROM SETTINGS ----------
ipcMain.handle('get-settings', () => buildSettingsPayload());

ipcMain.on('settings-add-tab', (event, id) => {
  openTabById(id);
});

ipcMain.on('settings-remove-tab', (event, id) => {
  closeTabById(id);
});

ipcMain.on('settings-add-custom-tab', (event, { name, url }) => {
  addCustomTab(name, url);
});

ipcMain.on('settings-remove-custom-tab', (event, id) => {
  removeCustomTab(id);
});

ipcMain.on('settings-set-sidebar-position', (event, pos) => {
  if (!['right', 'left', 'top'].includes(pos)) return;
  const settings = loadSettings();
  settings.sidebarPosition = pos;
  saveSettings(settings);
  if (tabs) tabs.setSidebarPosition(pos);
  mainWindow?.webContents.send('apply-settings', {
    theme: settings.theme,
    sidebarPosition: pos,
  });
  if (settingsVisible) layoutSettingsView();
  sendSettingsUpdated();
});

ipcMain.on('settings-set-theme', (event, theme) => {
  if (!['dark', 'light'].includes(theme)) return;
  const settings = loadSettings();
  settings.theme = theme;
  saveSettings(settings);
  mainWindow?.webContents.send('apply-settings', {
    theme,
    sidebarPosition: settings.sidebarPosition,
  });
  sendSettingsUpdated();
});

ipcMain.on('settings-close', () => {
  hideSettingsView();
});

// ---------- SECTION: APP STARTUP ----------
app.whenReady().then(createWindow);

// Quit when all windows are closed (except on macOS)
app.on('window-all-closed', () => {
  if (process.platform !== 'darwin') {
    app.quit();
  }
});

app.on('activate', () => {
  if (BrowserWindow.getAllWindows().length === 0) {
    createWindow();
  }
});
