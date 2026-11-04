/**
 * Top bar: "tmux-web · <hostname>", tmux status pill, Add panel, settings and
 * browser-fullscreen toggle.
 */
import { el, iconButton, plural, setButtonIcon } from './dom.js';
import { svgIcon } from './icons.js';
import { toast } from './toast.js';

/** How long the events socket may be down before the pill says so. */
const OFFLINE_GRACE_MS = 2500;

export class TopBar {
  /**
   * @param {HTMLElement} root the <header> element
   * @param {{store: any, picker: any, settings: any}} services
   */
  constructor(root, services) {
    this.root = root;
    this.services = services;
    this.offlineSince = Date.now();
    this.offlineTimer = null;

    this.title = el('div', { class: 'brand', testid: 'app-title' });
    this.pill = el('div', { class: 'status-pill', testid: 'status-pill', role: 'status' });
    this.addButton = el('button', {
      type: 'button',
      class: 'btn btn-primary add-panel-btn',
      testid: 'add-panel-btn',
      title: 'Add a panel: open a tmux session',
      'aria-haspopup': 'dialog',
    });
    this.addButton.append(svgIcon('plus', { size: 14 }), el('span', { text: 'Add panel' }));
    this.settingsButton = iconButton({
      icon: 'settings',
      title: 'Settings',
      testid: 'settings-btn',
      className: 'top-btn',
    });
    this.settingsButton.setAttribute('aria-haspopup', 'dialog');
    this.fullscreenButton = iconButton({
      icon: 'fullscreen',
      title: 'Toggle fullscreen',
      testid: 'fullscreen-btn',
      className: 'top-btn',
    });

    root.setAttribute('data-testid', 'top-bar');
    root.append(
      el('div', { class: 'top-left' }, svgIcon('terminal', { size: 16, className: 'brand-icon' }), this.title, this.pill),
      el('div', { class: 'top-right' }, this.addButton, this.settingsButton, this.fullscreenButton),
    );

    this.addButton.addEventListener('click', () =>
      services.picker.toggle({ mode: 'add', anchorElement: this.addButton }),
    );
    this.settingsButton.addEventListener('click', () => services.settings.toggle({ anchorElement: this.settingsButton }));
    this.#initFullscreen();

    const render = () => this.render();
    services.store.subscribe((kind) => {
      if (kind === 'connection' && !services.store.eventsConnected) {
        // Show "unreachable" only if the live socket stays down for a while.
        this.offlineSince = Date.now();
        clearTimeout(this.offlineTimer);
        this.offlineTimer = setTimeout(render, OFFLINE_GRACE_MS + 50);
      }
      render();
    });
    this.offlineTimer = setTimeout(render, OFFLINE_GRACE_MS + 50);
    this.render();
  }

  render() {
    const { store } = this.services;
    const hostname = store.info?.hostname;
    this.title.textContent = hostname ? `tmux-web · ${hostname}` : 'tmux-web';
    document.title = hostname ? `${hostname} · tmux-web` : 'tmux-web';

    const offline = !store.eventsConnected && Date.now() - this.offlineSince > OFFLINE_GRACE_MS;
    const status = store.status;
    let state;
    let text;
    let tooltip = '';
    if (offline) {
      state = 'offline';
      text = 'tmux-web unreachable';
      tooltip = 'Lost the live connection to the tmux-web server; retrying';
    } else if (!status) {
      state = 'loading';
      text = 'connecting…';
    } else if (status.running) {
      state = 'ok';
      const version = status.server?.version;
      text = `${version ? `tmux ${version}` : 'tmux'} · ${plural(status.sessions.length, 'session')}`;
      tooltip = store.info?.tmux_socket ? `tmux socket: ${store.info.tmux_socket}` : '';
    } else {
      state = 'down';
      text = 'tmux server not running';
      tooltip = 'No tmux server is running on this machine';
    }
    this.pill.dataset.state = state;
    this.pill.textContent = text;
    this.pill.title = tooltip;
  }

  #initFullscreen() {
    if (!document.fullscreenEnabled) {
      this.fullscreenButton.hidden = true;
      return;
    }
    this.fullscreenButton.addEventListener('click', () => {
      if (document.fullscreenElement) {
        void document.exitFullscreen();
      } else {
        document.documentElement.requestFullscreen().catch(() => toast.warn('The browser refused fullscreen mode'));
      }
    });
    document.addEventListener('fullscreenchange', () => {
      const active = Boolean(document.fullscreenElement);
      setButtonIcon(this.fullscreenButton, active ? 'fullscreen-exit' : 'fullscreen', active ? 'Exit fullscreen' : 'Enter fullscreen');
    });
  }
}
