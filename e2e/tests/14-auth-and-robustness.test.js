/**
 * Robustness: the token sign-in flow in a real browser, a large paste arriving intact, and an
 * app that loads and runs without failed requests or console errors.
 */
import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import { describe, it } from 'node:test';
import * as ui from '../lib/app.js';
import { newContext } from '../lib/browser.js';
import { TEST, useEnv } from '../lib/suite.js';
import { waitFor } from '../lib/wait.js';

const TOKEN = 'e2e-secret-token-0123456789abcdef';

describe('token authentication in the browser', () => {
  const ctx = useEnv('auth', { serveArgs: ['--token', TOKEN] });

  it('refuses a visit without the token, signs in with ?token=, and keeps the session in a cookie', TEST, async () => {
    const { env } = ctx;
    env.tmux.newSession('auth-a', { cwd: env.dir('auth-a') });
    const base = env.url;

    // Without the token: JSON 401 for the API, a sign-in form for the page.
    const api = await fetch(`${base}/api/status`);
    assert.equal(api.status, 401);
    assert.equal((await api.json()).error, 'unauthorized');
    const wrong = await fetch(`${base}/api/status`, { headers: { Authorization: 'Bearer not-the-token' } });
    assert.equal(wrong.status, 401);
    const ok = await fetch(`${base}/api/status`, { headers: { Authorization: `Bearer ${TOKEN}` } });
    assert.equal(ok.status, 200);

    const { context, page } = await newContext(env.browser);
    try {
      const refused = await page.goto(base);
      assert.equal(refused.status(), 401);
      assert.equal(await page.getByTestId('top-bar').count(), 0, 'the app is not served');
      assert.equal(await page.evaluate(() => window.__tmuxWeb), undefined);

      // With the token: a redirect to the same URL without it, and a cookie.
      await page.goto(`${base}/?token=${TOKEN}`);
      await page.waitForFunction(() => window.__tmuxWeb !== undefined);
      await page.evaluate(() => window.__tmuxWeb.ready);
      assert.equal(page.url(), `${base}/`, 'the token is not left in the address bar');
      const cookie = (await context.cookies(base)).find((entry) => entry.name === 'tmux_web_token');
      assert.ok(cookie, 'a cookie holds the session');
      assert.equal(cookie.httpOnly, true);
      assert.equal(cookie.sameSite, 'Lax', 'Lax, not Strict: the SSO login ends in a navigation from the identity provider');
      assert.equal(await page.evaluate(() => document.cookie.includes('tmux_web_token')), false, 'the page script cannot read it');

      // The WebSockets (events and attach) authenticate with that cookie too.
      await waitFor('the green pill', async () => (await page.getByTestId('status-pill').getAttribute('data-state')) === 'ok');
      const panel = await ui.addPanel(page, 'auth-a');
      await ui.waitForPrompt(page, panel.id);
      await ui.clickTerminal(page, panel.id);
      await ui.waitForTerminalFocus(page, panel.id);
      await ui.typeLine(page, 'echo signed-in');
      await ui.waitForScreen(page, panel.id, (line) => line === 'signed-in');
      // Saving the layout (a PUT with the cookie and the CSRF header) works as well.
      await waitFor('the layout to be saved', async () => {
        const saved = await fetch(`${base}/api/layout`, { headers: { Authorization: `Bearer ${TOKEN}` } }).then((response) => response.json());
        return Object.keys(saved.layout?.dockview?.panels ?? {}).length === 1;
      });
      await ui.reloadApp(page);
      assert.deepEqual((await ui.panels(page)).map((entry) => entry.session), ['auth-a'], 'a reload needs no token');
      assert.deepEqual(page.__errors, []);
    } finally {
      await context.close();
    }
  });
});

describe('robustness', () => {
  const ctx = useEnv('robust');

  it('a large multi-line paste reaches the program byte for byte', TEST, async () => {
    const { env } = ctx;
    env.tmux.newSession('paste-big', { cwd: env.dir('paste-big') });
    const page = await env.freshPage();
    const panel = await ui.addPanel(page, 'paste-big');
    await ui.waitForPrompt(page, panel.id);
    await ui.clickTerminal(page, panel.id);
    await ui.waitForTerminalFocus(page, panel.id);

    // 2000 lines of 99 characters and a newline: 200 000 bytes, each line different.
    const text = Array.from({ length: 2000 }, (_, index) => `line-${String(index).padStart(5, '0')}-${'x'.repeat(88)}`).join('\n') + '\n';
    assert.equal(text.length, 200_000);
    const md5 = createHash('md5').update(text).digest('hex');
    await ui.typeLine(page, 'cat | md5sum');
    await waitFor('cat to run', () => env.tmux.format('=paste-big:', '#{pane_current_command}') === 'cat');
    await ui.writeClipboard(page, text);
    await page.keyboard.press('Control+V');
    await page.keyboard.press('Control+D');
    await ui.waitForScreen(page, panel.id, (line) => line.startsWith(md5), { timeout: 30_000, what: `the md5 of the paste (${md5})` });
    assert.equal((await ui.panelOf(page, 'paste-big')).connected, true);
    assert.deepEqual(page.__errors, []);
  });

  it('the app loads and runs without failed requests or console errors', TEST, async () => {
    const { env } = ctx;
    for (const name of ['clean-a', 'clean-b']) env.tmux.newSession(name, { cwd: env.dir(name) });
    await env.closePage(); // another open window could save its own layout over this one
    const { context, page } = await newContext(env.browser);
    const failed = [];
    page.on('requestfailed', (request) => failed.push(`${request.method()} ${request.url()} ${request.failure()?.errorText}`));
    page.on('response', (response) => {
      if (response.status() >= 400) failed.push(`${response.status()} ${response.url()}`);
    });
    try {
      await env.resetLayout();
      await env.openApp(page);
      const a = await ui.addPanel(page, 'clean-a');
      const b = await ui.splitFrom(page, 'clean-a', 'right', 'clean-b');
      for (const panel of [a, b]) await ui.waitForPrompt(page, panel.id);
      await (await ui.groupControl(page, 'clean-b', 'header-maximize')).click();
      await waitFor('maximised', async () => (await ui.panelOf(page, 'clean-b')).maximized);
      await ui.reloadApp(page);
      await waitFor('the layout to be back', async () => (await ui.panels(page)).length === 2);
      assert.deepEqual(failed, [], 'every request succeeded');
      assert.deepEqual(page.__errors, [], 'no uncaught exception');
      assert.deepEqual(page.__console.filter((line) => line.startsWith('error')), [], 'no console error');
    } finally {
      await context.close();
    }
  });
});

