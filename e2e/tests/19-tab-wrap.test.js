/**
 * A group whose header is too narrow for all of its tabs wraps them onto more rows instead of
 * hiding some: every tab stays fully visible and clickable, the header grows to fit the rows,
 * and the terminal below shrinks to match (and grows back once the tabs fit on one row again).
 */
import assert from 'node:assert/strict';
import { before, describe, it } from 'node:test';
import * as ui from '../lib/app.js';
import { TEST, useEnv } from '../lib/suite.js';
import { waitFor } from '../lib/wait.js';

const SESSIONS = Array.from({ length: 10 }, (_, index) => `wrap-session-${index + 1}`);
const NARROW = { width: 640, height: 600 };
const WIDE = { width: 2400, height: 600 };

/** Geometry of the only group: its header, content area, terminal screen and every tab. */
function geometry(page) {
  return page.evaluate(() => {
    const box = (element) => {
      const rect = element.getBoundingClientRect();
      return { top: rect.top, bottom: rect.bottom, left: rect.left, right: rect.right, height: rect.height };
    };
    const group = document.querySelector('.dv-groupview');
    const header = group.querySelector('.dv-tabs-and-actions-container');
    const tabs = [...header.querySelectorAll('.dv-tab')].map((tab) => {
      const rect = box(tab);
      const hit = document.elementFromPoint((rect.left + rect.right) / 2, (rect.top + rect.bottom) / 2);
      return { ...rect, session: tab.querySelector('[data-testid="tab"]')?.dataset.session ?? null, hittable: tab.contains(hit) };
    });
    // Panels render in dockview's overlay ("always" renderer), not inside the group element.
    const activeId = window.__tmuxWeb.api.activePanel?.id;
    const visibleScreen = document.querySelector(`[data-testid="terminal-panel"][data-panel-id="${activeId}"] .xterm-screen`);
    return {
      viewport: { width: window.innerWidth, height: window.innerHeight },
      group: box(group),
      header: box(header),
      content: box(group.querySelector('.dv-content-container')),
      screen: visibleScreen ? box(visibleScreen) : null,
      // dockview's own idea of the content height (it subtracts a cached header height)
      panelHeight: window.__tmuxWeb.api.activePanel?.api.height ?? null,
      tabs,
    };
  });
}

const rows = (layout) => new Set(layout.tabs.map((tab) => Math.round(tab.top))).size;

/** The active terminal's size in cells. */
async function activeTermSize(page) {
  const active = await page.evaluate(() => window.__tmuxWeb.api.activePanel?.id ?? null);
  return ui.termSize(page, active);
}

