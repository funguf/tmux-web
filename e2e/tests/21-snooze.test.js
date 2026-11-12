/**
 * Snoozing and waking Claude from the session picker and the terminal context menu. Claude is a
 * fake script on an isolated tmux server (see lib/fake-claude.js); the real claude never runs.
 */
import assert from 'node:assert/strict';
import fs from 'node:fs';
import { describe, it } from 'node:test';
import * as ui from '../lib/app.js';
import { FakeClaude } from '../lib/fake-claude.js';
import { TEST, useEnv } from '../lib/suite.js';
import { waitFor } from '../lib/wait.js';

const CONVERSATION = '3f2b8c1e-6d4a-4b7e-9a10-5c2d7e8f9a01';
const MENU_CONVERSATION = '1a2b3c4d-5e6f-4a7b-8c9d-0e1f2a3b4c5d';
const ALL_CONVERSATION = '2b3c4d5e-6f7a-4b8c-9d0e-1f2a3b4c5d6e';
const OTHER_CONVERSATION = '9c1d2e3f-4a5b-4c6d-8e7f-0a1b2c3d4e5f';
const row = (page, session, testid) => page.locator(`[data-testid="${testid}"][data-session="${session}"]`);
const rowOf = (page, session) => page.locator('.picker-row').filter({ has: ui.pickerItem(page, session) });

