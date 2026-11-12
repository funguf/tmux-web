/**
 * Terminating a tmux session from the session picker: the red cross on each row, the confirm
 * dialog, and what happens to open panels. Every tmux command runs on the isolated server.
 */
import assert from 'node:assert/strict';
import fs from 'node:fs';
import { describe, it } from 'node:test';
import * as ui from '../lib/app.js';
import { TEST, useEnv } from '../lib/suite.js';
import { waitFor } from '../lib/wait.js';

const killButton = (page, session) => page.locator(`[data-testid="session-picker-kill"][data-session="${session}"]`);

describe('terminate a session from the picker', () => {
  const ctx = useEnv('kill');
  let counter = 0;

  function makeSession(prefix) {
    const { env } = ctx;
    counter += 1;
    const name = `${prefix}-${counter}`;
    env.tmux.newSession(name, { cwd: env.dir(name) });
    return name;
  }

  async function openPicker(page) {
    if (!(await page.getByTestId('session-picker').isVisible())) await page.getByTestId('add-panel-btn').click();
    await page.getByTestId('session-picker').waitFor({ state: 'visible' });
  }

  it('every row has a kill button that opens the dialog, and Cancel keeps the session', TEST, async () => {
    const { env } = ctx;
    const [a, b] = [makeSession('kill-a'), makeSession('kill-b')];
    const page = await env.freshPage();
    await openPicker(page);
    const rows = await page.getByTestId('session-picker-item').count();
    assert.ok(rows >= 2);
    assert.equal(await page.getByTestId('session-picker-kill').count(), rows, 'one kill button per row');

    if (process.env.TMUX_WEB_SHOT_DIR) {
      fs.mkdirSync(process.env.TMUX_WEB_SHOT_DIR, { recursive: true });
      await page.screenshot({ path: `${process.env.TMUX_WEB_SHOT_DIR}/kill-picker.png` });
    }
    await killButton(page, a).click();
    const dialog = page.getByTestId('kill-dialog');
    await dialog.waitFor({ state: 'visible' });
    assert.match((await dialog.textContent()) ?? '', new RegExp(`Terminate “${a}”\\?`));
    assert.equal(await page.evaluate(() => document.activeElement?.getAttribute('data-testid')), 'kill-cancel-btn');
    assert.equal((await ui.panels(page)).length, 0, 'clicking the cross did not open a panel');
    if (process.env.TMUX_WEB_SHOT_DIR) await page.screenshot({ path: `${process.env.TMUX_WEB_SHOT_DIR}/kill-dialog.png` });

    await page.getByTestId('kill-cancel-btn').click();
    await dialog.waitFor({ state: 'hidden' });
    assert.ok(env.tmux.sessions().includes(a) && env.tmux.sessions().includes(b));
    assert.equal((await ui.panels(page)).length, 0);
    assert.deepEqual(page.__errors, []);
  });

  it('Terminate kills the session, its picker row and its panel', TEST, async () => {
    const { env } = ctx;
    const name = makeSession('kill-open');
    const page = await env.freshPage();
    const panel = await ui.addPanel(page, name);
    await ui.waitForPrompt(page, panel.id);

    await openPicker(page);
    await killButton(page, name).click();
    await page.getByTestId('kill-dialog').waitFor({ state: 'visible' });
    assert.equal(await page.getByTestId('session-picker').isVisible(), false, 'the picker closes behind the dialog');
    await page.getByTestId('kill-confirm-btn').click();
    await page.getByTestId('kill-dialog').waitFor({ state: 'hidden' });

    await waitFor('the tmux session to be gone', async () => !env.tmux.sessions().includes(name));
    await waitFor('the panel to close', async () => (await ui.panels(page)).every((p) => p.session !== name));
    await waitFor('the success toast', async () => (await ui.toasts(page)).includes(`Terminated “${name}”`));
    await openPicker(page);
    await waitFor('the picker row to disappear', async () => (await ui.pickerItem(page, name).count()) === 0);
    assert.deepEqual(page.__errors, []);
  });

  it('Escape closes the dialog without killing', TEST, async () => {
    const { env } = ctx;
    const name = makeSession('kill-esc');
    const page = await env.freshPage();
    await openPicker(page);
    await killButton(page, name).click();
    await page.getByTestId('kill-dialog').waitFor({ state: 'visible' });
    await page.keyboard.press('Escape');
    await page.getByTestId('kill-dialog').waitFor({ state: 'hidden' });
    assert.ok(env.tmux.sessions().includes(name));
    assert.deepEqual(page.__errors, []);
  });

  it('a failing kill shows the error in the dialog', TEST, async () => {
    const { env } = ctx;
    const name = makeSession('kill-gone');
    const page = await env.freshPage();
    await openPicker(page);
    await killButton(page, name).click();
    await page.getByTestId('kill-dialog').waitFor({ state: 'visible' });
    env.tmux.run('kill-session', '-t', `=${name}`); // isolated server: the session vanishes first
    await page.getByTestId('kill-confirm-btn').click();
    await waitFor('the error', async () => /no session|not found|can't find/i.test((await page.getByTestId('kill-error').textContent()) ?? ''));
    assert.equal(await page.getByTestId('kill-dialog').isVisible(), true, 'the dialog stays open');
    await page.getByTestId('kill-cancel-btn').click();
    assert.deepEqual(page.__errors, []);
  });
});
