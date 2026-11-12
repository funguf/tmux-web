/**
 * "Terminate session" confirmation dialog: POST /api/sessions/kill, then every panel that
 * shows the session is closed. Opened from the session picker's red cross.
 */
import { el, errorMessage, plural } from './dom.js';
import { Modal } from './modal.js';
import { toast } from './toast.js';

export class KillDialog {
  /**
   * @param {{api: any, workspace: any}} services
   */
  constructor(services) {
    this.services = services;
    this.modal = new Modal({ title: 'Terminate session', testid: 'kill-dialog', className: 'modal-small' });
    this.session = '';
    this.busy = false;

    this.text = el('p', { class: 'modal-text', testid: 'kill-text' });
    this.claudeNote = el('p', { class: 'modal-text', testid: 'kill-claude', text: 'Claude Code is running here.' });
    this.error = el('div', { class: 'modal-error', testid: 'kill-error', role: 'alert', hidden: true });
    this.cancelButton = el('button', { type: 'button', class: 'btn', testid: 'kill-cancel-btn', text: 'Cancel' });
    this.confirmButton = el('button', { type: 'button', class: 'btn btn-danger', testid: 'kill-confirm-btn', text: 'Terminate' });

    this.modal.body.append(this.text, this.claudeNote, this.error);
    this.modal.footer.append(this.cancelButton, this.confirmButton);

    this.cancelButton.addEventListener('click', () => this.modal.close());
    this.confirmButton.addEventListener('click', () => void this.#submit());
    this.modal.on('close', () => this.services.workspace.focusActive());
  }

  /** @param {{session: {name: string, windows?: number, claude_panes?: number}}} options */
  open({ session }) {
    this.session = session.name;
    this.text.textContent = `Terminate “${session.name}”? This closes its ${plural(session.windows ?? 0, 'window')} and everything running in them.`;
    this.claudeNote.hidden = (session.claude_panes ?? 0) <= 0;
    this.#showError('');
    this.busy = false;
    this.confirmButton.disabled = false;
    this.modal.open(this.cancelButton);
  }

  #showError(message) {
    this.error.textContent = message;
    this.error.hidden = message === '';
  }

  async #submit() {
    if (this.busy) return;
    const name = this.session;
    this.busy = true;
    this.confirmButton.disabled = true;
    this.#showError('');
    try {
      await this.services.api.killSession(name);
    } catch (err) {
      this.busy = false;
      this.confirmButton.disabled = false;
      this.#showError(errorMessage(err));
      return;
    }
    this.busy = false;
    this.modal.close();
    toast.success(`Terminated “${name}”`);
    this.services.workspace.closeSession(name);
  }
}
