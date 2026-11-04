/**
 * Requirement 5: the layout persists. Reload the page: the same panels, groups and maximised
 * panel. Restart the tmux-web process (and use a brand new browser profile): the same.
 */
import assert from 'node:assert/strict';
import fs from 'node:fs';
import { before, describe, it } from 'node:test';
import * as ui from '../lib/app.js';
import { TEST, useEnv } from '../lib/suite.js';
import { waitFor } from '../lib/wait.js';

const SLACK = 2;

/** What the user sees of the layout: panels, groups and which one is maximised. */
async function captureState(page) {
  const panels = (await ui.panels(page))
    .map(({ id, session, groupId, maximized }) => ({ id, session, groupId, maximized }))
    .sort((x, y) => x.id.localeCompare(y.id));
  const groups = await ui.groups(page);
  return {
    panels,
    groups: groups.map(({ id, sessions, activePanelId, maximized }) => ({ id, sessions, activePanelId, maximized })).sort((x, y) => x.id.localeCompare(y.id)),
    rects: Object.fromEntries(groups.map((group) => [group.id, group.rect])),
    tabs: await ui.tabTitles(page),
  };
}

function assertSameState(actual, expected, label) {
  assert.deepEqual(actual.panels, expected.panels, `${label}: same panels, groups and maximised flags`);
  assert.deepEqual(actual.groups, expected.groups, `${label}: same groups, tab sets and active tabs`);
  assert.deepEqual([...actual.tabs].sort(), [...expected.tabs].sort(), `${label}: same tabs`);
  for (const [id, rect] of Object.entries(expected.rects)) {
    for (const key of ['x', 'y', 'width', 'height']) {
      assert.ok(Math.abs(actual.rects[id][key] - rect[key]) <= SLACK, `${label}: group ${id} ${key} ${actual.rects[id][key]} vs ${rect[key]}`);
    }
  }
}

