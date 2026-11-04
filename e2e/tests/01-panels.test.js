/**
 * Requirement 1: add panels from tmux sessions; the tab title equals the tmux session name;
 * renaming the session in tmux renames the tab.
 */
import assert from 'node:assert/strict';
import { describe, it } from 'node:test';
import * as ui from '../lib/app.js';
import { TEST, useEnv } from '../lib/suite.js';
import { waitFor, waitUntil } from '../lib/wait.js';

/** The saved layout's panel records: {id: {title, params, …}}. */
async function savedPanels(env) {
  const saved = await env.savedLayout();
  return saved.layout?.dockview?.panels ?? {};
}

describe('panels from tmux sessions', () => {
  const ctx = useEnv('panels');

  it('adds a panel per picked session and titles every tab with the exact session name', TEST, async () => {
    const { env } = ctx;
    // Spaces and non-ASCII characters must survive the picker, the tab and the attach URL.
    const names = ['alpha', 'beta', 'my proj ünï'];
    for (const name of names) env.tmux.newSession(name, { cwd: env.dir(name.replace(/\W/g, '_')) });
    const page = await env.freshPage();

    await waitFor('the status pill to count 3 sessions', async () => /3 sessions/.test(await page.getByTestId('status-pill').textContent()));
    assert.match(await page.getByTestId('status-pill').textContent(), /^tmux \d/);
    assert.deepEqual(await ui.panels(page), [], 'no panel yet');
    await waitFor('the empty state', () => page.getByTestId('empty-state').isVisible());

    // The picker has one row per tmux session, and marks none of them as open yet.
    await page.getByTestId('add-panel-btn').click();
    await page.getByTestId('session-picker').waitFor({ state: 'visible' });
    const rows = await page.getByTestId('session-picker-item').evaluateAll((items) => items.map((item) => item.dataset.session));
    assert.deepEqual([...rows].sort(), [...names].sort());
    assert.equal(await page.getByTestId('session-picker-open').count(), 0);
    await page.keyboard.press('Escape');
    await page.getByTestId('session-picker').waitFor({ state: 'hidden' });

    for (const name of names) {
      await ui.addPanel(page, name);
      const tab = ui.tabFor(page, name);
      assert.equal(await tab.count(), 1, `one tab for ${name}`);
      assert.equal(await tab.textContent(), name, 'the tab shows nothing but the session name');
      assert.equal(await tab.getByTestId('tab-title').textContent(), name);
    }
    assert.deepEqual(await ui.tabTitles(page), names, 'one tab per session, in the order they were added');
    assert.equal((await ui.panels(page)).length, 3);

    // Each tab is a terminal on its own session: the tmux status line names the session, and
    // typed input lands in that session and nowhere else.
    for (const name of [...names].reverse()) {
      await ui.tabFor(page, name).click();
      const panel = await ui.waitForPanel(page, name);
      await ui.waitForScreen(page, panel.id, `[${name}]`);
      await ui.waitForTerminalFocus(page, panel.id); // clicking a tab (also the active one) focuses its terminal
      const marker = `typed-in-${name.replace(/\W/g, '')}`;
      await ui.typeLine(page, `echo ${marker}`);
      await ui.waitForScreen(page, panel.id, (line) => line === marker);
      await waitFor(`tmux to show ${marker} in session ${name}`, () => env.tmux.capture(`=${name}:`).includes(marker));
      for (const other of names.filter((n) => n !== name)) {
        assert.ok(!env.tmux.capture(`=${other}:`).includes(marker), `${marker} must not reach ${other}`);
      }
    }

    // Opening the picker again marks the open sessions; choosing one focuses its panel.
    await page.getByTestId('add-panel-btn').click();
    await waitFor('every picker row marked open', async () => (await page.getByTestId('session-picker-open').count()) === 3);
    await ui.pickerItem(page, 'alpha').click();
    await waitFor('the alpha panel to be active', () => page.evaluate(() => window.__tmuxWeb.api.activePanel?.params?.session === 'alpha'));
    assert.equal((await ui.panels(page)).length, 3, 'no duplicate panel');
    assert.deepEqual(page.__errors, []);
  });

  it('handles the longest session name tmux-web accepts', TEST, async () => {
    const { env } = ctx;
    const name = `long-${'x'.repeat(95)}`; // 100 characters
    assert.equal(name.length, 100);
    env.tmux.newSession(name, { cwd: env.dir('long') });
    const page = await env.freshPage();
    const panel = await ui.addPanel(page, name);
    await ui.waitForPrompt(page, panel.id);
    assert.equal(await ui.tabFor(page, name).textContent(), name, 'the whole name is the tab title, however it is displayed');
    assert.equal(await ui.tabFor(page, name).getAttribute('title'), name, 'the full name is in the tooltip');
    await ui.clickTerminal(page, panel.id);
    await ui.waitForTerminalFocus(page, panel.id);
    await ui.typeLine(page, 'echo long-name-ok');
    await ui.waitForScreen(page, panel.id, (line) => line === 'long-name-ok');
    env.tmux.run('kill-session', '-t', `=${name}`);
  });

  it('renames the tab when the session is renamed in tmux, and keeps the terminal attached', TEST, async () => {
    const { env } = ctx;
    env.tmux.newSession('ren-one', { cwd: env.dir('ren-one') });
    const page = await env.freshPage();
    const original = await ui.addPanel(page, 'ren-one');
    await ui.waitForScreen(page, original.id, '[ren-one]');
    const sockets = ui.attachSockets(page).length;

    const renames = ['ren-one-renamed', 'ünï name 2'];
    let previous = 'ren-one';
    for (const next of renames) {
      env.tmux.run('rename-session', '-t', `=${previous}`, next);
      await waitFor(`the tab to be renamed to ${next}`, async () => (await ui.tabFor(page, next).count()) === 1, { timeout: 15_000 });
      assert.equal(await ui.tabFor(page, previous).count(), 0, 'the old title is gone');
      assert.equal(await ui.tabFor(page, next).textContent(), next);
      const panel = await ui.panelOf(page, next);
      assert.equal(panel.id, original.id, 'the same panel, not a new one');
      assert.equal(panel.connected, true);
      // The attach client follows the session: the new name shows up in tmux's status line, and
      // the terminal still takes input.
      await ui.waitForScreen(page, panel.id, `[${next}]`);
      await ui.clickTerminal(page, panel.id);
      const marker = `after-rename-${renames.indexOf(next)}`;
      await ui.typeLine(page, `echo ${marker}`);
      await ui.waitForScreen(page, panel.id, (line) => line === marker);
      previous = next;
    }
    assert.equal(ui.attachSockets(page).length, sockets, 'renaming does not reconnect the terminal');

    // The layout on the server follows the rename (title and params), so a reload keeps it.
    const finalName = renames.at(-1);
    await waitFor(
      'the saved layout to carry the new name',
      async () => {
        const saved = (await savedPanels(env))[original.id];
        return saved?.title === finalName && saved.params?.session === finalName;
      },
      { timeout: 10_000 },
    );
    await page.reload();
    await page.evaluate(() => window.__tmuxWeb.ready);
    const reloaded = await ui.waitForPanel(page, finalName);
    assert.equal(reloaded.id, original.id);
    assert.equal(await ui.tabFor(page, finalName).textContent(), finalName);
    await ui.waitForScreen(page, reloaded.id, `[${finalName}]`);
    assert.deepEqual(page.__errors, []);
  });

  it('renames a tab that has never been shown, which then attaches to the new name', TEST, async () => {
    const { env } = ctx;
    // Only these three sessions may exist, so that "Open all" opens exactly them.
    for (const name of ['lazy-a', 'lazy-b', 'lazy-c']) env.tmux.newSession(name, { cwd: env.dir(name) });
    for (const name of env.tmux.sessions().filter((n) => !n.startsWith('lazy-'))) env.tmux.run('kill-session', '-t', `=${name}`);
    const page = await env.freshPage();
    await waitFor('3 sessions in the pill', async () => /3 sessions/.test(await page.getByTestId('status-pill').textContent()));

    await page.getByTestId('add-panel-btn').click();
    await page.getByTestId('open-all-btn').click();
    await waitFor('three panels', async () => (await ui.panels(page)).length === 3);
    const first = await ui.waitForPanel(page, 'lazy-a');
    assert.equal(first.connected, true, 'the visible tab attached');
    // Lazy attach: the two tabs that were never visible hold no connection yet.
    const hidden = (await ui.panels(page)).filter((panel) => panel.session !== 'lazy-a');
    assert.equal(hidden.length, 2);
    assert.ok(hidden.every((panel) => !panel.connected), 'hidden tabs are not attached');
    assert.equal(ui.attachSockets(page).length, 1, 'only the visible panel opened a socket');

    env.tmux.run('rename-session', '-t', '=lazy-b', 'lazy-b-renamed');
    await waitFor('the hidden tab to be renamed', async () => (await ui.tabFor(page, 'lazy-b-renamed').count()) === 1, { timeout: 15_000 });
    assert.equal(await ui.tabFor(page, 'lazy-b-renamed').textContent(), 'lazy-b-renamed');
    assert.equal(await ui.tabFor(page, 'lazy-b').count(), 0);

    // Showing it attaches to the renamed session, not to the old name.
    await ui.tabFor(page, 'lazy-b-renamed').click();
    const renamed = await ui.waitForPanel(page, 'lazy-b-renamed');
    await ui.waitForScreen(page, renamed.id, '[lazy-b-renamed]');
    assert.equal(await page.getByTestId('panel-overlay').first().isVisible(), false, 'no "session gone" overlay');
    const toastTexts = await ui.toasts(page);
    assert.deepEqual(toastTexts.filter((text) => /error|failed/i.test(text)), []);
    await waitUntil('the saved layout to hold the renamed tab', async () => (await savedPanels(env))[renamed.id]?.title, (title) => title === 'lazy-b-renamed', { timeout: 10_000 });
    assert.deepEqual(page.__errors, []);
  });
});
