/**
 * Other UI behaviours of CONTRACT §8.6 that the numbered requirements rely on: the session
 * picker's keyboard and filter, the terminal context menu, the settings popover and resizing a
 * group by dragging its sash.
 */
import assert from 'node:assert/strict';
import { before, describe, it } from 'node:test';
import * as ui from '../lib/app.js';
import { TEST, useEnv } from '../lib/suite.js';
import { waitFor } from '../lib/wait.js';

describe('picker, context menu, settings and sashes', () => {
  const ctx = useEnv('extras');
  let counter = 0;

  function makeSession(prefix) {
    const { env } = ctx;
    counter += 1;
    const name = `${prefix}-${counter}`;
    env.tmux.newSession(name, { cwd: env.dir(name) });
    return name;
  }

  before(() => {
    for (const name of ['apple', 'apricot', 'banana', 'cherry']) ctx.env.tmux.newSession(name, { cwd: ctx.env.dir(name) });
  });

  it('the picker filters by name and works from the keyboard', TEST, async () => {
    const page = await ctx.env.freshPage();
    await page.getByTestId('add-panel-btn').click();
    const filter = page.getByTestId('session-picker-filter');
    assert.equal(await page.evaluate(() => document.activeElement?.getAttribute('data-testid')), 'session-picker-filter', 'the filter has the focus');
    const visible = () => page.getByTestId('session-picker-item').evaluateAll((items) => items.map((item) => item.dataset.session));

    assert.deepEqual(await visible(), ['apple', 'apricot', 'banana', 'cherry']);
    await filter.fill('AP');
    assert.deepEqual(await visible(), ['apple', 'apricot'], 'case-insensitive substring filter');
    await filter.fill('zzz');
    assert.deepEqual(await visible(), []);
    assert.equal(await page.getByTestId('session-picker-empty').isVisible(), true);

    // Arrow keys move the highlight, Enter opens it.
    await filter.fill('ap');
    await page.keyboard.press('ArrowDown');
    await page.keyboard.press('Enter');
    const panel = await ui.waitForPanel(page, 'apricot');
    await page.getByTestId('session-picker').waitFor({ state: 'hidden' });
    await ui.waitForPrompt(page, panel.id);
    await ui.waitForTerminalFocus(page, panel.id); // closing the picker hands the keyboard to the terminal

    // Escape closes without opening anything.
    await page.getByTestId('add-panel-btn').click();
    await page.keyboard.press('Escape');
    await page.getByTestId('session-picker').waitFor({ state: 'hidden' });
    assert.equal((await ui.panels(page)).length, 1);
    assert.deepEqual(page.__errors, []);
  });

  it('the context menu renames the session (and refuses bad or taken names)', TEST, async () => {
    const { env } = ctx;
    const name = makeSession('menu-rename');
    const taken = makeSession('menu-taken');
    const page = await env.freshPage();
    const panel = await ui.addPanel(page, name);
    await ui.waitForPrompt(page, panel.id);

    async function openRenameDialog() {
      const box = await ui.terminalPanel(page, panel.id).boundingBox();
      await page.mouse.click(box.x + 200, box.y + 200, { button: 'right' });
      await page.locator('[data-testid="context-menu-item"][data-action="rename"]').click();
      await page.getByTestId('rename-dialog').waitFor({ state: 'visible' });
    }
    await openRenameDialog();
    const input = page.getByTestId('rename-input');
    assert.equal(await input.inputValue(), name, 'the dialog starts with the current name');

    await input.fill(taken);
    await page.getByTestId('rename-confirm-btn').click();
    await waitFor('the "exists" error', async () => /exists|already/i.test((await page.getByTestId('rename-error').textContent()) ?? ''));
    await input.fill('bad.name');
    await page.getByTestId('rename-confirm-btn').click();
    await waitFor('the "." error', async () => /"\."/.test((await page.getByTestId('rename-error').textContent()) ?? ''));
    assert.ok(env.tmux.sessions().includes(name), 'nothing was renamed yet');

    await input.fill('menu renamed ✓');
    await input.press('Enter');
    await page.getByTestId('rename-dialog').waitFor({ state: 'hidden' });
    await waitFor('the tab to show the new name', async () => (await ui.tabFor(page, 'menu renamed ✓').count()) === 1);
    assert.equal(await ui.tabFor(page, 'menu renamed ✓').textContent(), 'menu renamed ✓');
    assert.ok(env.tmux.sessions().includes('menu renamed ✓') && !env.tmux.sessions().includes(name), 'tmux has the new name');
    const renamed = await ui.panelOf(page, 'menu renamed ✓');
    assert.equal(renamed.id, panel.id);
    assert.equal(renamed.connected, true);
    await ui.waitForTerminalFocus(page, panel.id); // closing the dialog hands the keyboard back
    assert.deepEqual(page.__errors, []);
  });

  it('the context menu splits, maximises and closes', TEST, async () => {
    const { env } = ctx;
    const [a, b] = [makeSession('menu-act'), makeSession('menu-act')];
    const page = await env.freshPage();
    const first = await ui.addPanel(page, a);
    await ui.waitForPrompt(page, first.id);
    const menu = async (action) => {
      const box = await ui.terminalPanel(page, first.id).boundingBox();
      await page.mouse.click(box.x + 100, box.y + 100, { button: 'right' });
      await page.locator(`[data-testid="context-menu-item"][data-action="${action}"]`).click();
    };

    await menu('split-right');
    await ui.pickerItem(page, b).click();
    const second = await ui.waitForPanel(page, b);
    assert.equal((await ui.groups(page)).length, 2);
    assert.notEqual(second.groupId, first.groupId);

    await menu('maximize');
    await waitFor('maximised', async () => (await ui.panelOf(page, a)).maximized);
    await menu('maximize'); // the same entry now reads Restore
    await waitFor('restored', async () => !(await ui.panelOf(page, a)).maximized);

    await menu('split-down');
    await page.getByTestId('session-picker').waitFor({ state: 'visible' });
    assert.match(await page.getByTestId('session-picker').textContent(), /below/);
    await page.keyboard.press('Escape');

    await menu('close');
    await waitFor('the panel to be closed', async () => (await ui.panelOf(page, a)) === null);
    assert.ok(env.tmux.sessions().includes(a), 'closing never kills the session');
    assert.deepEqual(page.__errors, []);
  });

  it('settings: font size resizes the terminal and tmux, GPU rendering can be toggled', TEST, async () => {
    const { env } = ctx;
    const name = makeSession('settings');
    const page = await env.freshPage();
    const panel = await ui.addPanel(page, name);
    await ui.waitForPrompt(page, panel.id);
    const sizeOf = () => ui.termSize(page, panel.id);
    const tmuxAgrees = async () => {
      const size = await sizeOf();
      const client = env.tmux.clientSize(name);
      return client?.width === size.cols && client?.height === size.rows;
    };
    await waitFor('tmux to agree with the terminal', tmuxAgrees);
    const before = await sizeOf();

    await page.getByTestId('settings-btn').click();
    for (let i = 0; i < 6; i += 1) await page.getByTestId('font-size-inc').click();
    assert.equal(await page.getByTestId('font-size-value').textContent(), '19');
    await waitFor('a bigger font to give fewer columns and rows', async () => {
      const size = await sizeOf();
      return size.cols < before.cols && size.rows < before.rows;
    });
    await waitFor('tmux to follow the font size change', tmuxAgrees);

    // GPU rendering off and on again: the terminal keeps working.
    await page.getByTestId('webgl-toggle').uncheck();
    await page.getByTestId('webgl-toggle').check();
    await page.keyboard.press('Escape');
    await ui.clickTerminal(page, panel.id);
    await ui.waitForTerminalFocus(page, panel.id);
    await ui.typeLine(page, 'echo still-typing');
    await ui.waitForScreen(page, panel.id, (line) => line === 'still-typing');
    assert.deepEqual(page.__errors, []);
  });

  it('a lost WebGL context falls back to the DOM renderer and the terminal keeps working', TEST, async (t) => {
    const name = makeSession('webgl');
    const page = await ctx.env.freshPage();
    const panel = await ui.addPanel(page, name);
    await ui.waitForPrompt(page, panel.id);
    const canvases = () => page.evaluate((id) => window.__tmuxWeb.terminal(id).element.querySelectorAll('canvas').length, panel.id);
    if ((await canvases()) === 0) {
      t.skip('this browser has no WebGL: nothing to lose');
      return;
    }

    // Take the GPU context away, as a driver reset or too many contexts would.
    const lost = await page.evaluate((id) => {
      for (const canvas of window.__tmuxWeb.terminal(id).element.querySelectorAll('canvas')) {
        const gl = canvas.getContext('webgl2') ?? canvas.getContext('webgl');
        const extension = gl?.getExtension('WEBGL_lose_context');
        if (extension) {
          extension.loseContext();
          return true;
        }
      }
      return false;
    }, panel.id);
    assert.equal(lost, true, 'a WebGL context was found and lost');
    await waitFor('the WebGL canvas to be gone', async () => (await canvases()) === 0);

    // The DOM renderer shows the text from here on.
    await ui.clickTerminal(page, panel.id);
    await ui.waitForTerminalFocus(page, panel.id);
    await ui.typeLine(page, 'echo dom-renderer-ok');
    await ui.waitForScreen(page, panel.id, (line) => line === 'dom-renderer-ok');
    await waitFor(
      'the DOM rows to show the output',
      () => page.evaluate((id) => window.__tmuxWeb.terminal(id).element.querySelector('.xterm-rows')?.textContent.includes('dom-renderer-ok'), panel.id),
    );
    assert.deepEqual(page.__errors, []);
  });

  it('dragging the sash between two groups resizes both terminals and their tmux windows', TEST, async () => {
    const { env } = ctx;
    const [a, b] = [makeSession('sash'), makeSession('sash')];
    const page = await env.freshPage();
    const left = await ui.addPanel(page, a);
    const right = await ui.splitFrom(page, a, 'right', b);
    for (const panel of [left, right]) await ui.waitForPrompt(page, panel.id);
    const sizes = async () => ({ left: await ui.termSize(page, left.id), right: await ui.termSize(page, right.id) });
    const before = await sizes();
    const sash = page.locator('.dv-sash').first();
    const box = await sash.boundingBox();
    assert.ok(box, 'there is a sash between the groups');

    await ui.dragMouse(page, ui.centreOf(box), { x: box.x + box.width / 2 + 200, y: box.y + box.height / 2 }, { settle: 100 });
    await waitFor('the left terminal to grow and the right one to shrink', async () => {
      const now = await sizes();
      return now.left.cols > before.left.cols + 15 && now.right.cols < before.right.cols - 15;
    });
    await waitFor('tmux to follow both sizes', async () => {
      const now = await sizes();
      return env.tmux.clientSize(a)?.width === now.left.cols && env.tmux.clientSize(b)?.width === now.right.cols;
    });
    assert.deepEqual(page.__errors, []);
  });
});
