/**
 * Dockview content renderer for one tmux session: an xterm.js terminal wired to
 * /ws/attach, with copy/paste, image upload, links, a context menu and a
 * not-connected overlay (CONTRACT §8.6 items 5-13).
 *
 * Lifecycle notes
 *   - Dockview moves a panel between groups without re-creating this object, so init()
 *     is idempotent and a panel never opens a second socket; dispose() closes the
 *     socket and the terminal.
 *   - The terminal is opened (and the socket connected) the first time the panel is
 *     visible with a non-zero size, so a tab that has never been shown does not attach
 *     with a bogus size.
 */
import { Terminal } from '../vendor/xterm/xterm.mjs';
import { FitAddon } from '../vendor/addon-fit/addon-fit.mjs';
import { WebLinksAddon } from '../vendor/addon-web-links/addon-web-links.mjs';
import { ClipboardAddon } from '../vendor/addon-clipboard/addon-clipboard.mjs';
import { WebglAddon } from '../vendor/addon-webgl/addon-webgl.mjs';
import { Unicode11Addon } from '../vendor/addon-unicode11/addon-unicode11.mjs';

import { AttachConnection, RECOVERABLE_REASONS } from './attach-connection.js';
import { canReadClipboard, copyText, osc52Provider, readClipboardText } from './clipboard.js';
import { debounce, el, errorMessage } from './dom.js';
import { EventHub } from './emitter.js';
import { dragHasFiles, imagesFromClipboard, pickImages, uploadImages } from './image-upload.js';
import { PanelOverlay } from './panel-overlay.js';
import { createKeyHandler } from './terminal-keys.js';
import { createLinkHandlers } from './terminal-links.js';
import { FONT_FAMILY, TERMINAL_THEME } from './terminal-theme.js';
import { toast } from './toast.js';
import { installTouchScroll } from './touch-scroll.js';

const COPY_ON_SELECT_DELAY_MS = 200;
/** Animation frames a focus request waits for a just-activated tab to become visible. */
const FOCUS_RETRY_FRAMES = 30;
const encoder = new TextEncoder();

/** Latin-1 string from xterm's onBinary event -> bytes 0..255. */
function binaryStringToBytes(text) {
  const bytes = new Uint8Array(text.length);
  for (let i = 0; i < text.length; i += 1) bytes[i] = text.charCodeAt(i) & 0xff;
  return bytes;
}

export class TerminalPanel {
  #attention = false;