describe('tabs wrap onto more rows in a narrow window', () => {
  const ctx = useEnv('tab-wrap');

  before(() => {
    for (const name of SESSIONS) ctx.env.tmux.newSession(name, { cwd: ctx.env.dir(name) });
  });

  it('every tab stays visible, the header grows and the terminal shrinks to fit', TEST, async () => {
    const page = await ctx.env.freshPage({ viewport: NARROW });
    for (const name of SESSIONS) await ui.addPanel(page, name);
    assert.equal((await ui.groups(page)).length, 1, 'all panels share one group');

    const narrow = await waitFor('the tabs wrap onto more than one row', async () => {
      const layout = await geometry(page);
      return rows(layout) > 1 ? layout : null;
    });

    assert.deepEqual(
      narrow.tabs.map((tab) => tab.session),
      SESSIONS,
      'every session has a tab, in order',
    );
    for (const tab of narrow.tabs) {
      assert.ok(tab.left >= narrow.header.left - 0.5 && tab.right <= narrow.header.right + 0.5, `${tab.session}: inside the header horizontally`);
      assert.ok(tab.top >= narrow.header.top - 0.5 && tab.bottom <= narrow.header.bottom + 0.5, `${tab.session}: inside the header vertically`);
      assert.ok(tab.right <= narrow.viewport.width + 0.5, `${tab.session}: inside the window`);
      assert.ok(tab.hittable, `${tab.session}: not covered or clipped (its centre hits the tab)`);
    }
    assert.ok(narrow.header.height > 40, `the header grew to fit the rows (height ${narrow.header.height})`);
    assert.ok(narrow.content.top >= narrow.header.bottom - 0.5, 'the content starts below the header');
    assert.ok(narrow.content.bottom <= narrow.group.bottom + 0.5, 'the content shrank to fit inside the group');
    assert.ok(narrow.screen && narrow.screen.bottom <= narrow.group.bottom + 0.5, 'the terminal screen fits inside the group');
    assert.ok(Math.abs(narrow.panelHeight - narrow.content.height) <= 1, `dockview's panel height matches the content (${narrow.panelHeight} vs ${narrow.content.height})`);

    // A tab on the last row can be clicked and activates its panel.
    const last = narrow.tabs.at(-1);
    await page.mouse.click((last.left + last.right) / 2, (last.top + last.bottom) / 2);
    await waitFor(`${last.session} becomes the active panel`, () =>
      page.evaluate((session) => window.__tmuxWeb.api.activePanel?.params?.session === session, last.session),
    );
    const narrowSize = await activeTermSize(page);

    // Wide enough for one row: the header is back to one row and the terminal regains the rows.
    await page.setViewportSize(WIDE);
    const wide = await waitFor('the tabs fit on one row again', async () => {
      const layout = await geometry(page);
      return rows(layout) === 1 && layout.header.height <= 33 ? layout : null;
    });
    assert.ok(wide.content.top >= wide.header.bottom - 0.5, 'wide: the content starts below the header');
    const wideSize = await waitFor('the terminal grows back', async () => {
      const size = await activeTermSize(page);
      return size && size.rows > narrowSize.rows ? size : null;
    });
    assert.ok(wideSize.rows > narrowSize.rows, `more terminal rows once the header is one row (${narrowSize.rows} -> ${wideSize.rows})`);

    // And narrow again: wraps again and the terminal shrinks again.
    await page.setViewportSize(NARROW);
    const again = await waitFor('the tabs wrap again', async () => {
      const layout = await geometry(page);
      return rows(layout) > 1 ? layout : null;
    });
    await waitFor('the terminal fits under the taller header again', async () => {
      const layout = await geometry(page);
      return layout.screen && layout.screen.bottom <= layout.group.bottom + 0.5 && layout.content.top >= layout.header.bottom - 0.5;
    });
    assert.ok(again.header.height > 40);
  });

  it('a tab that gets wider and starts a new row pushes the terminal down (no window resize)', TEST, async () => {
    const { env } = ctx;
    const page = await env.freshPage({ viewport: NARROW });
    const opened = SESSIONS.slice(0, 9);
    for (const name of opened) await ui.addPanel(page, name);
    const before = await waitFor('nine tabs settle on three full rows', async () => {
      const layout = await geometry(page);
      return layout.tabs.length === 9 && rows(layout) === 3 ? layout : null;
    });

    // Renaming in tmux renames the tab; the longer title widens it, so a tab no longer fits.
    const renamed = 'wrap-session-1-renamed-to-something-much-longer';
    env.tmux.run('rename-session', '-t', `=${opened[0]}`, renamed);
    const after = await waitFor('the wider tab pushes a tab onto a fourth row', async () => {
      const layout = await geometry(page);
      return layout.tabs[0]?.session === renamed && rows(layout) === 4 ? layout : null;
    });
    assert.ok(after.header.height > before.header.height, 'the header grew');

    const settled = await waitFor('the terminal moves below the new row and every tab can be clicked', async () => {
      const layout = await geometry(page);
      const fits = layout.screen && layout.screen.top >= layout.header.bottom - 0.5 && layout.screen.bottom <= layout.group.bottom + 0.5;
      return fits && layout.tabs.every((tab) => tab.hittable) ? layout : null;
    });
    assert.ok(settled.content.top >= settled.header.bottom - 0.5);
    await waitFor('dockview re-measures the taller header (panel height = content height)', async () => {
      const layout = await geometry(page);
      return Math.abs(layout.panelHeight - layout.content.height) <= 1 ? layout : null;
    });

    // A restored layout wraps too.
    await ui.reloadApp(page);
    await waitFor('the restored layout wraps its tabs onto four rows', async () => {
      const layout = await geometry(page);
      return layout.tabs.length === 9 && rows(layout) === 4 && layout.tabs.every((tab) => tab.hittable) ? layout : null;
    });
  });
});
