/**
 * Requirement 11: with tmux mouse mode on (`tmux set -g mouse on`) Shift+drag still selects
 * text in the browser (and copy works) and Ctrl+click still opens a link.
 */
import assert from 'node:assert/strict';
import { describe, it } from 'node:test';
import * as ui from '../lib/app.js';
import { TEST, useEnv } from '../lib/suite.js';
import { staysFalse, waitFor } from '../lib/wait.js';

describe('tmux mouse mode', () => {
  const ctx = useEnv('mouse');
  let sessions = 0;

  /** A page with a terminal attached to a session of a tmux server that has mouse mode on. */
  async function openTerminal({ mouse = 'on' } = {}) {
    const { env } = ctx;
    sessions += 1;
    const session = `ms-${sessions}`;
    env.tmux.newSession(session, { cwd: env.dir(session) });
    env.tmux.run('set-option', '-g', 'mouse', mouse);
    assert.equal(env.tmux.run('show-options', '-gv', 'mouse').trim(), mouse);
    const page = await env.freshPage();
    await ui.spyOnWindowOpen(page);
    const panel = await ui.addPanel(page, session);
    await ui.waitForPrompt(page, panel.id);
    await ui.clickTerminal(page, panel.id);
    await ui.waitForTerminalFocus(page, panel.id);
    return { page, panel, session };
  }

  it('the terminal tracks the mouse for tmux, so a plain drag is not a browser selection', TEST, async () => {
    const { page, panel } = await openTerminal();
    await waitFor('tmux to switch on mouse tracking', async () => (await ui.mouseMode(page, panel.id)) !== 'none');
    assert.notEqual(await ui.mouseMode(page, panel.id), 'none');
    await ui.typeLine(page, 'echo plain-drag-word');
    await ui.waitForScreen(page, panel.id, (line) => line === 'plain-drag-word');
    await ui.dragSelect(page, panel.id, 'plain-drag-word');
    assert.equal(await ui.selectionText(page, panel.id), '', 'the drag went to tmux, the browser selected nothing');
  });

  it('Shift+drag selects text in the browser and copy works', TEST, async () => {
    const { page, panel } = await openTerminal();
    await waitFor('tmux to switch on mouse tracking', async () => (await ui.mouseMode(page, panel.id)) !== 'none');
    await ui.typeLine(page, 'echo shift-select-me-777 tail');
    await ui.waitForScreen(page, panel.id, (line) => line === 'shift-select-me-777 tail');

    // Copy-on-select: the Shift+drag selection reaches the clipboard.
    await ui.writeClipboard(page, 'sentinel');
    await ui.dragSelect(page, panel.id, 'shift-select-me-777', { shift: true });
    assert.equal(await ui.selectionText(page, panel.id), 'shift-select-me-777');
    await waitFor('the selection on the clipboard', async () => (await ui.readClipboard(page)) === 'shift-select-me-777');

    // Ctrl+Shift+C copies that selection too.
    await ui.writeClipboard(page, 'sentinel 2');
    await page.keyboard.press('Control+Shift+C');
    await waitFor('Ctrl+Shift+C to copy the Shift+drag selection', async () => (await ui.readClipboard(page)) === 'shift-select-me-777');

    // And a part of a line works as well.
    await ui.dragSelect(page, panel.id, 'select-me', { shift: true });
    assert.equal(await ui.selectionText(page, panel.id), 'select-me');
    await waitFor('the part on the clipboard', async () => (await ui.readClipboard(page)) === 'select-me');
    assert.deepEqual(page.__errors, []);
  });

  it('Ctrl+click still opens a link, a plain click does not', TEST, async () => {
    const { page, panel } = await openTerminal();
    await waitFor('tmux to switch on mouse tracking', async () => (await ui.mouseMode(page, panel.id)) !== 'none');
    const url = 'http://127.0.0.1:9/mouse-mode-link';
    await ui.typeLine(page, `echo ${url}`);
    await ui.waitForScreen(page, panel.id, (line) => line === url);

    const pos = await ui.locateText(page, panel.id, url);
    const point = { x: (pos.x0 + pos.x1) / 2, y: pos.y };
    await page.mouse.move(point.x - 20, point.y + 60);
    await page.mouse.move(point.x, point.y, { steps: 4 });
    await waitFor('the link tooltip', () => page.getByTestId('link-tooltip').isVisible());

    await page.mouse.click(point.x, point.y);
    await staysFalse('a plain click opened the link', async () => (await ui.openedWindows(page)).length > 0, { duration: 700 });

    await page.keyboard.down('Control');
    try {
      await page.mouse.click(point.x, point.y);
    } finally {
      await page.keyboard.up('Control');
    }
    await waitFor('window.open to be called', async () => (await ui.openedWindows(page)).length > 0);
    assert.deepEqual(await ui.openedWindows(page), [[url, '_blank', 'noopener,noreferrer']]);
    assert.deepEqual(page.__errors, []);
  });

  it('without mouse mode a plain drag selects in the browser (the contrast)', TEST, async () => {
    const { page, panel } = await openTerminal({ mouse: 'off' });
    assert.equal(await ui.mouseMode(page, panel.id), 'none');
    await ui.typeLine(page, 'echo plain-select-word');
    await ui.waitForScreen(page, panel.id, (line) => line === 'plain-select-word');
    await ui.dragSelect(page, panel.id, 'plain-select-word');
    assert.equal(await ui.selectionText(page, panel.id), 'plain-select-word');
  });
});