  /**
   * @param {object} services app services (api, store, persistence, workspace, registry, picker, ...)
   * @param {string} id dockview panel id
   */
  constructor(services, id) {
    this.services = services;
    this.id = id;
    this.params = {};
    this.title = '';
    this.panelApi = null;
    this.initialised = false;
    this.disposed = false;
    this.opened = false;

    /** @type {import('../vendor/xterm/xterm.mjs').Terminal|null} */
    this.term = null;
    this.fit = null;
    this.webgl = null;
    this.lastSent = null;
    this.layoutFrame = 0;
    this.layoutFramesLeft = 0;
    this.disposables = [];
    /** Emits "state" whenever the connection state changes, "attention" when the flag flips. */
    this.events = new EventHub();

    this.element = el('div', { class: 'terminal-panel', testid: 'terminal-panel', data: { panelId: id } });
    this.host = el('div', { class: 'terminal-host' });
    this.overlay = new PanelOverlay({
      onRecreate: () => void this.#recreateSession(),
      onClose: () => this.panelApi?.close(),
      onReattach: () => this.conn.retryNow(),
      onRetry: () => this.conn.retryNow(),
    });
    this.element.append(this.host, this.overlay.element);

    this.conn = new AttachConnection({
      api: services.api,
      getSession: () => this.session,
      getSize: () => this.#sizeForConnect(),
    });
    this.conn.on('state', (info) => this.#onConnectionState(info));
    this.conn.on('open', () => this.#onSocketOpen());
    this.conn.on('output', (bytes) => this.term?.write(bytes));

    services.registry.set(id, this);
  }

  // ---- dockview content renderer interface ------------------------------------

  /** @param {{params: object, title: string, api: any, containerApi: any}} parameters */
  init(parameters) {
    if (this.initialised) {
      this.update({ params: parameters.params ?? {} });
      return;
    }
    this.initialised = true;
    this.params = { ...(parameters.params ?? {}) };
    this.title = parameters.title ?? '';
    this.panelApi = parameters.api;
    const { store, persistence } = this.services;

    this.disposables.push(
      this.panelApi.onDidVisibilityChange(() => this.#scheduleLayout(2)),
      this.panelApi.onDidDimensionsChange(() => this.#scheduleLayout()),
      this.panelApi.onDidActiveChange((event) => {
        if (event.isActive) this.#focusSoon();
        this.#clearAttentionIfFocused();
      }),
      this.panelApi.onDidActiveGroupChange(() => this.#clearAttentionIfFocused()),
      this.panelApi.onDidTitleChange((event) => {
        this.title = event.title;
      }),
    );

    const unsubscribers = [
      store.on('status', (status) => this.#onStatus(status)),
      persistence.on('prefs', (prefs, key) => this.#onPrefsChanged(prefs, key)),
    ];
    this.disposables.push(...unsubscribers.map((off) => ({ dispose: off })));

    const observer = new ResizeObserver(() => this.#scheduleLayout());
    observer.observe(this.element);
    this.disposables.push({ dispose: () => observer.disconnect() });

    // The page gaining focus (window focus, tab becoming visible) can make this panel focused.
    const recheckAttention = () => this.#clearAttentionIfFocused();
    window.addEventListener('focus', recheckAttention);
    document.addEventListener('visibilitychange', recheckAttention);
    this.disposables.push({
      dispose: () => {
        window.removeEventListener('focus', recheckAttention);
        document.removeEventListener('visibilitychange', recheckAttention);
      },
    });

    this.#installInteractions();
    this.#scheduleLayout();
  }

  /** Called by dockview when panel parameters change (params is the full merged object). */
  update(event) {
    this.params = { ...(event?.params ?? this.params) };
  }

  layout() {
    this.#scheduleLayout();
  }

  focus() {
    this.term?.focus();
  }

  onShow() {
    this.#scheduleLayout(2);
  }

  dispose() {
    if (this.disposed) return;
    this.disposed = true;
    this.#setAttention(false);
    if (this.layoutFrame) cancelAnimationFrame(this.layoutFrame);
    this.layoutFrame = 0;
    this.conn.stop();
    this.overlay.dispose();
    for (const disposable of this.disposables.splice(0)) {
      try {
        disposable.dispose();
      } catch (err) {
        console.error('tmux-web: dispose failed', err);
      }
    }
    try {
      this.webgl?.dispose();
    } catch {
      // renderer already gone
    }
    this.webgl = null;
    try {
      this.term?.dispose();
    } catch (err) {
      console.error('tmux-web: terminal dispose failed', err);
    }
    this.term = null;
    if (this.services.registry.get(this.id) === this) this.services.registry.delete(this.id);
    this.element.remove();
  }

  // ---- public state ---------------------------------------------------------------

  /** Current tmux session name (follows renames). */
  get session() {
    return this.params.session ?? this.title;
  }

  get connected() {
    return this.conn.isOpen;
  }

  /** State for the tab's dot: idle | connecting | connected | waiting | closed. */
  get dotState() {
    const state = this.conn.state;
    return state === 'open' ? 'connected' : state;
  }

  /** Visible rows of the terminal buffer joined by "\n" (trailing spaces trimmed). */
  screenText() {
    const term = this.term;
    if (!term) return '';
    const buffer = term.buffer.active;
    const lines = [];
    for (let y = 0; y < term.rows; y += 1) {
      const line = buffer.getLine(buffer.viewportY + y);
      lines.push(line ? line.translateToString(true) : '');
    }
    return lines.join('\n');
  }

  /** Upload image files and paste their server paths into the terminal. */
  async uploadFiles(files) {
    const paths = await uploadImages(this.services.api, files);
    if (paths.length === 0 || this.disposed || !this.term) return;
    this.term.paste(`${paths.join(' ')} `);
    if (!this.connected) toast.warn(`Not connected; the image path is ${paths.join(' ')}`);
  }

  /** Open the file chooser and upload what the user picks. */
  async chooseAndUploadImages() {
    const files = await pickImages();
    if (files.length > 0) await this.uploadFiles(files);
    this.term?.focus();
  }

  /** Copy the terminal selection to the clipboard. */
  copySelection({ clear = false } = {}) {
    const term = this.term;
    if (!term || !term.hasSelection()) return false;
    const text = term.getSelection();
    if (clear) term.clearSelection();
    void copyText(text);
    return true;
  }

  /** Paste clipboard text (context menu); falls back to a hint when the API is unavailable. */
  async pasteFromClipboard() {
    if (!canReadClipboard()) {
      toast.info('Clipboard access is not available here: use Ctrl+V to paste');
      return;
    }
    const text = await readClipboardText();
    if (text === null) {
      toast.info('The browser blocked clipboard access: use Ctrl+V to paste');
      return;
    }
    if (text) this.term?.paste(text);
  }

  // ---- layout / lifecycle ---------------------------------------------------------

  /** True when the panel can be measured: shown, attached and non-empty. */
  #layoutReady() {
    return (
      !this.disposed &&
      this.element.isConnected &&
      this.panelApi?.isVisible !== false &&
      this.element.clientWidth > 0 &&
      this.element.clientHeight > 0
    );
  }

  /**
   * Coalesce layout work into one animation frame. `frames > 1` waits for dockview's own
   * positioning of the panel; a pending longer wait is never shortened by a later request.
   */
  #scheduleLayout(frames = 1) {
    if (this.disposed) return;
    const wanted = Math.max(frames, this.layoutFramesLeft);
    if (this.layoutFrame) cancelAnimationFrame(this.layoutFrame);
    this.layoutFramesLeft = wanted;
    const step = () => {
      this.layoutFrame = requestAnimationFrame(() => {
        this.layoutFrame = 0;
        this.layoutFramesLeft -= 1;
        if (this.layoutFramesLeft > 0) {
          step();
          return;
        }
        this.layoutFramesLeft = 0;
        this.#layoutNow();
      });
    };
    step();
  }

  #layoutNow() {
    if (this.disposed || !this.#layoutReady()) return;
    if (!this.opened) {
      this.#openTerminal();
      this.#fit();
      this.conn.start();
      if (this.panelApi?.isActive) this.#focusSoon();
      return;
    }
    this.#fit();
  }

  #fit() {
    const term = this.term;
    if (!term || !this.fit || !this.#layoutReady()) return;
    const dims = this.fit.proposeDimensions();
    if (!dims || !Number.isFinite(dims.cols) || !Number.isFinite(dims.rows) || dims.cols < 2 || dims.rows < 2) return;
    if (dims.cols !== term.cols || dims.rows !== term.rows) this.fit.fit();
  }

  #openTerminal() {
    const { persistence, linkTooltip } = this.services;
    const prefs = persistence.prefs;
    const links = createLinkHandlers(linkTooltip);
    const term = new Terminal({
      allowProposedApi: true,
      scrollback: 1000,
      fontFamily: FONT_FAMILY,
      fontSize: prefs.fontSize,
      theme: { ...TERMINAL_THEME },
      cursorBlink: false,
      macOptionIsMeta: true,
      macOptionClickForcesSelection: true,
      linkHandler: links.oscLinkHandler,
    });
    this.term = term;
    this.fit = new FitAddon();
    term.loadAddon(this.fit);
    term.loadAddon(new Unicode11Addon());
    term.unicode.activeVersion = '11';
    term.loadAddon(new WebLinksAddon(links.webLinksHandler, links.webLinksOptions));
    term.loadAddon(new ClipboardAddon(undefined, osc52Provider));
    term.open(this.host);
    this.opened = true;
    if (prefs.webgl) this.#enableWebgl();

    term.attachCustomKeyEventHandler(
      createKeyHandler({
        hasSelection: () => term.hasSelection(),
        copySelection: (options) => this.copySelection(options),
        toggleMaximize: () => this.services.workspace.toggleMaximize(this.id),
      }),
    );

    // Copy-on-select: copy once the selection has settled (debounced), if enabled in prefs.
    const copyOnSelect = debounce(() => {
      if (this.disposed || !this.services.persistence.prefs.copyOnSelect) return;
      if (term.hasSelection()) void copyText(term.getSelection(), { quiet: true });
    }, COPY_ON_SELECT_DELAY_MS);

    // xterm only reports a selection that changed. Selecting the same text again (after the
    // clipboard was overwritten, or after copy-on-select was switched on) must copy it again,
    // so the end of a mouse selection asks for a copy as well. A click that only clears the
    // selection finds nothing to copy.
    const copyAfterMouseSelection = (event) => {
      if (event.button === 0) copyOnSelect();
    };
    term.element.addEventListener('mouseup', copyAfterMouseSelection);

    this.disposables.push(
      { dispose: () => copyOnSelect.cancel() },
      { dispose: () => term.element?.removeEventListener('mouseup', copyAfterMouseSelection) },
      term.onData((data) => this.conn.sendInput(encoder.encode(data))),
      term.onBinary((data) => this.conn.sendInput(binaryStringToBytes(data))),
      term.onResize(() => this.#syncSize()),
      term.onSelectionChange(() => copyOnSelect()),
      term.onBell(() => {
        if (!this.#isFocused()) this.#setAttention(true);
      }),
    );
  }

  #enableWebgl() {
    if (this.webgl || !this.term) return;
    const addon = new WebglAddon();
    try {
      addon.onContextLoss(() => {
        // Fall back to the DOM renderer when the GPU context is lost.
        try {
          addon.dispose();
        } catch {
          // already disposed
        }
        if (this.webgl === addon) this.webgl = null;
      });
      this.term.loadAddon(addon);
      this.webgl = addon;
    } catch (err) {
      console.warn('tmux-web: WebGL renderer unavailable, using the DOM renderer', err);
      try {
        addon.dispose();
      } catch {
        // partially initialised
      }
    }
  }

  #disableWebgl() {
    if (!this.webgl) return;
    try {
      this.webgl.dispose();
    } catch {
      // already disposed
    }
    this.webgl = null;
  }

  #onPrefsChanged(prefs, key) {
    if (!this.term) return;
    if (key === 'fontSize') {
      this.term.options.fontSize = prefs.fontSize;
      this.#scheduleLayout();
    } else if (key === 'webgl') {
      if (prefs.webgl) this.#enableWebgl();
      else this.#disableWebgl();
    }
  }

  // ---- connection -------------------------------------------------------------------

  #sizeForConnect() {
    const size = { cols: this.term?.cols ?? 80, rows: this.term?.rows ?? 24 };
    this.lastSent = size;
    return size;
  }

  #onSocketOpen() {
    // A fresh attach redraws the whole screen, so start from a clean terminal.
    this.term?.reset();
    this.#syncSize();
    this.services.workspace.syncPanel(this.id); // learn session id / server id by name
  }

  /** Send a resize frame, but only when cols/rows differ from what the server knows. */
  #syncSize() {
    const term = this.term;
    if (!term || !this.conn.isOpen) return;
    const { cols, rows } = term;
    if (this.lastSent && this.lastSent.cols === cols && this.lastSent.rows === rows) return;
    if (this.conn.sendResize(cols, rows)) this.lastSent = { cols, rows };
  }

  #onConnectionState(info) {
    const session = this.session;
    switch (info.state) {
      case 'open':
        this.overlay.hide();
        break;
      case 'connecting':
        if (this.overlay.visible) this.overlay.show({ kind: 'reconnecting', session });
        break;
      case 'waiting':
        this.overlay.show({ kind: 'waiting', session, message: info.message, retryAt: info.retryAt });
        break;
      case 'closed':
        if (info.reason === 'detached') this.overlay.show({ kind: 'detached', session });
        else if (info.reason === 'no-server' || info.reason === 'server-down') {
          this.overlay.show({ kind: 'server-down', session });
        } else this.overlay.show({ kind: 'gone', session });
        break;
      default:
        break;
    }
    this.events.emit('state', info);
  }

