/**
 * "Rename session…" dialog: POST /api/sessions/rename, then the panel follows the new
 * name (title and params are updated and the layout is saved).
 */
import { el, errorMessage, validateSessionName } from './dom.js';
import { Modal } from './modal.js';

export class RenameDialog {
  /**
   * @param {{api: any, workspace: any}} services
   */
  constructor(services) {
    this.services = services;
    this.modal = new Modal({ title: 'Rename session', testid: 'rename-dialog', className: 'modal-small' });
    this.panelId = null;
    this.session = '';
    this.busy = false;

    this.input = el('input', {
      type: 'text',
      class: 'modal-input',
      testid: 'rename-input',
      'aria-label': 'New session name',
      autocomplete: 'off',
      spellcheck: 'false',
    });
    this.error = el('div', { class: 'modal-error', testid: 'rename-error', role: 'alert', hidden: true });
    this.cancelButton = el('button', { type: 'button', class: 'btn', testid: 'rename-cancel-btn', text: 'Cancel' });
    this.confirmButton = el('button', { type: 'button', class: 'btn btn-primary', testid: 'rename-confirm-btn', text: 'Rename' });

    this.label = el('label', { class: 'modal-label' });
    this.modal.body.append(this.label, this.input, this.error);
    this.modal.footer.append(this.cancelButton, this.confirmButton);

    this.cancelButton.addEventListener('click', () => this.modal.close());
    this.confirmButton.addEventListener('click', () => void this.#submit());
    this.input.addEventListener('keydown', (ev) => {
      if (ev.key === 'Enter') {
        ev.preventDefault();
        void this.#submit();
      }
    });
    this.input.addEventListener('input', () => this.#showError(''));
    this.modal.on('close', () => this.services.workspace.focusActive());
  }

  /** @param {{panelId: string, session: string}} options */
  open({ panelId, session }) {
    this.panelId = panelId;
    this.session = session;
    this.label.textContent = `New name for “${session}”`;
    this.input.value = session;
    this.#showError('');
    this.modal.open(this.input);
    this.input.select();
  }

  #showError(message) {
    this.error.textContent = message;
    this.error.hidden = message === '';
  }

  async #submit() {
    if (this.busy) return;
    const newName = this.input.value;
    if (newName === this.session) {
      this.modal.close();
      return;
    }
    const problem = validateSessionName(newName);
    if (problem) {
      this.#showError(problem);
      return;
    }
    this.busy = true;
    this.confirmButton.disabled = true;
    try {
      const result = await this.services.api.renameSession(this.session, newName);
      this.services.workspace.applyRename(this.panelId, result?.name ?? newName);
      this.modal.close();
    } catch (err) {
      this.#showError(errorMessage(err));
    } finally {
      this.busy = false;
      this.confirmButton.disabled = false;
    }
  }
}
