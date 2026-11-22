// ============================================================
// FILE: tabs.js
// What this file does: manages the tabs (one per AI agent),
// each with its own isolated sign-in.
// ============================================================

// ---------- SECTION: SETUP ----------
const { WebContentsView, shell, Menu } = require('electron');
const path = require('path');

// ---------- SECTION: TAB MANAGER ----------
class TabManager {

  // ----- Constructor: runs once when the app starts -----
  // position: 'right' (default), 'left', or 'top' — where the tab bar sits.
  constructor(win, { sidebarWidth = 200, position = 'right' } = {}) {
    this.win = win;
    this.sidebarWidth = sidebarWidth;  // sidebar width (left/right) or bar height (top)
    this.position = position;

    this.tabs = new Map();         // tabId -> { view, partition }
    this.activeId = null;

    win.on('resize', () => this.layout());
  }

  // ----- Sidebar width: called when you drag the resize handle -----
  setSidebarWidth(w) {
    this.sidebarWidth = w;
    this.layout();
  }

  // ----- Sidebar position: right, left, or top -----
  setSidebarPosition(pos) {
    this.position = pos;
    this.layout();
  }

  // ----- Create a tab: makes a new view with its own sign-in -----
  createTab(id, url) {
    // Unique partition per tab *instance*, not per site.
    const partition = `persist:tab-${id}`;

    const view = new WebContentsView({
      webPreferences: {
        partition,                  // isolated cookies, localStorage, IndexedDB, cache
        contextIsolation: true,
        sandbox: true,
        nodeIntegration: false,
        preload: path.join(__dirname, 'preload.js'),
      },
    });

    // ----- Auth domains: sites allowed to open inside the app -----
    const AUTH_DOMAINS = [
      'https://accounts.google.com',
      'https://github.com',
      'https://appleid.apple.com',
      'https://login.microsoftonline.com',
      'https://www.facebook.com',
      'https://facebook.com',
      'https://x.com',
      'https://twitter.com',
    ];

    // ----- New windows: allow the tab's own site + auth sites, send the rest to the browser -----
    // The tab's own origin (e.g. https://claude.ai) is allowed so its own links stay in-app.
    const tabOrigin = new URL(url).origin;
    view.webContents.setWindowOpenHandler(({ url: newUrl }) => {
      if (newUrl.startsWith(tabOrigin) || AUTH_DOMAINS.some((d) => newUrl.startsWith(d))) {
        return { action: 'allow' };
      }
      shell.openExternal(newUrl);
      return { action: 'deny' };
    });

    // ----- Right-click menu: Copy / Paste / Select All / Close Tab -----
    view.webContents.on('context-menu', (event, params) => {
      const menu = Menu.buildFromTemplate([
        { label: 'Copy', role: 'copy', enabled: params.editFlags.canCopy },
        { label: 'Paste', role: 'paste', enabled: params.editFlags.canPaste },
        { label: 'Select All', role: 'selectAll' },
        { type: 'separator' },
        { label: 'Close Tab', click: () => this.closeTab(id) },
      ]);
      menu.popup();
    });

    view.webContents.loadURL(url);
    this.tabs.set(id, { view, partition });
    this.switchTo(id);
    return view;
  }

  // ----- Switch tabs: hides the old one, shows the new one -----
  switchTo(id) {
    const next = this.tabs.get(id);
    if (!next) return;

    // Hide the current tab but keep it alive; don't destroy it.
    if (this.activeId && this.tabs.has(this.activeId)) {
      this.tabs.get(this.activeId).view.setVisible(false);
    }

    // addChildView on an existing child just reorders it to the top.
    this.win.contentView.addChildView(next.view);
    next.view.setVisible(true);
    this.activeId = id;
    this.layout();
    next.view.webContents.focus();
  }

  // ----- Layout: sizes the active view around the tab bar -----
  layout() {
    const active = this.tabs.get(this.activeId);
    if (!active) return;
    const { width, height } = this.win.getContentBounds();
    if (this.position === 'top') {
      // Tab bar is a horizontal strip across the top.
      const barHeight = 40;
      active.view.setBounds({ x: 0, y: barHeight, width, height: height - barHeight });
    } else if (this.position === 'left') {
      active.view.setBounds({
        x: this.sidebarWidth,
        y: 0,
        width: width - this.sidebarWidth,
        height,
      });
    } else {
      // Right (default): bar on the right, view on the left.
      active.view.setBounds({
        x: 0,
        y: 0,
        width: width - this.sidebarWidth,
        height,
      });
    }
  }

  // ----- Close a tab -----
  closeTab(id) {
    const tab = this.tabs.get(id);
    if (!tab) return;
    this.win.contentView.removeChildView(tab.view);
    tab.view.webContents.close();   // views are not GC'd automatically
    this.tabs.delete(id);
    if (this.activeId === id) {
      this.activeId = null;
      const [nextId] = this.tabs.keys();
      if (nextId) this.switchTo(nextId);
    }
  }

  // ----- Open tab ids: which tabs are still open (for saving) -----
  getOpenTabIds() {
    return Array.from(this.tabs.keys());
  }

  // ----- Clear tab data: "Sign out and wipe this tab's data" -----
  async clearTabData(id) {
    const tab = this.tabs.get(id);
    if (!tab) return;
    const ses = tab.view.webContents.session;
    await ses.clearStorageData();   // cookies, localStorage, IndexedDB, etc.
    await ses.clearCache();
  }
}

// ---------- SECTION: EXPORTS ----------
module.exports = { TabManager };
