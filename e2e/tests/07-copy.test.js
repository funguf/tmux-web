/**
 * Requirement 8: copy. Copy-on-select puts the selected text on the clipboard; Ctrl+C with a
 * selection copies and does not send ^C; Ctrl+Shift+C copies. All with real mouse and keyboard
 * input; the clipboard is read back from the browser.
 */
import assert from 'node:assert/strict';
import { describe, it } from 'node:test';
import * as ui from '../lib/app.js';
import { TEST, useEnv } from '../lib/suite.js';
import { staysFalse, waitFor } from '../lib/wait.js';

describe('copying text', () => {
  const ctx = useEnv('copy');
  let sessions = 0;

  /** A page with a focused terminal at a clean prompt on a session of its own. */
  async function openTerminal() {
    const { env } = ctx;
    sessions += 1;
    const session = `cp-${sessions}`;
    env.tmux.newSession(session, { cwd: env.dir(session) });
    const page = await env.freshPage();
    const panel = await ui.addPanel(page, session);
    await ui.waitForPrompt(page, panel.id);
    await ui.clickTerminal(page, panel.id);
    await ui.waitForTerminalFocus(page, panel.id);
    return { page, panel, session };
  }

  async function setCopyOnSelect(page, enabled) {
    await page.getByTestId('settings-btn').click();
    const toggle = page.getByTestId('copy-on-select-toggle');
    await toggle.setChecked(enabled);
    await page.keyboard.press('Escape');
    await page.getByTestId('settings-popover').waitFor({ state: 'hidden' });
  }

  it('copy-on-select puts the selected text on the clipboard', TEST, async () => {
    const { page, panel } = await openTerminal();
    await ui.typeLine(page, 'echo copy-me-12345 and more');
    await ui.waitForScreen(page, panel.id, (line) => line === 'copy-me-12345 and more');

    // A drag over the whole word...
    await ui.writeClipboard(page, 'sentinel');
    await ui.dragSelect(page, panel.id, 'copy-me-12345');
    assert.equal(await ui.selectionText(page, panel.id), 'copy-me-12345');
    await waitFor('the clipboard to hold the selection', async () => (await ui.readClipboard(page)) === 'copy-me-12345');

    // ...and over a part of it: only that part is copied.
    await ui.dragSelect(page, panel.id, 'me-123');
    assert.equal(await ui.selectionText(page, panel.id), 'me-123');
    await waitFor('the clipboard to hold the new selection', async () => (await ui.readClipboard(page)) === 'me-123');

    // A double click selects a word and copies it too.
    await ui.writeClipboard(page, 'sentinel again');
    await ui.doubleClickWord(page, panel.id, 'more');
    await waitFor('the double-clicked word on the clipboard', async () => (await ui.readClipboard(page)) === 'more');
    assert.deepEqual(page.__errors, []);
  });

  it('copy-on-select can be switched off in the settings', TEST, async () => {
    const { page, panel } = await openTerminal();
    await ui.typeLine(page, 'echo keep-clipboard-value');
    await ui.waitForScreen(page, panel.id, (line) => line === 'keep-clipboard-value');
    await setCopyOnSelect(page, false);

    await ui.writeClipboard(page, 'sentinel');
    await ui.dragSelect(page, panel.id, 'keep-clipboard-value');
    assert.equal(await ui.selectionText(page, panel.id), 'keep-clipboard-value', 'the text is selected');
    await staysFalse('selecting copied although copy-on-select is off', async () => (await ui.readClipboard(page)) !== 'sentinel', { duration: 1000 });

    await setCopyOnSelect(page, true);
    await ui.dragSelect(page, panel.id, 'keep-clipboard-value');
    await waitFor('copy-on-select to work again', async () => (await ui.readClipboard(page)) === 'keep-clipboard-value');
  });

  it('Ctrl+C with a selection copies it and does not send ^C; without one it interrupts', TEST, async () => {
    const { env } = ctx;
    const { page, panel, session } = await openTerminal();
    await setCopyOnSelect(page, false); // so that the clipboard can only come from Ctrl+C
    await ui.clickTerminal(page, panel.id);
    await ui.waitForTerminalFocus(page, panel.id);

    // `cat` stays in the foreground until it gets a ^C or end of input.
    await ui.typeLine(page, 'cat');
    await waitFor('cat to run', () => env.tmux.format(`=${session}:`, '#{pane_current_command}') === 'cat');
    await ui.typeLine(page, 'select-this-word');
    await ui.waitForScreen(page, panel.id, (line) => line === 'select-this-word');
    await ui.writeClipboard(page, 'sentinel');
    await ui.doubleClickWord(page, panel.id, 'select-this-word');
    await waitFor('the word to be selected', async () => (await ui.selectionText(page, panel.id)) === 'select-this-word');
    assert.equal(await ui.readClipboard(page), 'sentinel', 'selecting alone did not copy');

    await page.keyboard.press('Control+C');
    await waitFor('Ctrl+C to copy', async () => (await ui.readClipboard(page)) === 'select-this-word');
    await waitFor('the selection to be cleared', async () => (await ui.selectionText(page, panel.id)) === '');
    // No ^C reached the terminal: cat still runs and no "^C" was echoed.
    await staysFalse(
      'a ^C reached the foreground program',
      async () => env.tmux.format(`=${session}:`, '#{pane_current_command}') !== 'cat' || (await ui.screenLines(page, panel.id)).some((line) => line.includes('^C')),
      { duration: 1000 },
    );

    // Nothing selected now: Ctrl+C is a real interrupt again.
    await page.keyboard.press('Control+C');
    await waitFor('cat to be interrupted', () => env.tmux.format(`=${session}:`, '#{pane_current_command}') === 'sh');
    await ui.waitForScreen(page, panel.id, (line) => line.includes('^C'));
    assert.equal(await ui.readClipboard(page), 'select-this-word', 'the interrupt did not touch the clipboard');
    assert.deepEqual(page.__errors, []);
  });

  it('Ctrl+Shift+C copies the selection and sends nothing to the terminal', TEST, async () => {
    const { env } = ctx;
    const { page, panel, session } = await openTerminal();
    await setCopyOnSelect(page, false);
    await ui.clickTerminal(page, panel.id);
    await ui.waitForTerminalFocus(page, panel.id);
    await ui.typeLine(page, 'cat');
    await waitFor('cat to run', () => env.tmux.format(`=${session}:`, '#{pane_current_command}') === 'cat');
    await ui.typeLine(page, 'shift-copy-word');
    await ui.waitForScreen(page, panel.id, (line) => line === 'shift-copy-word');

    await ui.writeClipboard(page, 'sentinel');
    await ui.doubleClickWord(page, panel.id, 'shift-copy-word');
    await waitFor('the word to be selected', async () => (await ui.selectionText(page, panel.id)) === 'shift-copy-word');
    await page.keyboard.press('Control+Shift+C');
    await waitFor('Ctrl+Shift+C to copy', async () => (await ui.readClipboard(page)) === 'shift-copy-word');
    assert.equal(await ui.selectionText(page, panel.id), 'shift-copy-word', 'the selection stays (it is not a ^C)');

    // With nothing selected Ctrl+Shift+C does nothing: no copy, no input to the program.
    await page.mouse.click(5, 400); // clears the selection
    await waitFor('the selection to be gone', async () => (await ui.selectionText(page, panel.id)) === '');
    await ui.writeClipboard(page, 'sentinel 2');
    await page.keyboard.press('Control+Shift+C');
    await staysFalse('Ctrl+Shift+C changed the clipboard without a selection', async () => (await ui.readClipboard(page)) !== 'sentinel 2', { duration: 600 });
    assert.equal(env.tmux.format(`=${session}:`, '#{pane_current_command}'), 'cat', 'cat was not interrupted');
    assert.ok(!(await ui.screenLines(page, panel.id)).some((line) => line.includes('^')), 'no control character was echoed');
    assert.deepEqual(page.__errors, []);
  });

  it('the copy and maximise shortcuts are claimed from the browser, the paste shortcuts are not', TEST, async () => {
    const { page, panel } = await openTerminal();
    await ui.typeLine(page, 'echo claimed-word');
    await ui.waitForScreen(page, panel.id, (line) => line === 'claimed-word');
    // Record what the page ends up doing with each key: Ctrl+Shift+C opens the developer tools
    // in Chrome unless the page prevents its default; paste needs the default to fire `paste`.
    await page.evaluate(() => {
      window.__keys = [];
      window.addEventListener('keydown', (event) => window.__keys.push({ key: event.key, ctrl: event.ctrlKey, shift: event.shiftKey, prevented: event.defaultPrevented }));
    });
    const keyLog = () => page.evaluate(() => window.__keys.splice(0));

    await ui.doubleClickWord(page, panel.id, 'claimed-word');
    await page.keyboard.press('Control+Shift+C');
    assert.deepEqual((await keyLog()).filter((entry) => entry.key.toLowerCase() === 'c'), [{ key: 'C', ctrl: true, shift: true, prevented: true }]);

    await page.keyboard.press('Control+Shift+Enter');
    assert.deepEqual((await keyLog()).filter((entry) => entry.key === 'Enter'), [{ key: 'Enter', ctrl: true, shift: true, prevented: true }]);
    await ui.waitForTerminalFocus(page, panel.id);
    await page.keyboard.press('Control+Shift+Enter'); // maximised again: back to normal
    await keyLog();

    await ui.writeClipboard(page, 'pasted');
    await page.keyboard.press('Control+V');
    await page.keyboard.press('Control+Shift+V');
    const pastes = (await keyLog()).filter((entry) => entry.key.toLowerCase() === 'v');
    assert.equal(pastes.length, 2);
    assert.ok(pastes.every((entry) => entry.prevented === false), 'the browser keeps the paste default, so that it fires a paste event');
    await ui.waitForScreen(page, panel.id, (line) => line === '$ pastedpasted');
    assert.deepEqual(page.__errors, []);
  });

  it('the context menu copies the selection', TEST, async () => {
    const { page, panel } = await openTerminal();
    await setCopyOnSelect(page, false);
    await ui.clickTerminal(page, panel.id);
    await ui.typeLine(page, 'echo menu-copy-word');
    await ui.waitForScreen(page, panel.id, (line) => line === 'menu-copy-word');
    await ui.writeClipboard(page, 'sentinel');
    const pos = await ui.doubleClickWord(page, panel.id, 'menu-copy-word');
    await waitFor('the word to be selected', async () => (await ui.selectionText(page, panel.id)) === 'menu-copy-word');

    await page.mouse.click(pos.x0 + 40, pos.y + 120, { button: 'right' });
    await page.getByTestId('context-menu').waitFor({ state: 'visible' });
    const copy = page.locator('[data-testid="context-menu-item"][data-action="copy"]');
    assert.equal(await copy.isDisabled(), false, 'Copy is enabled while text is selected');
    await copy.click();
    await waitFor('the menu to copy', async () => (await ui.readClipboard(page)) === 'menu-copy-word');
    await page.getByTestId('context-menu').waitFor({ state: 'hidden' });
  });

  it('text copied in tmux reaches the clipboard through OSC 52', TEST, async () => {
    const { env } = ctx;
    const { page } = await openTerminal();
    await ui.writeClipboard(page, 'sentinel');
    // `set-buffer -w` is what tmux copy-mode does: it sends the text to the terminal with OSC 52.
    env.tmux.run('set-buffer', '-w', 'from-tmux-copy-mode');
    await waitFor('the OSC 52 text on the clipboard', async () => (await ui.readClipboard(page)) === 'from-tmux-copy-mode');
    assert.deepEqual(page.__errors, []);
  });
});