  /** Reconnect automatically once the session is back on a running server. */
  #onStatus(status) {
    if (this.disposed || this.conn.state !== 'closed') return;
    if (!RECOVERABLE_REASONS.has(this.conn.reason)) return;
    if (status.running && this.services.store.hasSession(this.session)) this.conn.start();
  }

  async #recreateSession() {
    const { api, store } = this.services;
    const name = this.session;
    try {
      const created = await api.createSession(name);
      this.#adoptIds(created?.id ?? null, store.serverId());
    } catch (err) {
      if (err?.code !== 'exists') {
        toast.error(`Could not recreate session “${name}”: ${errorMessage(err)}`);
        return;
      }
    }
    this.conn.retryNow();
  }

  #adoptIds(sessionId, serverId) {
    if (!this.panelApi || (sessionId === null && serverId === null)) return;
    this.panelApi.updateParameters({ ...this.params, sessionId, serverId });
  }

  // ---- user interaction -----------------------------------------------------------------

  #installInteractions() {
    const root = this.element;
    const listen = (type, handler, options) => {
      root.addEventListener(type, handler, options);
      this.disposables.push({ dispose: () => root.removeEventListener(type, handler, options) });
    };

    // Clicking or focusing inside the terminal makes this panel (and its group) active.
    const activate = () => {
      if (this.panelApi && !this.panelApi.isActive) this.panelApi.setActive();
    };
    listen('pointerdown', activate, true);
    listen('focusin', activate);

    // Image paste: capture phase, so xterm's own paste handler never sees image pastes.
    listen(
      'paste',
      (event) => {
        const files = imagesFromClipboard(event.clipboardData);
        if (files.length === 0) return;
        event.preventDefault();
        event.stopPropagation();
        void this.uploadFiles(files);
      },
      true,
    );

    // Image drop: only OS file drags, so dockview's tab drag and drop is untouched.
    let dragDepth = 0;
    const setDropActive = (active) => root.classList.toggle('drop-active', active);
    listen(
      'dragenter',
      (event) => {
        if (!dragHasFiles(event.dataTransfer)) return;
        event.preventDefault();
        event.stopPropagation();
        dragDepth += 1;
        setDropActive(true);
      },
      true,
    );
    listen(
      'dragover',
      (event) => {
        if (!dragHasFiles(event.dataTransfer)) return;
        event.preventDefault();
        event.stopPropagation();
        event.dataTransfer.dropEffect = 'copy';
      },
      true,
    );
    listen(
      'dragleave',
      (event) => {
        if (!dragHasFiles(event.dataTransfer)) return;
        event.stopPropagation();
        dragDepth = Math.max(0, dragDepth - 1);
        if (dragDepth === 0) setDropActive(false);
      },
      true,
    );
    listen(
      'drop',
      (event) => {
        if (!dragHasFiles(event.dataTransfer)) return;
        event.preventDefault();
        event.stopPropagation();
        dragDepth = 0;
        setDropActive(false);
        void this.uploadFiles(Array.from(event.dataTransfer.files));
      },
      true,
    );

    // Finger scrolling: tmux history, or a wheel for a program that tracks the mouse.
    this.disposables.push({
      dispose: installTouchScroll(root, {
        rowHeight: () => (this.term && this.term.rows > 0 ? this.host.clientHeight / this.term.rows : 0),
        scroll: (lines) => this.services.api.scrollSession(this.session, lines),
        mouseTracking: () => (this.term?.modes.mouseTrackingMode ?? 'none') !== 'none',
        wheel: (deltaY, clientX, clientY) => {
          const target = this.term?.element?.querySelector('.xterm-screen') ?? this.term?.element;
          target?.dispatchEvent(
            new WheelEvent('wheel', { deltaY, deltaMode: 0, clientX, clientY, bubbles: true, cancelable: true }),
          );
        },
      }),
    });

    // Context menu: unless the program inside tracks the mouse, or Shift is held.
    listen(
      'contextmenu',
      (event) => {
        const tracking = this.term && this.term.modes.mouseTrackingMode !== 'none';
        if (tracking && !event.shiftKey) return;
        event.preventDefault();
        event.stopPropagation();
        this.#openContextMenu(event);
      },
      true,
    );
  }

  #openContextMenu(event) {
    const { contextMenu, workspace, picker } = this.services;
    const group = this.panelApi?.group;
    const maximized = workspace.isMaximized(this.id);
    contextMenu.open(
      { x: event.clientX, y: event.clientY },
      [
        {
          label: 'Copy',
          action: 'copy',
          icon: 'copy',
          shortcut: 'Ctrl+Shift+C',
          disabled: !this.term?.hasSelection(),
          onSelect: () => this.copySelection(),
        },
        { label: 'Paste', action: 'paste', icon: 'paste', shortcut: 'Ctrl+V', onSelect: () => void this.pasteFromClipboard() },
        { label: 'Upload image…', action: 'upload', icon: 'upload', onSelect: () => void this.chooseAndUploadImages() },
        { separator: true },
        {
          label: 'Split right',
          action: 'split-right',
          icon: 'split-right',
          onSelect: () => picker.open({ mode: 'split', referenceGroup: group, direction: 'right' }),
        },
        {
          label: 'Split down',
          action: 'split-down',
          icon: 'split-down',
          onSelect: () => picker.open({ mode: 'split', referenceGroup: group, direction: 'below' }),
        },
        {
          label: maximized ? 'Restore' : 'Maximise',
          action: 'maximize',
          icon: maximized ? 'restore' : 'maximize',
          shortcut: 'Ctrl+Shift+Enter',
          onSelect: () => workspace.toggleMaximize(this.id),
        },
        { separator: true },
        {
          label: 'Rename session…',
          action: 'rename',
          icon: 'pencil',
          onSelect: () => this.services.renameDialog.open({ panelId: this.id, session: this.session }),
        },
        { label: 'Close panel', action: 'close', icon: 'close', onSelect: () => this.panelApi?.close() },
      ],
      { onClose: () => this.term?.focus() },
    );
  }

  /** True while a bell rang in this panel and the user has not looked at it yet. */
  get attention() {
    return this.#attention;
  }

  /** The user is looking at this panel: active panel of the active group, in a focused page. */
  #isFocused() {
    return (
      Boolean(this.panelApi?.isActive) &&
      this.panelApi?.isGroupActive !== false &&
      document.visibilityState === 'visible' &&
      document.hasFocus()
    );
  }

  #clearAttentionIfFocused() {
    if (this.#attention && this.#isFocused()) this.#setAttention(false);
  }

  #setAttention(value) {
    if (this.#attention === value) return;
    this.#attention = value;
    this.events.emit('attention', value);
  }

  /**
   * Give the terminal keyboard focus unless the user is typing in another control.
   * A tab that was just activated is still `visibility: hidden` for a frame or two (dockview
   * shows its render overlay after the active-panel change), and focus() on a hidden element
   * silently does nothing, so the attempt is repeated until the focus has really moved.
   */
  #focusSoon(framesLeft = FOCUS_RETRY_FRAMES) {
    requestAnimationFrame(() => {
      if (this.disposed || !this.term || this.panelApi?.isGroupActive === false) return;
      const active = document.activeElement;
      const typingElsewhere =
        active instanceof HTMLElement &&
        !this.element.contains(active) &&
        active.matches('input, textarea, select, [contenteditable="true"]');
      if (typingElsewhere) return;
      this.term.focus();
      if (framesLeft > 0 && document.activeElement !== this.term.textarea) this.#focusSoon(framesLeft - 1);
    });
  }
}
