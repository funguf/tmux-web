/**
 * More of CONTRACT §8.6: the top bar, Unicode width, the paste shortcuts and the context-menu
 * paste, Shift+right-click under tmux mouse mode, saved layouts that cannot be used, the empty
 * state and the auto-dismissing toasts.
 */
import assert from 'node:assert/strict';
import os from 'node:os';
import { before, describe, it } from 'node:test';
import * as ui from '../lib/app.js';
import { TEST, useEnv } from '../lib/suite.js';
import { sleep, staysFalse, waitFor } from '../lib/wait.js';

describe('top bar, paste, layouts that cannot be restored, empty state, toasts', () => {
  const ctx = useEnv('contract-ui');
  let counter = 0;

  function makeSession(prefix) {
    const { env } = ctx;
    counter += 1;
    const name = `${prefix}-${counter}`;
    env.tmux.newSession(name, { cwd: env.dir(name) });
    return name;
  }

  before(() => {
    makeSession('base');
  });

  async function openTerminal() {
    const { env } = ctx;
    const name = makeSession('term');
    const page = await env.freshPage();
    const panel = await ui.addPanel(page, name);
    await ui.waitForPrompt(page, panel.id);
    await ui.clickTerminal(page, panel.id);
    await ui.waitForTerminalFocus(page, panel.id);
    return { page, panel, name };
  }

  it('the top bar shows the host name and the tmux status', TEST, async () => {
    const { env } = ctx;
    const page = await env.freshPage();
    const hostname = os.hostname();
    assert.equal((await env.api('GET', '/api/info')).json.hostname, hostname);
    assert.equal(await page.getByTestId('app-title').textContent(), `tmux-web · ${hostname}`);
    assert.equal(await page.title(), `${hostname} · tmux-web`);
    const pill = page.getByTestId('status-pill');
    await waitFor('the green pill', async () => (await pill.getAttribute('data-state')) === 'ok');
    const sessions = env.tmux.sessions().length;
    assert.match(await pill.textContent(), new RegExp(`^tmux \\d+\\.\\d+[a-z]? · ${sessions} sessions?$`));
    assert.match((await pill.getAttribute('title')) ?? '', /tmux socket: -L tmw-e2e-/, 'the tooltip names the socket');

    // A session created behind tmux-web's back shows up in the count.
    makeSession('pill');
    await waitFor('the count to follow', async () => new RegExp(` · ${sessions + 1} sessions$`).test(await pill.textContent()));

    // Fullscreen toggle.
    const toggle = page.getByTestId('fullscreen-btn');
    if (await toggle.isVisible()) {
      await toggle.click();
      await waitFor('fullscreen', () => page.evaluate(() => document.fullscreenElement !== null));
      await toggle.click();
      await waitFor('to leave fullscreen', () => page.evaluate(() => document.fullscreenElement === null));
    }
    assert.deepEqual(page.__errors, []);
  });

  it('the picker rows show windows, attached clients, the Claude badge and the open mark', TEST, async () => {
    const { env } = ctx;
    const name = makeSession('rowinfo');
    env.tmux.run('new-window', '-t', `=${name}:`, '-c', env.dir('rowinfo'));
    const page = await env.freshPage();
    await ui.addPanel(page, name);
    await waitFor('the client to attach', () => env.tmux.clientCount(name) === 1);
    await page.getByTestId('add-panel-btn').click();
    const row = ui.pickerItem(page, name);
    await waitFor('the row to show 2 windows and 1 attached', async () => /2 windows · 1 attached/.test(await row.getByTestId('session-picker-meta').textContent()));
    assert.equal(await row.getAttribute('data-open'), 'true');
    assert.equal(await row.getByTestId('session-picker-open').textContent(), 'open');
    assert.equal(await row.getByTestId('session-picker-claude').count(), 0, 'no Claude here');
  });

  it('Unicode 11 widths: wide characters take two columns', TEST, async () => {
    const { page, panel } = await openTerminal();
    assert.equal(await page.evaluate((id) => window.__tmuxWeb.terminal(id).unicode.activeVersion, panel.id), '11');
    await ui.typeLine(page, 'echo 日本語-x');
    await ui.waitForScreen(page, panel.id, (line) => line === '日本語-x');
    const widths = await page.evaluate((id) => {
      const term = window.__tmuxWeb.terminal(id);
      const buffer = term.buffer.active;
      for (let y = 0; y < term.rows; y += 1) {
        const line = buffer.getLine(buffer.viewportY + y);
        if (line?.translateToString(true) === '日本語-x') return Array.from({ length: 8 }, (_, x) => line.getCell(x)?.getWidth());
      }
      return null;
    }, panel.id);
    assert.deepEqual(widths, [2, 0, 2, 0, 2, 0, 1, 1], 'each CJK character covers two cells');
  });

  it('Ctrl+Shift+V, Shift+Insert and the context menu paste text', TEST, async () => {
    const { page, panel } = await openTerminal();
    await ui.writeClipboard(page, 'one');
    await page.keyboard.press('Control+Shift+V');
    await ui.waitForScreen(page, panel.id, (line) => line === '$ one');
    await ui.writeClipboard(page, 'two');
    await page.keyboard.press('Shift+Insert');
    await ui.waitForScreen(page, panel.id, (line) => line === '$ onetwo');

    await ui.writeClipboard(page, 'three');
    const box = await ui.terminalPanel(page, panel.id).boundingBox();
    await page.mouse.click(box.x + 300, box.y + 300, { button: 'right' });
    await page.locator('[data-testid="context-menu-item"][data-action="paste"]').click();
    await ui.waitForScreen(page, panel.id, (line) => line === '$ onetwothree');
    // None of it reached the shell as a literal ^V.
    assert.ok(!(await ui.screenLines(page, panel.id)).some((line) => line.includes('^V')));
    assert.deepEqual(page.__errors, []);
  });

  it('under tmux mouse mode a plain right click is left to tmux, Shift+right click opens the menu', TEST, async () => {
    const { env } = ctx;
    const name = makeSession('ctxmouse');
    env.tmux.run('set-option', '-g', 'mouse', 'on');
    const page = await env.freshPage();
    const panel = await ui.addPanel(page, name);
    await ui.waitForPrompt(page, panel.id);
    await waitFor('mouse tracking', async () => (await ui.mouseMode(page, panel.id)) !== 'none');
    const box = await ui.terminalPanel(page, panel.id).boundingBox();
    const menu = page.getByTestId('context-menu');

    await page.mouse.click(box.x + 300, box.y + 200, { button: 'right' });
    await staysFalse('the app menu opened although tmux tracks the mouse', () => menu.isVisible(), { duration: 700 });
    await page.keyboard.press('Escape'); // closes tmux's own menu, if it drew one

    await page.keyboard.down('Shift');
    await page.mouse.click(box.x + 300, box.y + 200, { button: 'right' });
    await page.keyboard.up('Shift');
    await waitFor('the app menu', () => menu.isVisible());
    await page.keyboard.press('Escape');
    env.tmux.run('set-option', '-g', 'mouse', 'off');
  });

  it('a saved layout that cannot be restored starts empty with a message, and saving works afterwards', TEST, async () => {
    const { env } = ctx;
    const base = env.tmux.sessions()[0];
    const broken = { schema: 1, dockview: { grid: { root: { type: 'branch', data: 'not a tree' }, width: 10, height: 10, orientation: 'HORIZONTAL' }, panels: { 'p-x': { id: 'p-x', contentComponent: 'terminal', title: 'x' } } }, maximizedPanelId: null, prefs: {} };
    await env.api('PUT', '/api/layout', { layout: broken, client_id: 'e2e-broken' });
    const page = await env.freshPage({ resetLayout: false });
    await waitFor('the error toast', async () => (await ui.toasts(page)).some((text) => /could not be restored/i.test(text)));
    assert.deepEqual(await ui.panels(page), [], 'starts empty');
    await page.getByTestId('empty-state').waitFor({ state: 'visible' });

    // The app still works and replaces the broken document with a good one.
    await ui.addPanel(page, base);
    await waitFor('a good layout to be saved', async () => Object.keys((await env.savedLayout()).layout?.dockview?.panels ?? {}).length === 1, { timeout: 10_000 });
    await ui.reloadApp(page);
    assert.deepEqual((await ui.panels(page)).map((panel) => panel.session), [base]);

    // An unknown schema version is refused the same way.
    await env.closePage();
    await env.api('PUT', '/api/layout', { layout: { schema: 99, dockview: {}, maximizedPanelId: null }, client_id: 'e2e-future' });
    const second = await env.freshPage({ resetLayout: false });
    await waitFor('the format toast', async () => (await ui.toasts(second)).some((text) => /unknown format/i.test(text)));
    assert.deepEqual(await ui.panels(second), []);
  });

  it('the empty state creates a session, lists sessions and opens them all', TEST, async () => {
    const { env } = ctx;
    const page = await env.freshPage();
    const empty = page.getByTestId('empty-state');
    await empty.waitFor({ state: 'visible' });
    const names = env.tmux.sessions();
    const listed = await page.getByTestId('empty-session-item').evaluateAll((items) => items.map((item) => item.dataset.session));
    assert.deepEqual([...listed].sort(), names);

    await page.getByTestId('empty-new-session-input').fill('from empty state');
    await page.getByTestId('empty-new-session-input').press('Enter');
    const created = await ui.waitForPanel(page, 'from empty state');
    assert.ok(env.tmux.sessions().includes('from empty state'));
    await ui.waitForPrompt(page, created.id);
    await empty.waitFor({ state: 'hidden' });

    // Close it again: the empty state returns, and "Open all" adds every session as a tab.
    await ui.tabFor(page, 'from empty state').getByTestId('tab-close').click();
    await empty.waitFor({ state: 'visible' });
    const all = env.tmux.sessions();
    await page.getByTestId('empty-open-all-btn').click();
    await waitFor('a panel per session', async () => (await ui.panels(page)).length === all.length);
    assert.deepEqual((await ui.panels(page)).map((panel) => panel.session).sort(), all);
    assert.equal((await ui.groups(page)).length, 1, 'all as tabs of one group');
    assert.deepEqual(page.__errors, []);
  });

  it('toasts dismiss themselves after about four seconds', TEST, async () => {
    const { page, panel } = await openTerminal();
    // The app's own toast host, as a real action would use it.
    await page.evaluate(async () => (await import('/js/toast.js')).toast.success('Something happened'));
    const toast = page.getByTestId('toast');
    await waitFor('the toast', async () => (await toast.count()) === 1);
    const shown = Date.now();
    await sleep(2500);
    assert.equal(await toast.count(), 1, 'still there after 2.5 s');
    await waitFor('the toast to go', async () => (await toast.count()) === 0, { timeout: 4000 });
    const lived = Date.now() - shown;
    assert.ok(lived >= 3500 && lived <= 5500, `the toast lived ${lived} ms`);
    assert.ok(panel.id);
  });
});