describe('snooze and wake Claude', () => {
  const ctx = useEnv('snooze');
  let counter = 0;
  /** @type {FakeClaude} */
  let fake;

  function claudeSession(prefix, conversation = CONVERSATION) {
    const { env } = ctx;
    fake ??= new FakeClaude(env);
    counter += 1;
    const name = `${prefix}-${counter}`;
    fake.newSession(name);
    fake.start(name, conversation);
    return name;
  }

  async function openPicker(page) {
    if (!(await page.getByTestId('session-picker').isVisible())) await page.getByTestId('add-panel-btn').click();
    await page.getByTestId('session-picker').waitFor({ state: 'visible' });
  }

  async function shot(page, file) {
    if (!process.env.TMUX_WEB_SHOT_DIR) return;
    fs.mkdirSync(process.env.TMUX_WEB_SHOT_DIR, { recursive: true });
    await page.screenshot({ path: `${process.env.TMUX_WEB_SHOT_DIR}/${file}` });
  }

  it('snoozes from the picker (closing the tab), wakes from the picker (reopening it)', TEST, async () => {
    const { env } = ctx;
    const name = claudeSession('snz');
    const page = await env.freshPage();
    await ui.addPanel(page, name);
    await waitFor('the fake claude to register', async () => fake.pids().length === 1);
    const [pid] = fake.pids();

    await openPicker(page);
    await waitFor('the Claude badge', async () => (await rowOf(page, name).getByTestId('session-picker-claude').count()) === 1);
    assert.equal(await row(page, name, 'session-picker-snooze').count(), 1);
    assert.equal(await row(page, name, 'session-picker-wake').count(), 0, 'nothing is snoozed yet');
    assert.equal(await ui.tabFor(page, name).getByTestId('tab-snoozed').isVisible(), false);
    const title = await row(page, name, 'session-picker-snooze').getAttribute('title');
    assert.equal(title, `Snooze Claude in “${name}” to free memory`);

    await row(page, name, 'session-picker-snooze').click();
    await waitFor('the success toast', async () =>
      (await ui.toasts(page)).some((text) => new RegExp(`^Snoozed “${name}” · freed \\d+ MB$`).test(text)),
    );
    await waitFor('the session panel to close', async () => (await ui.panels(page)).filter((p) => p.session === name).length === 0);
    assert.equal(await page.getByTestId('session-picker').isVisible(), true, 'the picker stays open after a snooze');
    await waitFor('the snoozed badge', async () => (await rowOf(page, name).getByTestId('session-picker-snoozed').count()) === 1);
    assert.equal(await rowOf(page, name).getByTestId('session-picker-snoozed').textContent(), 'snoozed');
    await waitFor('the Claude badge to go', async () => (await rowOf(page, name).getByTestId('session-picker-claude').count()) === 0);
    assert.equal(await row(page, name, 'session-picker-wake').count(), 1);
    assert.equal(await row(page, name, 'session-picker-snooze').count(), 0);
    await waitFor('the fake claude to be gone', async () => !fs.existsSync(`/proc/${pid}`));
    assert.ok(env.tmux.sessions().includes(name), 'the tmux session stays');
    await shot(page, 'snooze-picker-snoozed.png');

    // A second session that still runs Claude, next to the snoozed one: both buttons show.
    const other = claudeSession('snz-live', OTHER_CONVERSATION);
    await waitFor('the second row with a snooze button', async () => (await row(page, other, 'session-picker-snooze').count()) === 1);
    await shot(page, 'snooze-picker.png');

    // Wake from the picker: the session opens, is active, and Claude resumes the same conversation.
    await row(page, name, 'session-picker-wake').click();
    await waitFor('the wake toast', async () => (await ui.toasts(page)).includes(`Woke “${name}”`));
    await page.getByTestId('session-picker').waitFor({ state: 'hidden' });
    await waitFor('the woken session to have an open panel', async () => (await ui.panels(page)).some((p) => p.session === name));
    await ui.tabFor(page, name).waitFor({ state: 'visible' });
    const woken = await ui.panelOf(page, name);
    assert.ok(woken, 'a panel for the woken session');
    await waitFor('the woken panel to be active', async () =>
      (await ui.groups(page)).some((group) => group.activePanelId === woken.id),
    );
    await waitFor('a Claude with the same conversation in the pane', async () => fake.conversations().includes(CONVERSATION));
    await ui.tabFor(page, name).getByTestId('tab-claude').waitFor({ state: 'visible' });
    await ui.tabFor(page, name).getByTestId('tab-snoozed').waitFor({ state: 'hidden' });
    await openPicker(page);
    assert.equal(await rowOf(page, name).getByTestId('session-picker-snoozed').count(), 0);
    assert.equal(await row(page, name, 'session-picker-wake').count(), 0);
    assert.deepEqual(page.__errors, []);
  });

  it('opening a snoozed session by its row does not wake it; the context menu wakes it', TEST, async () => {
    const { env } = ctx;
    const name = claudeSession('snz-menu', MENU_CONVERSATION);
    const page = await env.freshPage();
    await ui.addPanel(page, name);
    await waitFor('the fake claude to register', async () => fake.conversations().includes(MENU_CONVERSATION));
    await openPicker(page);
    await row(page, name, 'session-picker-snooze').click();
    await waitFor('the session panel to close', async () => (await ui.panels(page)).filter((p) => p.session === name).length === 0);
    await waitFor('the snoozed badge', async () => (await rowOf(page, name).getByTestId('session-picker-snoozed').count()) === 1);
    await waitFor('the Claude to stop', async () => !fake.conversations().includes(MENU_CONVERSATION));

    // Clicking the row opens the session without waking it.
    await page.keyboard.press('Escape');
    await page.getByTestId('session-picker').waitFor({ state: 'hidden' });
    const panel = await ui.addPanel(page, name);
    await ui.tabFor(page, name).getByTestId('tab-snoozed').waitFor({ state: 'visible' });
    assert.equal(await ui.tabFor(page, name).getByTestId('tab-snoozed').getAttribute('title'), 'Claude is snoozed');
    assert.equal(await ui.tabFor(page, name).getByTestId('tab-claude').isVisible(), false);
    assert.equal(fake.conversations().includes(MENU_CONVERSATION), false, 'opening did not wake');
    await shot(page, 'snooze-tab.png');

    await ui.terminalPanel(page, panel.id).click({ button: 'right', position: { x: 60, y: 60 } });
    await page.getByTestId('context-menu').waitFor({ state: 'visible' });
    assert.equal(await page.locator('[data-testid="context-menu-item"][data-action="snooze"]').count(), 0, 'no Claude running here');
    await page.locator('[data-testid="context-menu-item"][data-action="wake"]').click();
    await waitFor('the wake toast', async () => (await ui.toasts(page)).includes(`Woke “${name}”`));
    await waitFor('a Claude with the same conversation in the pane', async () => fake.conversations().includes(MENU_CONVERSATION));
    await ui.tabFor(page, name).getByTestId('tab-snoozed').waitFor({ state: 'hidden' });
    await ui.tabFor(page, name).getByTestId('tab-claude').waitFor({ state: 'visible' });
    assert.equal((await ui.panels(page)).filter((p) => p.session === name).length, 1, 'waking focused the open panel, no duplicate');
    await openPicker(page);
    await waitFor('the snoozed badge to go', async () => (await rowOf(page, name).getByTestId('session-picker-snoozed').count()) === 0);
    assert.deepEqual(page.__errors, []);
  });

  it('Open all skips a snoozed session', TEST, async () => {
    const { env } = ctx;
    const name = claudeSession('snz-all', ALL_CONVERSATION);
    const page = await env.freshPage();
    await ui.addPanel(page, name);
    await openPicker(page);
    await row(page, name, 'session-picker-snooze').click();
    await waitFor('the session panel to close', async () => (await ui.panels(page)).filter((p) => p.session === name).length === 0);
    await waitFor('the snoozed badge', async () => (await rowOf(page, name).getByTestId('session-picker-snoozed').count()) === 1);
    // Sessions Open all should open: every row that is not put away (snoozed with no Claude badge).
    const rows = await page.locator('.picker-row').evaluateAll((items) =>
      items.map((item) => ({
        session: item.querySelector('[data-testid="session-picker-item"]').dataset.session,
        putAway: Boolean(item.querySelector('[data-testid="session-picker-snoozed"]')) && !item.querySelector('[data-testid="session-picker-claude"]'),
      })),
    );
    assert.ok(rows.find((r) => r.session === name)?.putAway, 'the snoozed session is put away');
    const open = new Set((await ui.panels(page)).map((p) => p.session));
    const expected = rows.filter((r) => !r.putAway && !open.has(r.session)).length;
    const button = page.getByTestId('open-all-btn');
    const label = expected > 0 ? `Open all (${expected})` : 'Open all';
    await waitFor(`the button to read "${label}"`, async () => ((await button.textContent()) ?? '').trim() === label);
    if (expected > 0) {
      await button.click();
      await waitFor('the other sessions to open', async () => (await ui.panels(page)).length === open.size + expected);
    }
    assert.equal((await ui.panels(page)).filter((p) => p.session === name).length, 0, 'the snoozed session stayed closed');
    assert.deepEqual(page.__errors, []);
  });

  it('a replying Claude is not snoozed: error toast, still running', TEST, async () => {
    const { env } = ctx;
    fake ??= new FakeClaude(env);
    fake.setStatus('busy');
    const before = fake.pids().length;
    const name = claudeSession('snz-busy');
    const page = await env.freshPage();
    await waitFor('the busy fake to register', async () => fake.pids().length === before + 1);
    await openPicker(page);
    await waitFor('the snooze button', async () => (await row(page, name, 'session-picker-snooze').count()) === 1);
    await row(page, name, 'session-picker-snooze').click();
    await waitFor('the error toast', async () => (await ui.toasts(page)).some((text) => /replying/.test(text)));
    assert.equal(fake.pids().length, before + 1, 'still running');
    assert.equal(await rowOf(page, name).getByTestId('session-picker-snoozed').count(), 0);
    fake.setStatus('idle');
    assert.deepEqual(page.__errors, []);
  });
});
