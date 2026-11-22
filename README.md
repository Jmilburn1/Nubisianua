# Nubisianua

Nubisianua — Latin *nubis* (cloud) + *ianua* (doorway): the Cloud's Doorway.

A lightweight Electron desktop wrapper that loads AI chat apps into tabs, each with its own isolated sign-in. Muse is the resident, front and center; every other AI is a guest.

This is a complete rebuild from scratch. If you still have to Original Nubisianua, You should uninstall it and use this one instead.

## Tabs

- **AI Assistants:** Muse, Meta AI, Claude, ChatGPT, Gemini, Copilot, Grok, Perplexity, Qwen, Kimi, Vibe, Poe, Pi, HuggingChat
- **Companion Chats:** DeepSeek, Vidy, Kindroid, Nomi
- **Video & Content:** FlixAI
- Plus your own custom AIs, added by name and URL.

Tabs close three ways (hover ×, right-click, middle-click), closed tabs stay closed across restarts, and the sidebar resizes and docks left, right, or on top. Settings (gear) and Add tabs (+) open as in-app panels.

## Run it

```bash
npm install
npm start
```

## Build it

```bash
npm run dist
```

Produces a portable Windows exe and a Linux AppImage. Unsigned builds will show a SmartScreen warning on first launch on Windows — More info, then Run anyway. Run it normally, not as Administrator, so drag-and-drop keeps working.

## Version

0.1.0 — first official release.
