/**
 * Group header actions (right side of every group's tab strip):
 * split right, split down, upload image, maximise / restore.
 * Test ids: header-split-right, header-split-down, header-upload, header-maximize.
 */
import { el, iconButton, setButtonIcon } from './dom.js';
import { pickImages } from './image-upload.js';
import { toast } from './toast.js';

export class HeaderActions {
  /**
   * @param {object} services
   * @param {any} group the DockviewGroupPanel this header belongs to
   */
  constructor(services, group) {
    this.services = services;
    this.group = group;
    this.disposables = [];

    this.splitRight = iconButton({
      icon: 'split-right',
      title: 'Split right: open a session in a new group to the right',
      testid: 'header-split-right',
      className: 'header-btn',
      onClick: (ev) => this.#openSplit('right', ev.currentTarget),
    });
    this.splitDown = iconButton({
      icon: 'split-down',
      title: 'Split down: open a session in a new group below',
      testid: 'header-split-down',
      className: 'header-btn',
      onClick: (ev) => this.#openSplit('below', ev.currentTarget),
    });
    this.upload = iconButton({
      icon: 'upload',
      title: 'Upload image(s) and paste the path into this panel',
      testid: 'header-upload',
      className: 'header-btn',
      onClick: () => void this.#upload(),
    });
    this.maximize = iconButton({
      icon: 'maximize',
      title: 'Maximise this group',
      testid: 'header-maximize',
      className: 'header-btn',
      onClick: () => this.services.workspace.toggleMaximizeGroup(this.group),
    });
    this.element = el(
      'div',
      { class: 'header-actions', testid: 'header-actions' },
      this.splitRight,
      this.splitDown,
      this.upload,
      this.maximize,
    );
  }

  /** @param {{api: any, containerApi: any, group: any}} params */
  init(params) {
    this.group = params.group;
    const containerApi = params.containerApi;
    const refresh = () => this.#refresh();
    this.disposables.push(
      containerApi.onDidMaximizedGroupChange(refresh),
      containerApi.onDidLayoutChange(refresh),
      containerApi.onDidActivePanelChange(refresh),
    );
    this.#refresh();
  }

  dispose() {
    for (const disposable of this.disposables.splice(0)) disposable.dispose();
    this.element.remove();
  }

  #refresh() {
    const maximized = this.group.api.isMaximized();
    const state = maximized ? 'maximized' : 'normal';
    if (this.maximize.dataset.state !== state) {
      this.maximize.dataset.state = state;
      setButtonIcon(this.maximize, maximized ? 'restore' : 'maximize', maximized ? 'Restore this group' : 'Maximise this group');
    }
    this.maximize.setAttribute('aria-pressed', String(maximized));
    this.upload.disabled = !this.group.activePanel;
  }

  #openSplit(direction, button) {
    this.services.picker.open({
      mode: 'split',
      referenceGroup: this.group,
      direction,
      anchor: button.getBoundingClientRect(),
    });
  }

  async #upload() {
    const panel = this.group.activePanel;
    const terminal = panel ? this.services.registry.get(panel.id) : null;
    if (!terminal) {
      toast.info('Open a session in this group first');
      return;
    }
    const files = await pickImages();
    if (files.length > 0) await terminal.uploadFiles(files);
    terminal.focus();
  }
}