describe('layout persistence', () => {
  const ctx = useEnv('persist');

  before(() => {
    const { env } = ctx;
    for (const name of ['pe-a', 'pe-b', 'pe-c']) env.tmux.newSession(name, { cwd: env.dir(name) });
  });

  /** Two groups ([pe-a, pe-c] and [pe-b]); pe-a active in the first; pe-b's group maximised. Waits until saved. */
  async function buildLayout(page) {
    const { env } = ctx;
    await ui.addPanel(page, 'pe-a');
    await ui.addPanel(page, 'pe-c');
    const b = await ui.splitFrom(page, 'pe-a', 'right', 'pe-b');
    await ui.tabFor(page, 'pe-a').click();
    await waitFor('pe-a to be the active tab', () => page.evaluate(() => window.__tmuxWeb.api.activePanel?.params?.session === 'pe-a'));
    await (await ui.groupControl(page, 'pe-b', 'header-maximize')).click();
    await waitFor('pe-b to be maximised', async () => (await ui.panelOf(page, 'pe-b'))?.maximized === true);
    await waitFor(
      'the layout to be saved on the server',
      async () => {
        const saved = await env.savedLayout();
        return saved.layout?.maximizedPanelId === b.id && Object.keys(saved.layout?.dockview?.panels ?? {}).length === 3;
      },
      { timeout: 10_000 },
    );
    return captureState(page);
  }

  it('reloading the page restores the same panels, groups and maximised panel', TEST, async () => {
    const { env } = ctx;
    const page = await env.freshPage();
    const before = await buildLayout(page);
    assert.equal(before.panels.length, 3);
    assert.equal(before.groups.length, 2);
    assert.deepEqual(before.panels.filter((panel) => panel.maximized).map((panel) => panel.session), ['pe-b']);

    await ui.reloadApp(page);
    const after = await captureState(page);
    assertSameState(after, before, 'after reload');

    // The restored panels are live: the visible one attaches; restoring the maximised group
    // brings the others up, with the layout they had.
    const b = await ui.waitForPanel(page, 'pe-b');
    await ui.waitForPrompt(page, b.id);
    await (await ui.groupControl(page, 'pe-b', 'header-maximize')).click();
    await waitFor('the others to attach once visible', async () => (await ui.panels(page)).filter((panel) => panel.connected).length >= 2);
    const restored = await ui.panels(page);
    assert.ok(restored.every((panel) => !panel.maximized));
    const groups = await ui.groups(page);
    assert.deepEqual(groups.find((group) => group.sessions.includes('pe-a')).sessions, ['pe-a', 'pe-c'], 'the tab stack survived');
    assert.deepEqual(page.__errors, []);
  });

  it('restarting tmux-web, and opening a new browser profile, keeps the same layout', TEST, async () => {
    const { env } = ctx;
    const page = await env.freshPage();
    const before = await buildLayout(page);
    const saved = await env.savedLayout();
    assert.ok(saved.version >= 1 && saved.updated_at > 0);
    const listen = env.server.address;
    assert.ok(fs.existsSync(`${env.stateDir}/layout.json`), 'the layout is a file in the state directory');

    // "Closing Chrome": drop the whole browser context, then restart the server.
    await env.closePage();
    await env.server.stop();
    await env.server.start({ listen });
    assert.equal(env.server.address, listen, 'same address after the restart');
    const afterRestart = await env.savedLayout();
    assert.deepEqual(afterRestart, saved, 'the server returns exactly the stored layout after a restart');

    const fresh = await env.freshPage({ resetLayout: false });
    const after = await captureState(fresh);
    assertSameState(after, before, 'after restart');
    const b = await ui.waitForPanel(fresh, 'pe-b');
    await ui.waitForPrompt(fresh, b.id);
    assert.deepEqual(fresh.__errors, []);
  });

  it('a change made just before the page goes away is not lost', TEST, async () => {
    const { env } = ctx;
    const page = await env.freshPage();
    await ui.addPanel(page, 'pe-a');
    await waitFor('the first panel to be saved', async () => Object.keys((await env.savedLayout()).layout?.dockview?.panels ?? {}).length === 1, { timeout: 10_000 });
    const saved = (await env.savedLayout()).version;

    // Add a second panel and leave at once: well inside the 300 ms save delay.
    await page.getByTestId('add-panel-btn').click();
    await ui.pickerItem(page, 'pe-c').click();
    await page.goto('about:blank');
    await waitFor(
      'the page-hide flush to save the new panel',
      async () => {
        const layout = await env.savedLayout();
        return layout.version > saved && Object.keys(layout.layout?.dockview?.panels ?? {}).length === 2;
      },
      { timeout: 10_000 },
    );

    const fresh = await env.freshPage({ resetLayout: false });
    assert.deepEqual((await ui.panels(fresh)).map((panel) => panel.session).sort(), ['pe-a', 'pe-c']);
  });

  it('preferences are saved with the layout', TEST, async () => {
    const { env } = ctx;
    const page = await env.freshPage();
    const a = await ui.addPanel(page, 'pe-a');
    assert.equal(await page.evaluate((id) => window.__tmuxWeb.terminal(id).options.fontSize, a.id), 13, 'the default font size');
    await page.getByTestId('settings-btn').click();
    await page.getByTestId('font-size-inc').click();
    await page.getByTestId('font-size-inc').click();
    await page.getByTestId('copy-on-select-toggle').uncheck();
    assert.equal(await page.getByTestId('font-size-value').textContent(), '15');
    await waitFor(
      'the preferences to be saved',
      async () => {
        const prefs = (await env.savedLayout()).layout?.prefs;
        return prefs?.fontSize === 15 && prefs?.copyOnSelect === false;
      },
      { timeout: 10_000 },
    );

    await ui.reloadApp(page);
    const reloaded = await ui.waitForPanel(page, 'pe-a');
    assert.equal(await page.evaluate((id) => window.__tmuxWeb.terminal(id).options.fontSize, reloaded.id), 15, 'the terminal uses the saved font size');
    await page.getByTestId('settings-btn').click();
    assert.equal(await page.getByTestId('font-size-value').textContent(), '15');
    assert.equal(await page.getByTestId('copy-on-select-toggle').isChecked(), false);
    assert.deepEqual(page.__errors, []);
  });
});
