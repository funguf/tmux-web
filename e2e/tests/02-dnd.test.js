/**
 * Requirement 2: panels move. Drag a tab into another group (it stacks as a tab) and onto a
 * group edge (it creates a split). Real mouse input, dockview's own HTML5 drag and drop.
 */
import assert from 'node:assert/strict';
import { before, describe, it } from 'node:test';
import * as ui from '../lib/app.js';
import { TEST, useEnv } from '../lib/suite.js';
import { waitFor } from '../lib/wait.js';

/** Rounding slack for rectangles that must touch or line up. */
const SLACK = 2;

const byId = (all, id) => all.find((group) => group.id === id);

describe('moving panels with the mouse', () => {
  const ctx = useEnv('dnd');

  before(() => {
    const { env } = ctx;
    for (const name of ['mv-a', 'mv-b', 'mv-c', 'edge-x', 'edge-y']) env.tmux.newSession(name, { cwd: env.dir(name) });
  });

  it('dragging a tab into another group stacks it as a tab there', TEST, async () => {
    const page = await ctx.env.freshPage();
    const a = await ui.addPanel(page, 'mv-a');
    const b = await ui.splitFrom(page, 'mv-a', 'right', 'mv-b');
    let all = await ui.groups(page);
    assert.equal(all.length, 2, 'two groups after the split');
    assert.notEqual(a.groupId, b.groupId);
    const sockets = ui.attachSockets(page).length;

    // Drop in the middle of mv-a's terminal area.
    const target = await ui.terminalPanel(page, a.id).boundingBox();
    await ui.dragTabTo(page, 'mv-b', target);

    await waitFor('both panels to be in one group', async () => (await ui.groups(page)).length === 1);
    all = await ui.groups(page);
    assert.deepEqual(all[0].sessions, ['mv-a', 'mv-b'], 'the dropped tab is appended to the target group');
    assert.equal(all[0].activePanelId, b.id, 'the dropped panel is the active tab');
    assert.deepEqual(await ui.tabTitles(page), ['mv-a', 'mv-b']);
    assert.equal(ui.attachSockets(page).length, sockets, 'moving a panel does not reconnect it');
    const moved = await ui.waitForPanel(page, 'mv-b');
    assert.equal(moved.id, b.id);
    assert.equal(moved.groupId, a.groupId, 'mv-b is now in mv-a\'s group');

    // The moved terminal still works and is shown at full size.
    const box = await ui.terminalPanel(page, b.id).boundingBox();
    assert.ok(box.width > 1000 && box.height > 400, `the group fills the work area (${JSON.stringify(box)})`);
    await ui.clickTerminal(page, b.id);
    await ui.typeLine(page, 'echo moved-ok');
    await ui.waitForScreen(page, b.id, (line) => line === 'moved-ok');
    assert.deepEqual(page.__errors, []);
  });

  for (const edge of ['right', 'bottom', 'left', 'top']) {
    it(`dragging a tab onto the ${edge} edge of a group splits it`, TEST, async () => {
      const page = await ctx.env.freshPage();
      const y = await ui.addPanel(page, 'edge-y');
      const x = await ui.addPanel(page, 'edge-x'); // the second tab of the same group
      assert.equal(x.groupId, y.groupId);
      assert.equal((await ui.groups(page)).length, 1);
      const sockets = ui.attachSockets(page).length;

      // Drop 12 px inside the edge of the group's terminal area, mid-way along it.
      const area = await ui.terminalPanel(page, x.id).boundingBox();
      const inset = { right: { x: -12 }, left: { x: 12 }, bottom: { y: -12 }, top: { y: 12 } }[edge];
      // Drag the tab that is not active (edge-y), so that the group keeps showing a terminal.
      await ui.dragTabTo(page, 'edge-y', area, { inset, fx: 0.5, fy: 0.5 });

      await waitFor('a second group', async () => (await ui.groups(page)).length === 2);
      const all = await ui.groups(page);
      const moved = byId(all, (await ui.panelOf(page, 'edge-y')).groupId);
      const stayed = byId(all, (await ui.panelOf(page, 'edge-x')).groupId);
      assert.notEqual(moved.id, stayed.id, 'the dragged panel got a group of its own');
      assert.deepEqual(moved.sessions, ['edge-y']);
      assert.deepEqual(stayed.sessions, ['edge-x']);

      const [m, s] = [moved.rect, stayed.rect];
      switch (edge) {
        case 'right':
          assert.ok(m.x >= s.x + s.width - SLACK, `edge-y is right of edge-x: ${JSON.stringify({ m, s })}`);
          assert.ok(Math.abs(m.y - s.y) <= SLACK && Math.abs(m.height - s.height) <= SLACK, 'side by side, same height');
          break;
        case 'left':
          assert.ok(m.x + m.width <= s.x + SLACK, `edge-y is left of edge-x: ${JSON.stringify({ m, s })}`);
          assert.ok(Math.abs(m.y - s.y) <= SLACK && Math.abs(m.height - s.height) <= SLACK, 'side by side, same height');
          break;
        case 'bottom':
          assert.ok(m.y >= s.y + s.height - SLACK, `edge-y is below edge-x: ${JSON.stringify({ m, s })}`);
          assert.ok(Math.abs(m.x - s.x) <= SLACK && Math.abs(m.width - s.width) <= SLACK, 'stacked, same width');
          break;
        default:
          assert.ok(m.y + m.height <= s.y + SLACK, `edge-y is above edge-x: ${JSON.stringify({ m, s })}`);
          assert.ok(Math.abs(m.x - s.x) <= SLACK && Math.abs(m.width - s.width) <= SLACK, 'stacked, same width');
      }
      assert.ok(m.width > 200 && m.height > 100 && s.width > 200 && s.height > 100, 'both groups have a usable size');

      // Both terminals are alive: attached, not reconnected by the move, and showing their prompt.
      const panels = await Promise.all([ui.waitForPanel(page, 'edge-x'), ui.waitForPanel(page, 'edge-y')]);
      assert.equal(ui.attachSockets(page).length, sockets, 'moving a panel does not reconnect it');
      for (const panel of panels) await ui.waitForPrompt(page, panel.id);
      assert.deepEqual(page.__errors, []);
    });
  }

  it('dragging a tab inside the tab strip reorders the tabs', TEST, async () => {
    const page = await ctx.env.freshPage();
    for (const name of ['mv-a', 'mv-b', 'mv-c']) await ui.addPanel(page, name);
    assert.deepEqual(await ui.tabTitles(page), ['mv-a', 'mv-b', 'mv-c']);
    const sockets = ui.attachSockets(page).length;

    // mv-c onto the left half of mv-a: it goes first.
    const first = await ui.tabFor(page, 'mv-a').boundingBox();
    await ui.dragMouse(page, ui.centreOf(await ui.tabFor(page, 'mv-c').boundingBox()), { x: first.x + 8, y: first.y + first.height / 2 });
    await waitFor('mv-c to be the first tab', async () => (await ui.tabTitles(page))[0] === 'mv-c');
    assert.deepEqual(await ui.tabTitles(page), ['mv-c', 'mv-a', 'mv-b']);

    // mv-c onto the empty part of the tab strip: it goes last.
    const strip = await ui.groupControl(page, 'mv-a', 'header-actions');
    const actions = await strip.boundingBox();
    const lastTab = await ui.tabFor(page, 'mv-b').boundingBox();
    const emptyX = (lastTab.x + lastTab.width + actions.x) / 2;
    await ui.dragMouse(page, ui.centreOf(await ui.tabFor(page, 'mv-c').boundingBox()), { x: emptyX, y: lastTab.y + lastTab.height / 2 });
    await waitFor('mv-c to be the last tab', async () => (await ui.tabTitles(page)).at(-1) === 'mv-c');
    assert.deepEqual(await ui.tabTitles(page), ['mv-a', 'mv-b', 'mv-c']);
    assert.equal((await ui.groups(page)).length, 1, 'still one group');
    assert.equal(ui.attachSockets(page).length, sockets, 'reordering does not reconnect anything');
    assert.deepEqual(page.__errors, []);
  });
});