describe('tmux stopped under an open page', () => {
  const ctx = useEnv('cold');

  it('says that tmux is not running, and creating a session starts it', TEST, async () => {
    const { env } = ctx;
    // A page that opens while tmux is down starts it (see 12-autostart); this one loses its
    // tmux after it opened, which the page leaves alone.
    env.tmux.newSession('seed', { cwd: env.dir('seed') });
    const page = await env.freshPage();
    const pill = page.getByTestId('status-pill');
    await waitFor('the green pill', async () => (await pill.getAttribute('data-state')) === 'ok');
    env.tmux.killServer();
    assert.ok(env.tmux.waitGone(), 'the tmux server is gone');
    assert.equal(env.tmux.hasServer(), false, 'no tmux server runs');
    await waitFor('the red pill', async () => (await pill.getAttribute('data-state')) === 'down');
    assert.equal(await pill.textContent(), 'tmux server not running');
    const empty = page.getByTestId('empty-state');
    await empty.waitFor({ state: 'visible' });
    assert.equal(await page.getByTestId('empty-heading').textContent(), 'tmux server is not running');
    assert.equal(await page.getByTestId('empty-open-all-btn').isDisabled(), true, 'nothing to open all of');
    await page.getByTestId('add-panel-btn').click();
    assert.match(await page.getByTestId('session-picker-empty').textContent(), /not running: create a session to start it/);
    await page.keyboard.press('Escape');

    // The first session starts the server.
    await page.getByTestId('empty-new-session-input').fill('first');
    await page.getByTestId('empty-new-session-input').press('Enter');
    const panel = await ui.waitForPanel(page, 'first');
    assert.equal(env.tmux.hasServer(), true, 'tmux-web started a tmux server');
    await ui.waitForPrompt(page, panel.id);
    await waitFor('the green pill', async () => (await pill.getAttribute('data-state')) === 'ok');
    assert.match(await pill.textContent(), / · 1 session$/);
    await ui.clickTerminal(page, panel.id);
    await ui.waitForTerminalFocus(page, panel.id);
    await ui.typeLine(page, 'echo cold-start-ok');
    await ui.waitForScreen(page, panel.id, (line) => line === 'cold-start-ok');
    // The server it started is configured like every tmux of this suite (no user config, plain sh).
    assert.equal(env.tmux.run('show-options', '-gv', 'default-command').trim(), 'exec /bin/sh');
    assert.deepEqual(page.__errors, []);
  });
});

describe('many sessions', () => {
  const ctx = useEnv('many');
  const COUNT = 40;

  it('lists them all; Open all adds a tab each and only the shown tab attaches', TEST, async () => {
    const { env } = ctx;
    const names = Array.from({ length: COUNT }, (_, index) => `many-${String(index).padStart(2, '0')}`);
    for (const name of names) env.tmux.newSession(name, { cwd: env.dir(name) });
    const page = await env.freshPage();
    await waitFor('the pill to count them', async () => new RegExp(` · ${COUNT} sessions$`).test(await page.getByTestId('status-pill').textContent()));

    await page.getByTestId('add-panel-btn').click();
    assert.equal(await page.getByTestId('session-picker-item').count(), COUNT);
    assert.match(await page.getByTestId('open-all-btn').textContent(), new RegExp(`Open all \\(${COUNT}\\)`));
    await page.getByTestId('open-all-btn').click();
    await waitFor('a panel per session', async () => (await ui.panels(page)).length === COUNT);
    assert.deepEqual(
      (await ui.panels(page)).map((panel) => panel.session).sort(),
      names,
      'one panel per session',
    );
    assert.equal((await ui.groups(page)).length, 1, 'all as tabs of one group');
    assert.deepEqual((await ui.tabTitles(page)).sort(), names, 'every tab shows its exact session name');
    await waitFor('the first tab to attach', async () => (await ui.panels(page)).filter((panel) => panel.connected).length === 1);
    assert.equal(ui.attachSockets(page).length, 1, 'only the shown tab opened a socket');

    // Tabs attach one by one as they are shown.
    for (const name of [names[7], names[23], names[39]]) {
      await ui.tabFor(page, name).scrollIntoViewIfNeeded();
      await ui.tabFor(page, name).click();
      const panel = await ui.waitForPanel(page, name);
      await ui.waitForPrompt(page, panel.id);
    }
    assert.equal(ui.attachSockets(page).length, 4);
    assert.deepEqual(page.__errors, []);
  });
});
