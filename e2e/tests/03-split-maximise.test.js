/**
 * Requirement 3: split right and split down from the header buttons.
 * Requirement 4: maximise fills the work area; restore returns the previous layout
 * (the same groups and sizes).
 */
import assert from 'node:assert/strict';
import { before, describe, it } from 'node:test';
import * as ui from '../lib/app.js';
import { TEST, useEnv } from '../lib/suite.js';
import { waitFor } from '../lib/wait.js';

const SLACK = 2;
const near = (actual, expected, what) => assert.ok(Math.abs(actual - expected) <= SLACK, `${what}: ${actual} is not within ${SLACK} of ${expected}`);
const byId = (all, id) => all.find((group) => group.id === id);

describe('splitting and maximising groups', () => {
  const ctx = useEnv('split');

  before(() => {
    const { env } = ctx;
    for (const name of ['sp-a', 'sp-b', 'sp-c', 'mx-a', 'mx-b', 'mx-c']) env.tmux.newSession(name, { cwd: env.dir(name) });
  });

  it('split right puts the chosen session in a new group to the right', TEST, async () => {
    const page = await ctx.env.freshPage();
    const a = await ui.addPanel(page, 'sp-a');
    await ui.waitForPrompt(page, a.id);
    assert.equal((await ui.groups(page)).length, 1);

    await (await ui.groupControl(page, 'sp-a', 'header-split-right')).click();
    const picker = page.getByTestId('session-picker');
    await picker.waitFor({ state: 'visible' });
    assert.equal(await picker.getAttribute('data-mode'), 'split', 'the picker opens in split mode');
    assert.match(await picker.textContent(), /to the right/);
    assert.equal(await page.getByTestId('open-all-btn').isVisible(), false, '"Open all" is not offered when splitting');

    // Escape cancels: nothing changes.
    await page.keyboard.press('Escape');
    await picker.waitFor({ state: 'hidden' });
    assert.equal((await ui.groups(page)).length, 1);
    assert.equal((await ui.panels(page)).length, 1);

    const b = await ui.splitFrom(page, 'sp-a', 'right', 'sp-b');
    await picker.waitFor({ state: 'hidden' });
    const all = await ui.groups(page);
    assert.equal(all.length, 2);
    const left = byId(all, a.groupId);
    const right = byId(all, b.groupId);
    assert.notEqual(left.id, right.id);
    assert.deepEqual(right.sessions, ['sp-b'], 'the chosen session is alone in the new group');
    assert.deepEqual(left.sessions, ['sp-a']);
    assert.ok(right.rect.x >= left.rect.x + left.rect.width - SLACK, `new group is right of the old one: ${JSON.stringify({ left: left.rect, right: right.rect })}`);
    near(right.rect.y, left.rect.y, 'same top');
    near(right.rect.height, left.rect.height, 'same height');
    assert.ok(left.rect.width > 300 && right.rect.width > 300, 'both groups are usable');
    await ui.waitForPrompt(page, b.id);
    await ui.waitForPrompt(page, a.id);
    assert.deepEqual(page.__errors, []);
  });

  it('split down puts the chosen session in a new group below', TEST, async () => {
    const page = await ctx.env.freshPage();
    const a = await ui.addPanel(page, 'sp-a');
    const b = await ui.splitFrom(page, 'sp-a', 'right', 'sp-b');

    await (await ui.groupControl(page, 'sp-a', 'header-split-down')).click();
    const picker = page.getByTestId('session-picker');
    await picker.waitFor({ state: 'visible' });
    assert.equal(await picker.getAttribute('data-mode'), 'split');
    assert.match(await picker.textContent(), /below/);
    await ui.pickerItem(page, 'sp-c').click();
    const c = await ui.waitForPanel(page, 'sp-c');

    const all = await ui.groups(page);
    assert.equal(all.length, 3);
    const ga = byId(all, a.groupId);
    const gb = byId(all, b.groupId);
    const gc = byId(all, c.groupId);
    assert.deepEqual(gc.sessions, ['sp-c']);
    assert.ok(gc.rect.y >= ga.rect.y + ga.rect.height - SLACK, `sp-c is below sp-a: ${JSON.stringify({ ga: ga.rect, gc: gc.rect })}`);
    near(gc.rect.x, ga.rect.x, 'same left');
    near(gc.rect.width, ga.rect.width, 'same width');
    // The group to the right is untouched: it still spans the full height.
    near(gb.rect.height, ga.rect.height + gc.rect.height, 'the right group keeps the full height');
    assert.ok(gb.rect.x >= ga.rect.x + ga.rect.width - SLACK, 'sp-b stays on the right');
    for (const panel of [a, b, c]) await ui.waitForPrompt(page, panel.id);
    assert.deepEqual(page.__errors, []);
  });

  it('splitting with a session that is already open moves its panel', TEST, async () => {
    const page = await ctx.env.freshPage();
    const a = await ui.addPanel(page, 'sp-a');
    const b = await ui.splitFrom(page, 'sp-a', 'right', 'sp-b');
    const sockets = ui.attachSockets(page).length;

    // sp-b is open already: choosing it below sp-a moves that panel instead of adding a second one.
    await (await ui.groupControl(page, 'sp-a', 'header-split-down')).click();
    await ui.pickerItem(page, 'sp-b').click();
    await waitFor('sp-b to sit below sp-a', async () => {
      const [ga, gb] = [await ui.groupOf(page, 'sp-a'), await ui.groupOf(page, 'sp-b')];
      return Boolean(ga && gb) && ga.id !== gb.id && gb.rect.y >= ga.rect.y + ga.rect.height - SLACK;
    });
    const all = await ui.groups(page);
    assert.equal(all.length, 2, 'the group sp-b left is gone');
    assert.equal((await ui.panels(page)).length, 2, 'no second panel for sp-b');
    const moved = await ui.panelOf(page, 'sp-b');
    assert.equal(moved.id, b.id, 'the same panel');
    assert.notEqual(moved.groupId, a.groupId);
    assert.equal(ui.attachSockets(page).length, sockets, 'the move does not reconnect');
    assert.deepEqual(page.__errors, []);
  });

  it('maximise fills the work area and restore returns the same groups and sizes', TEST, async () => {
    const { env } = ctx;
    const page = await env.freshPage();
    const a = await ui.addPanel(page, 'mx-a');
    const b = await ui.splitFrom(page, 'mx-a', 'right', 'mx-b');
    const c = await ui.splitFrom(page, 'mx-b', 'down', 'mx-c');
    for (const panel of [a, b, c]) await ui.waitForPrompt(page, panel.id);

    // Before: three groups; remember their geometry and what tmux thinks the clients' sizes are.
    const names = ['mx-a', 'mx-b', 'mx-c'];
    await waitFor('tmux to know the size of every terminal', async () => {
      for (const [index, panel] of [a, b, c].entries()) {
        const size = await ui.termSize(page, panel.id);
        const client = env.tmux.clientSize(names[index]);
        if (!size || !client || client.width !== size.cols || client.height !== size.rows) return false;
      }
      return true;
    });
    const before = await ui.groups(page);
    assert.equal(before.length, 3);
    assert.ok(before.every((group) => !group.maximized));
    const sizesBefore = Object.fromEntries(await Promise.all([a, b, c].map(async (panel) => [panel.id, await ui.termSize(page, panel.id)])));
    const area = await ui.workArea(page);

    // Maximise mx-c's group with its header button.
    const button = await ui.groupControl(page, 'mx-c', 'header-maximize');
    assert.equal(await button.getAttribute('data-state'), 'normal');
    await button.click();
    await waitFor('mx-c to be maximised', async () => (await ui.panelOf(page, 'mx-c'))?.maximized === true);
    await waitFor('the maximised group to fill the work area', async () => {
      const group = byId(await ui.groups(page), c.groupId);
      return Math.abs(group.rect.width - area.width) <= SLACK && Math.abs(group.rect.height - area.height) <= SLACK;
    });
    const maximised = byId(await ui.groups(page), c.groupId);
    near(maximised.rect.x, area.x, 'maximised left');
    near(maximised.rect.y, area.y, 'maximised top');
    assert.equal(await button.getAttribute('data-state'), 'maximized');
    assert.match(await button.getAttribute('title'), /restore/i, 'the button offers Restore now');
    assert.deepEqual((await ui.panels(page)).filter((p) => p.maximized).map((p) => p.session), ['mx-c'], 'only mx-c is maximised');
    assert.equal(await ui.terminalPanel(page, a.id).isVisible(), false, 'the other groups are out of sight');
    assert.equal(await ui.terminalPanel(page, b.id).isVisible(), false);
    assert.equal(await ui.terminalPanel(page, c.id).isVisible(), true);
    // The terminal grew with its group, and tmux was told.
    await waitFor('the maximised terminal to resize', async () => {
      const size = await ui.termSize(page, c.id);
      return size.cols > sizesBefore[c.id].cols && size.rows > sizesBefore[c.id].rows && env.tmux.clientSize('mx-c')?.width === size.cols;
    });

    // Restore with the same button.
    await button.click();
    await waitFor('the layout to be restored', async () => (await ui.panels(page)).every((panel) => !panel.maximized));
    await waitFor('the group sizes to come back', async () => {
      const after = await ui.groups(page);
      return after.every((group) => {
        const was = byId(before, group.id);
        return was && Math.abs(group.rect.width - was.rect.width) <= SLACK && Math.abs(group.rect.height - was.rect.height) <= SLACK;
      });
    });
    const after = await ui.groups(page);
    assert.deepEqual(
      after.map(({ id, sessions, panelIds }) => ({ id, sessions, panelIds })),
      before.map(({ id, sessions, panelIds }) => ({ id, sessions, panelIds })),
      'the same groups with the same panels',
    );
    for (const group of after) {
      const was = byId(before, group.id);
      near(group.rect.x, was.rect.x, `group ${group.id} left`);
      near(group.rect.y, was.rect.y, `group ${group.id} top`);
      near(group.rect.width, was.rect.width, `group ${group.id} width`);
      near(group.rect.height, was.rect.height, `group ${group.id} height`);
    }
    assert.equal(await button.getAttribute('data-state'), 'normal');
    await waitFor('the terminals to return to their old size', async () => {
      const sizes = await Promise.all([a, b, c].map((panel) => ui.termSize(page, panel.id)));
      return [a, b, c].every((panel, index) => sizes[index].cols === sizesBefore[panel.id].cols && sizes[index].rows === sizesBefore[panel.id].rows);
    });
    assert.deepEqual(page.__errors, []);
  });

  it('closing the maximised group, or splitting from it, leaves a sane layout', TEST, async () => {
    const { env } = ctx;
    const page = await env.freshPage();
    const a = await ui.addPanel(page, 'mx-a');
    const b = await ui.splitFrom(page, 'mx-a', 'right', 'mx-b');
    const area = await ui.workArea(page);

    // Close the only tab of the maximised group: the other group comes back at full size.
    await (await ui.groupControl(page, 'mx-b', 'header-maximize')).click();
    await waitFor('mx-b to be maximised', async () => (await ui.panelOf(page, 'mx-b'))?.maximized === true);
    await ui.tabFor(page, 'mx-b').getByTestId('tab-close').click();
    await waitFor('mx-b to be gone', async () => (await ui.panelOf(page, 'mx-b')) === null);
    await waitFor('mx-a to fill the work area', async () => {
      const [group] = await ui.groups(page);
      return Math.abs(group.rect.width - area.width) <= SLACK && Math.abs(group.rect.height - area.height) <= SLACK && !group.maximized;
    });
    await waitFor('mx-a to be shown again', () => ui.terminalPanel(page, a.id).isVisible());
    await waitFor('the terminal to grow to the work area and tmux to follow', async () => {
      const size = await ui.termSize(page, a.id);
      const client = env.tmux.clientSize('mx-a');
      return size.cols > 150 && client?.width === size.cols && client?.height === size.rows;
    });

    // Split from a maximised group: maximise ends, the new group sits next to it.
    await (await ui.groupControl(page, 'mx-a', 'header-maximize')).click();
    await waitFor('mx-a to be maximised', async () => (await ui.panelOf(page, 'mx-a'))?.maximized === true);
    await ui.splitFrom(page, 'mx-a', 'right', 'mx-c');
    const all = await ui.groups(page);
    assert.equal(all.length, 2);
    assert.ok(all.every((group) => !group.maximized), 'nothing is maximised any more');
    const left = await ui.groupOf(page, 'mx-a');
    const right = await ui.groupOf(page, 'mx-c');
    assert.ok(right.rect.x >= left.rect.x + left.rect.width - SLACK, 'mx-c is right of mx-a');
    assert.ok(left.rect.width < area.width - 100, 'mx-a is no longer full width');
    assert.deepEqual(page.__errors, []);
  });

  it('a tab shown inside a maximised group takes the full size', TEST, async () => {
    const { env } = ctx;
    const page = await env.freshPage();
    const a = await ui.addPanel(page, 'mx-a');
    const b = await ui.addPanel(page, 'mx-b'); // a second tab of the same group
    const c = await ui.splitFrom(page, 'mx-b', 'right', 'mx-c');
    await ui.tabFor(page, 'mx-a').click(); // mx-a shown again, at half width
    await ui.waitForTerminalFocus(page, a.id);
    const half = await ui.termSize(page, a.id);

    await (await ui.groupControl(page, 'mx-a', 'header-maximize')).click();
    await waitFor('the maximised group', async () => (await ui.panelOf(page, 'mx-a'))?.maximized === true);
    await ui.tabFor(page, 'mx-b').click(); // the tab that was hidden at half width, now in a maximised group
    await ui.waitForTerminalFocus(page, b.id);
    await waitFor('mx-b to fill the maximised group and tmux to follow', async () => {
      const size = await ui.termSize(page, b.id);
      const client = env.tmux.clientSize('mx-b');
      return size.cols > half.cols + 40 && client?.width === size.cols && client?.height === size.rows;
    });
    assert.ok(c.id);
    assert.deepEqual(page.__errors, []);
  });

  it('maximise also works from the tab (double click) and the keyboard (Ctrl+Shift+Enter)', TEST, async () => {
    const page = await ctx.env.freshPage();
    const a = await ui.addPanel(page, 'mx-a');
    const b = await ui.splitFrom(page, 'mx-a', 'right', 'mx-b');
    const area = await ui.workArea(page);
    const before = await ui.groups(page);

    // Double click on the tab toggles maximise of its group.
    await ui.tabFor(page, 'mx-a').dblclick();
    await waitFor('mx-a to be maximised by double click', async () => (await ui.panelOf(page, 'mx-a'))?.maximized === true);
    const maximised = byId(await ui.groups(page), a.groupId);
    near(maximised.rect.width, area.width, 'maximised width');
    near(maximised.rect.height, area.height, 'maximised height');
    await ui.tabFor(page, 'mx-a').dblclick();
    await waitFor('the layout to be restored by double click', async () => (await ui.panels(page)).every((panel) => !panel.maximized));

    // Ctrl+Shift+Enter in the focused terminal toggles it too.
    await ui.clickTerminal(page, b.id);
    await ui.waitForTerminalFocus(page, b.id);
    await page.keyboard.press('Control+Shift+Enter');
    await waitFor('mx-b to be maximised by the keyboard', async () => (await ui.panelOf(page, 'mx-b'))?.maximized === true);
    near(byId(await ui.groups(page), b.groupId).rect.width, area.width, 'keyboard-maximised width');
    await ui.waitForTerminalFocus(page, b.id);
    await page.keyboard.press('Control+Shift+Enter');
    await waitFor('the layout to be restored by the keyboard', async () => (await ui.panels(page)).every((panel) => !panel.maximized));
    const after = await ui.groups(page);
    for (const group of after) near(group.rect.width, byId(before, group.id).rect.width, `group ${group.id} width`);
    // No Ctrl+Shift+Enter ever reached the shell as a stray key.
    assert.ok(!(await ui.screenLines(page, b.id)).some((line) => /\^|;/.test(line)), 'no escape sequence echoed');
    assert.deepEqual(page.__errors, []);
  });
});
