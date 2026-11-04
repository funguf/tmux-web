/**
 * A bell (BEL) rung by a program in a panel that the user is not looking at makes the
 * panel's tab flash (data-attention="true") until the panel is focused again.
 */
import assert from 'node:assert/strict';
import fs from 'node:fs';
import { describe, it } from 'node:test';
import * as ui from '../lib/app.js';
import { TEST, useEnv } from '../lib/suite.js';
import { staysFalse, waitFor } from '../lib/wait.js';

describe('bell attention', () => {
  const ctx = useEnv('bell-attention');
  let sessions = 0;

  /** Two sessions as tabs of one group; the second one is the active tab. */
  async function openTwoTabs() {
    const { env } = ctx;
    sessions += 1;
    const names = [`ba-${sessions}-a`, `ba-${sessions}-b`];
    for (const name of names) env.tmux.newSession(name, { cwd: env.dir(name) });
    const page = await env.freshPage();
    for (const name of names) await ui.addPanel(page, name);
    const second = await ui.panelOf(page, names[1]);
    await ui.waitForPrompt(page, second.id);
    return { page, first: names[0], second: names[1] };
  }

  /** Ring the bell the way a program does: BEL straight to the pane's tty. */
  function ring(session) {
    const tty = ctx.env.tmux.format(session, '#{pane_tty}');
    fs.writeFileSync(tty, '\x07');
  }

  const attention = (page, session) => ui.tabFor(page, session).getAttribute('data-attention');
  const isTrue = async (page, session) => (await attention(page, session)) === 'true';
  const panelAttention = async (page, session) => (await ui.panelOf(page, session)).attention;

  it('flashes the tab of an inactive panel until the tab is clicked', TEST, async () => {
    const { page, first, second } = await openTwoTabs();
    assert.equal(await attention(page, first), 'false');
    ring(first);
    await waitFor('the inactive tab to flash', () => isTrue(page, first));
    assert.equal(await panelAttention(page, first), true);
    assert.equal(await attention(page, second), 'false');
    assert.equal(await panelAttention(page, second), false);

    await ui.tabFor(page, first).click();
    await waitFor('the flash to stop after the tab was clicked', async () => (await attention(page, first)) === 'false');
    assert.equal(await panelAttention(page, first), false);
    assert.deepEqual(page.__errors, []);
  });

  it('does not flash the active, focused panel', TEST, async () => {
    const { page, second } = await openTwoTabs();
    await ui.clickTerminal(page, (await ui.panelOf(page, second)).id);
    ring(second);
    await staysFalse('the focused panel flashed', () => isTrue(page, second));
    assert.equal(await panelAttention(page, second), false);
    assert.deepEqual(page.__errors, []);
  });

  it('flashes the active panel while the page has no focus, and stops when it regains it', TEST, async () => {
    const { page, second } = await openTwoTabs();
    await page.evaluate(() => {
      document.hasFocus = () => false;
    });
    ring(second);
    await waitFor('the tab to flash while the page is unfocused', () => isTrue(page, second));
    assert.equal(await panelAttention(page, second), true);

    await page.evaluate(() => {
      delete document.hasFocus;
      window.dispatchEvent(new Event('focus'));
    });
    await waitFor('the flash to stop once the page is focused', async () => (await attention(page, second)) === 'false');
    assert.equal(await panelAttention(page, second), false);
    assert.deepEqual(page.__errors, []);
  });

  it('negative control: no bell, no flash', TEST, async () => {
    const { page, first } = await openTwoTabs();
    await staysFalse('an inactive tab flashed without a bell', () => isTrue(page, first));
    assert.equal(await panelAttention(page, first), false);
    assert.deepEqual(page.__errors, []);
  });
});
