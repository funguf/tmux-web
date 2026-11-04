/**
 * Requirement 9: Ctrl+click on a printed URL calls `window.open` with that URL; a plain click
 * does not. Plain-text URLs and OSC 8 hyperlinks, with the real mouse and keyboard.
 */
import assert from 'node:assert/strict';
import { describe, it } from 'node:test';
import * as ui from '../lib/app.js';
import { TEST, useEnv } from '../lib/suite.js';
import { staysFalse, waitFor } from '../lib/wait.js';

const OPEN_ARGS = ['_blank', 'noopener,noreferrer'];

describe('links in the terminal', () => {
  const ctx = useEnv('links');
  let sessions = 0;

  /** A page with a focused terminal, `window.open` replaced by a recorder, and a dialog recorder. */
  async function openTerminal() {
    const { env } = ctx;
    sessions += 1;
    const session = `ln-${sessions}`;
    env.tmux.newSession(session, { cwd: env.dir(session) });
    const page = await env.freshPage();
    page.__dialogs = [];
    page.on('dialog', (dialog) => {
      page.__dialogs.push(dialog.message());
      void dialog.dismiss();
    });
    await ui.spyOnWindowOpen(page);
    const panel = await ui.addPanel(page, session);
    await ui.waitForPrompt(page, panel.id);
    await ui.clickTerminal(page, panel.id);
    await ui.waitForTerminalFocus(page, panel.id);
    return { page, panel };
  }

  /** Hover the middle of `text` on screen; resolves to the point once the link tooltip shows. */
  async function hover(page, panel, text) {
    const pos = await ui.locateText(page, panel.id, text);
    const point = { x: (pos.x0 + pos.x1) / 2, y: pos.y };
    await page.mouse.move(point.x - 20, point.y + 60); // come from outside the link
    await page.mouse.move(point.x, point.y, { steps: 4 });
    return point;
  }

  async function ctrlClick(page, point) {
    await page.keyboard.down('Control');
    try {
      await page.mouse.click(point.x, point.y);
    } finally {
      await page.keyboard.up('Control');
    }
  }

  it('Ctrl+click on a printed URL calls window.open with it; a plain click does not', TEST, async () => {
    const { page, panel } = await openTerminal();
    const url = 'http://127.0.0.1:9/e2e-link-target?a=1&b=2';
    await ui.typeLine(page, `echo '${url}'`); // quoted: the & is a shell operator
    await ui.waitForScreen(page, panel.id, (line) => line === url);

    const point = await hover(page, panel, url);
    const tooltip = page.getByTestId('link-tooltip');
    await waitFor('the link tooltip', () => tooltip.isVisible());
    assert.match(await tooltip.textContent(), /Ctrl\+click to open/);

    // A plain click only moves the caret/selection: nothing is opened.
    await page.mouse.click(point.x, point.y);
    await staysFalse('a plain click opened a window', async () => (await ui.openedWindows(page)).length > 0, { duration: 700 });

    // Ctrl+click opens exactly that URL, in a new tab without an opener.
    await ctrlClick(page, point);
    await waitFor('window.open to be called', async () => (await ui.openedWindows(page)).length > 0);
    assert.deepEqual(await ui.openedWindows(page), [[url, ...OPEN_ARGS]]);

    // Moving away hides the tooltip.
    await page.mouse.move(point.x, point.y + 150, { steps: 4 });
    await waitFor('the tooltip to go away', async () => !(await tooltip.isVisible()));
    assert.deepEqual(page.__dialogs, []);
    assert.deepEqual(page.__errors, []);
  });

  it('a URL inside punctuation links only the URL', TEST, async () => {
    const { page, panel } = await openTerminal();
    const url = 'http://127.0.0.1:9/punct?x=1';
    await ui.typeLine(page, `echo "see (${url})."`);
    await ui.waitForScreen(page, panel.id, (line) => line === `see (${url}).`);
    const point = await hover(page, panel, url);
    await waitFor('the link tooltip', () => page.getByTestId('link-tooltip').isVisible());
    await ctrlClick(page, point);
    await waitFor('window.open to be called', async () => (await ui.openedWindows(page)).length > 0);
    assert.deepEqual(await ui.openedWindows(page), [[url, ...OPEN_ARGS]], 'the brackets and the full stop are not part of the link');
  });

  it('OSC 8 hyperlinks open on Ctrl+click only; non-web schemes never open', TEST, async () => {
    const { page, panel } = await openTerminal();
    const link = (url, label) => `printf '\\033]8;;${url}\\033\\\\${label}\\033]8;;\\033\\\\\\n'`;
    await ui.typeLine(page, link('http://127.0.0.1:9/osc8-target', 'osc8-label'));
    await ui.typeLine(page, link('file:///etc/hostname', 'file-label'));
    await ui.typeLine(page, link('javascript:alert(1)', 'script-label'));
    await ui.waitForScreen(page, panel.id, (line) => line === 'script-label');

    const point = await hover(page, panel, 'osc8-label');
    await waitFor('the link tooltip', () => page.getByTestId('link-tooltip').isVisible());
    await page.mouse.click(point.x, point.y);
    await staysFalse('a plain click opened a hyperlink', async () => (await ui.openedWindows(page)).length > 0, { duration: 700 });
    await ctrlClick(page, point);
    await waitFor('window.open to be called', async () => (await ui.openedWindows(page)).length > 0);
    assert.deepEqual(await ui.openedWindows(page), [['http://127.0.0.1:9/osc8-target', ...OPEN_ARGS]]);

    for (const label of ['file-label', 'script-label']) {
      const target = await hover(page, panel, label);
      await ctrlClick(page, target);
    }
    await staysFalse('a non-web hyperlink was opened', async () => (await ui.openedWindows(page)).length > 1, { duration: 800 });
    assert.deepEqual(page.__dialogs, [], 'no confirmation dialog either');
    assert.deepEqual(page.__errors, []);
  });
});
