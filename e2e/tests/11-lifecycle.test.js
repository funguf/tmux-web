/**
 * Panel lifecycle beyond the numbered requirements: creating sessions from the picker,
 * closing panels (the session lives on), the overlays when a session disappears or the client
 * detaches, reconnecting when tmux-web itself restarts, and the toast when another window
 * changes the layout.
 */
import assert from 'node:assert/strict';
import { describe, it } from 'node:test';
import * as ui from '../lib/app.js';
import { newContext } from '../lib/browser.js';
import { TEST, useEnv } from '../lib/suite.js';
import { sleep, staysFalse, waitFor } from '../lib/wait.js';

describe('panel lifecycle', () => {
  const ctx = useEnv('lifecycle');
  let counter = 0;

  /** A tmux session with a name of its own. */
  function makeSession(prefix) {
    const { env } = ctx;
    counter += 1;
    const name = `${prefix}-${counter}`;
    env.tmux.newSession(name, { cwd: env.dir(name) });
    return name;
  }

  it('creates a session from the picker and opens it; bad names are refused', TEST, async () => {
    const { env } = ctx;
    makeSession('keep'); // so that a tmux server runs
    const page = await env.freshPage();
    const input = page.getByTestId('new-session-input');
    const error = page.getByTestId('picker-error');

    await page.getByTestId('add-panel-btn').click();
    await input.fill('bad:name');
    await input.press('Enter');
    await waitFor('the name to be refused', () => error.isVisible());
    assert.match(await error.textContent(), /":"/);
    assert.deepEqual(await ui.panels(page), [], 'no panel for a refused name');
    assert.ok(!env.tmux.sessions().includes('bad:name'));
    await input.fill('');
    await input.press('Enter');
    assert.match(await error.textContent(), /name/i, 'an empty name is refused too');

    await input.fill('lc created');
    await input.press('Enter');
    const created = await ui.waitForPanel(page, 'lc created');
    await page.getByTestId('session-picker').waitFor({ state: 'hidden' });
    assert.ok(env.tmux.sessions().includes('lc created'), 'the session exists in tmux');
    assert.equal(await ui.tabFor(page, 'lc created').textContent(), 'lc created');
    await ui.waitForPrompt(page, created.id);

    // The same name again does not create a second session: the existing panel is focused.
    await page.getByTestId('add-panel-btn').click();
    await input.fill('lc created');
    await input.press('Enter');
    await waitFor('the notice', async () => (await ui.toasts(page)).some((text) => /already exists/.test(text)));
    assert.equal((await ui.panels(page)).length, 1);

    // In split mode the new session goes into a new group.
    await (await ui.groupControl(page, 'lc created', 'header-split-right')).click();
    await input.fill('lc split');
    await input.press('Enter');
    const split = await ui.waitForPanel(page, 'lc split');
    assert.notEqual(split.groupId, created.groupId);
    assert.equal((await ui.groups(page)).length, 2);
    assert.deepEqual(page.__errors, []);
  });

  it('closing a panel keeps the tmux session running and detaches its client', TEST, async () => {
    const { env } = ctx;
    const [first, second] = [makeSession('close'), makeSession('close')];
    const page = await env.freshPage();
    const a = await ui.addPanel(page, first);
    await ui.addPanel(page, second);
    await waitFor('both clients to be attached', () => env.tmux.clientCount(first) === 1 && env.tmux.clientCount(second) === 1);

    // Leave something in the first session that only survives if the session does.
    await ui.tabFor(page, first).click();
    await ui.waitForTerminalFocus(page, a.id);
    await ui.typeLine(page, 'echo kept-state-1234');
    await ui.waitForScreen(page, a.id, (line) => line === 'kept-state-1234');

    // The × button.
    await ui.tabFor(page, first).hover();
    await ui.tabFor(page, first).getByTestId('tab-close').click();
    await waitFor('the first panel to go', async () => (await ui.panelOf(page, first)) === null);
    assert.equal(await ui.tabFor(page, first).count(), 0);
    assert.ok(env.tmux.sessions().includes(first), 'closing a panel does not kill the session');
    await waitFor('the client to detach', () => env.tmux.clientCount(first) === 0, { timeout: 8000 });

    // A middle click on the tab.
    await ui.tabFor(page, second).click({ button: 'middle' });
    await waitFor('the second panel to go', async () => (await ui.panelOf(page, second)) === null);
    assert.ok(env.tmux.sessions().includes(second));
    await waitFor('the second client to detach', () => env.tmux.clientCount(second) === 0, { timeout: 8000 });

    // No panel left: the empty state offers the sessions, and the first one is as it was left.
    await page.getByTestId('empty-state').waitFor({ state: 'visible' });
    await page.locator(`[data-testid="empty-session-item"][data-session="${first}"]`).click();
    const reopened = await ui.waitForPanel(page, first);
    await ui.waitForScreen(page, reopened.id, (line) => line === 'kept-state-1234');
    assert.deepEqual(page.__errors, []);
  });

  it('shows an overlay when the session disappears: Recreate, auto-reconnect and Close', TEST, async () => {
    const { env } = ctx;
    makeSession('keep'); // the server must outlive the victim
    const victim = makeSession('gone');
    const page = await env.freshPage();
    const panel = await ui.addPanel(page, victim);
    await ui.waitForPrompt(page, panel.id);
    const overlay = page.getByTestId('panel-overlay');
    const dot = ui.tabFor(page, victim).getByTestId('tab-dot');

    // The session is killed in tmux.
    env.tmux.run('kill-session', '-t', `=${victim}`);
    await waitFor('the overlay', () => overlay.isVisible());
    assert.match(await page.getByTestId('overlay-message').textContent(), new RegExp(`Session “${victim}” no longer exists`));
    assert.equal(await dot.getAttribute('data-state'), 'closed');
    assert.equal((await ui.panelOf(page, victim)).connected, false);

    // Recreate session: it exists again and the terminal is back.
    await page.getByTestId('overlay-recreate').click();
    await waitFor('the session to exist', () => env.tmux.sessions().includes(victim));
    await waitFor('the overlay to go', async () => !(await overlay.isVisible()));
    await ui.waitForPrompt(page, panel.id);
    assert.equal((await ui.panelOf(page, victim)).connected, true);

    // Killed again, then created by someone else: the panel notices and reconnects by itself.
    env.tmux.run('kill-session', '-t', `=${victim}`);
    await waitFor('the overlay again', () => overlay.isVisible());
    env.tmux.newSession(victim, { cwd: env.dir('elsewhere') });
    await waitFor('the panel to reconnect by itself', async () => (await ui.panelOf(page, victim)).connected);
    await ui.waitForPrompt(page, panel.id);
    assert.equal(await overlay.isVisible(), false);

    // And "Close panel" removes the panel (the session is not touched).
    env.tmux.run('kill-session', '-t', `=${victim}`);
    await waitFor('the overlay a third time', () => overlay.isVisible());
    await page.getByTestId('overlay-close').click();
    await waitFor('the panel to be removed', async () => (await ui.panelOf(page, victim)) === null);
    assert.deepEqual(page.__errors, []);
  });

  it('shows Reattach after the client was detached, and reattaching works', TEST, async () => {
    const { env } = ctx;
    const name = makeSession('detach');
    const page = await env.freshPage();
    const panel = await ui.addPanel(page, name);
    await ui.waitForPrompt(page, panel.id);
    await waitFor('the client to be attached', () => env.tmux.clientCount(name) === 1);

    env.tmux.run('detach-client', '-s', `=${name}`);
    const overlay = page.getByTestId('panel-overlay');
    await waitFor('the overlay', () => overlay.isVisible());
    assert.match(await page.getByTestId('overlay-message').textContent(), /Detached from session/);
    assert.equal((await ui.panelOf(page, name)).connected, false);
    assert.ok(env.tmux.sessions().includes(name), 'the session is untouched');

    await page.getByTestId('overlay-reattach').click();
    await waitFor('the terminal to be connected again', async () => (await ui.panelOf(page, name)).connected);
    await waitFor('the overlay to go', async () => !(await overlay.isVisible()));
    await ui.clickTerminal(page, panel.id);
    await ui.waitForTerminalFocus(page, panel.id);
    await ui.typeLine(page, 'echo reattached-ok');
    await ui.waitForScreen(page, panel.id, (line) => line === 'reattached-ok');
    assert.equal(env.tmux.clientCount(name), 1, 'exactly one client again');
    assert.deepEqual(page.__errors, []);
  });

  it('stays connected through a 30 s server ping and an idle minute', { timeout: 120_000 }, async () => {
    const { env } = ctx;
    const name = makeSession('idle');
    const page = await env.freshPage();
    const panel = await ui.addPanel(page, name);
    await ui.waitForPrompt(page, panel.id);
    await waitFor('the events socket', () => page.__sockets.some((socket) => socket.url.includes('/ws/events')));
    const opened = page.__sockets.length;

    // The server pings every 30 s on both kinds of socket; wait through one full period.
    await sleep(36_000);
    assert.equal((await ui.panelOf(page, name)).connected, true);
    assert.equal(page.__sockets.length, opened, 'no socket was reopened');
    assert.ok(page.__sockets.every((socket) => !socket.closed), 'no socket was closed');

    // Both streams still deliver: a new session shows up, and typing reaches the shell.
    const count = env.tmux.sessions().length;
    makeSession('idle-extra');
    await waitFor('the status stream to report the new session', async () => new RegExp(` · ${count + 1} sessions$`).test(await page.getByTestId('status-pill').textContent()));
    await ui.clickTerminal(page, panel.id);
    await ui.waitForTerminalFocus(page, panel.id);
    await ui.typeLine(page, 'echo alive-after-idle');
    await ui.waitForScreen(page, panel.id, (line) => line === 'alive-after-idle');
    assert.deepEqual(page.__errors, []);
  });

  it('reconnects the terminals and the status stream when tmux-web restarts', { timeout: 120_000 }, async () => {
    const { env } = ctx;
    const name = makeSession('restart');
    const page = await env.freshPage();
    const panel = await ui.addPanel(page, name);
    await ui.waitForPrompt(page, panel.id);
    await ui.clickTerminal(page, panel.id);
    await ui.waitForTerminalFocus(page, panel.id);
    await ui.typeLine(page, 'echo before-restart');
    await ui.waitForScreen(page, panel.id, (line) => line === 'before-restart');

    const listen = env.server.address;
    await env.server.stop();
    const pill = page.getByTestId('status-pill');
    await waitFor('the pill to say tmux-web is unreachable', async () => (await pill.getAttribute('data-state')) === 'offline', { timeout: 15_000 });
    await waitFor('the panel to notice', async () => !(await ui.panelOf(page, name)).connected);
    assert.equal(await page.getByTestId('panel-overlay').getAttribute('data-kind'), 'waiting');
    assert.match(await page.getByTestId('overlay-hint').textContent(), /Retrying in \d+s/);
    assert.ok(env.tmux.sessions().includes(name), 'tmux itself is unaffected');

    await env.server.start({ listen });
    // Backoff is 1, 2, 4, 8, 10 s: everything is back well within a minute, by itself.
    await waitFor('the status stream to reconnect', async () => (await pill.getAttribute('data-state')) === 'ok', { timeout: 40_000 });
    await waitFor('the terminal to reconnect', async () => (await ui.panelOf(page, name)).connected, { timeout: 40_000 });
    await waitFor('the overlay to go', async () => !(await page.getByTestId('panel-overlay').isVisible()));
    await ui.waitForScreen(page, panel.id, (line) => line === 'before-restart');
    await ui.clickTerminal(page, panel.id);
    await ui.waitForTerminalFocus(page, panel.id);
    await ui.typeLine(page, 'echo after-restart');
    await ui.waitForScreen(page, panel.id, (line) => line === 'after-restart');
    assert.equal(env.tmux.clientCount(name), 1, 'one attach client, no leftovers of the old connection');
    assert.deepEqual(page.__errors, []);
  });

  it('a layout change in another window offers a Load button and is never applied by itself', TEST, async () => {
    const { env } = ctx;
    const [one, two] = [makeSession('sync'), makeSession('sync')];
    const pageA = await env.freshPage();
    await ui.addPanel(pageA, one);
    await waitFor('the first layout to be saved', async () => Object.keys((await env.savedLayout()).layout?.dockview?.panels ?? {}).length === 1, { timeout: 10_000 });

    const { context, page: pageB } = await newContext(env.browser);
    try {
      await env.openApp(pageB);
      await waitFor('window B to show the saved panel', async () => (await ui.panels(pageB)).length === 1);

      // Window A adds a panel: window B is told, but keeps its own layout until asked.
      await ui.addPanel(pageA, two);
      const load = pageB.getByTestId('toast-load-layout');
      await waitFor('the Load toast in window B', () => load.isVisible(), { timeout: 10_000 });
      assert.equal(await pageA.getByTestId('toast-load-layout').count(), 0, 'the window that saved is not told');
      await staysFalse('window B applied the other window\'s layout by itself', async () => (await ui.panels(pageB)).length !== 1, { duration: 800 });

      const version = (await env.savedLayout()).version;
      await load.click();
      await waitFor('window B to take the new layout', async () => (await ui.panels(pageB)).length === 2);
      await staysFalse('loading a layout saved it back (a save loop)', async () => (await env.savedLayout()).version !== version, { duration: 1500 });
      assert.deepEqual(pageB.__errors, []);
    } finally {
      await context.close();
    }
    assert.deepEqual(pageA.__errors, []);
  });
});
