/**
 * Opening tmux-web starts tmux. A page whose first status says that tmux is not running asks
 * the server to start it, once: a plain default session is created. A tmux that is stopped on
 * purpose while the page stays open stays down until the user asks for it with the empty
 * state's "Start tmux" button.
 *
 * Every scenario has its own environment (its own tmux server, state and browser).
 */
import assert from 'node:assert/strict';
import fs from 'node:fs';
import path from 'node:path';
import { describe, it } from 'node:test';
import * as ui from '../lib/app.js';
import { TEST, useEnv } from '../lib/suite.js';
import { sleep, staysFalse, waitFor } from '../lib/wait.js';

const START_PATH = '/api/server/start';

/** The start requests `page` makes, as they happen: `starts.length` is how many so far. */
function watchStarts(page) {
  const starts = [];
  page.on('request', (request) => {
    if (request.method() === 'POST' && new URL(request.url()).pathname === START_PATH) starts.push(request.url());
  });
  return starts;
}

/** Names of the sessions the empty state lists. */
const listed = (page) => page.getByTestId('empty-session-item').evaluateAll((items) => items.map((item) => item.dataset.session));

/** Kill the tmux server under the open page and wait until the page knows. */
async function killTmux(env, page) {
  env.tmux.killServer();
  assert.ok(env.tmux.waitGone(), 'the tmux server is gone');
  await waitFor('the red pill', async () => (await page.getByTestId('status-pill').getAttribute('data-state')) === 'down');
}

const failure = (status, error, message) => ({ status, contentType: 'application/json', body: JSON.stringify({ error, message }) });

describe('opening the page starts tmux', () => {
  const ctx = useEnv('autostart');

  it('starts tmux once when the page opens, and leaves it down when it is stopped under the open page', { timeout: 150_000 }, async () => {
    const { env } = ctx;
    assert.equal(env.tmux.hasServer(), false, 'no tmux server runs yet');

    // A page that opens while tmux is down starts it: the default session, and only that.
    let starts;
    const page = await env.freshPage({ beforeOpen: (opened) => (starts = watchStarts(opened)) });
    const pill = page.getByTestId('status-pill');
    await waitFor('the green pill', async () => (await pill.getAttribute('data-state')) === 'ok');
    assert.match(await pill.textContent(), / · 1 session$/);
    assert.deepEqual(env.tmux.sessions(), ['main']);
    await page.getByTestId('empty-state').waitFor({ state: 'visible' });
    await waitFor('main in the empty state', async () => (await listed(page)).join() === 'main');
    assert.equal(await page.getByTestId('empty-heading').textContent(), 'No panels open');
    assert.equal(await page.getByTestId('empty-start-tmux').isVisible(), false, 'tmux runs: nothing to start');
    await waitFor('the toast', async () => (await ui.toasts(page)).includes('Started tmux: session main'));
    assert.equal(starts.length, 1, 'the page asked once');

    // tmux dies under the open page. That is deliberate as far as the page can tell: it stays down.
    await killTmux(env, page);
    const start = page.getByTestId('empty-start-tmux');
    await start.waitFor({ state: 'visible' });
    assert.equal(await page.getByTestId('empty-heading').textContent(), 'tmux server is not running');
    assert.equal(await page.locator('.empty-hint').textContent(), 'Start tmux to begin.');
    assert.equal(await start.isDisabled(), false);
    await staysFalse('tmux was started again without being asked', () => env.tmux.hasServer(), { duration: 5500 });
    assert.equal((await env.api('GET', '/api/status')).json.running, false);
    assert.equal(await pill.getAttribute('data-state'), 'down');
    assert.equal(starts.length, 1, 'the page did not ask again');

    // The button asks, and only the default session comes back.
    await start.click();
    await waitFor('main to be back', () => env.tmux.sessions().join() === 'main', { timeout: 30_000 });
    await waitFor('the toast', async () => (await ui.toasts(page)).includes('Started tmux: session main'));
    await waitFor('the green pill', async () => (await pill.getAttribute('data-state')) === 'ok');
    await waitFor('main in the empty state', async () => (await listed(page)).join() === 'main');
    assert.equal(await start.isVisible(), false, 'tmux runs again: nothing to start');
    assert.equal(starts.length, 2);

    // And again, after tmux died once more.
    await killTmux(env, page);
    await start.waitFor({ state: 'visible' });
    await start.click();
    await waitFor('main to be created', () => env.tmux.sessions().join() === 'main', { timeout: 30_000 });
    await waitFor('the toast', async () => (await ui.toasts(page)).includes('Started tmux: session main'));
    await waitFor('the green pill', async () => (await pill.getAttribute('data-state')) === 'ok');
    assert.equal(starts.length, 3);
    assert.equal(fs.existsSync(path.join(env.stateDir, 'snapshots')), false, 'no snapshots are written');
    assert.deepEqual(page.__errors, []);
  });
});

describe('starting tmux fails', () => {
  const ctx = useEnv('autostart-errors');

  it('reports the failure of the automatic start in a toast and the failure of the button in the empty state', TEST, async () => {
    const { env } = ctx;
    let attempts = 0;
    const page = await env.freshPage({
      beforeOpen: (opened) =>
        opened.route(`**${START_PATH}`, async (route) => {
          attempts += 1;
          if (attempts === 1) return route.fulfill(failure(500, 'tmux_error', 'tmux broke'));
          if (attempts === 2) {
            await sleep(1000); // long enough to see the button wait for the answer
            return route.fulfill(failure(409, 'busy', 'tmux is busy right now'));
          }
          return route.continue();
        }),
    });

    // The automatic start failed: an error toast, tmux is still down, the button is there.
    await waitFor('the error toast', async () => (await ui.toasts(page)).includes('Could not start tmux: tmux broke'));
    assert.equal(env.tmux.hasServer(), false);
    const start = page.getByTestId('empty-start-tmux');
    await start.waitFor({ state: 'visible' });
    assert.equal(await start.isDisabled(), false);
    assert.equal(await page.getByTestId('empty-error').isVisible(), false);

    // The button waits for the answer, then reports the failure where the empty state reports errors.
    await start.click();
    await waitFor('the button to be disabled while the request is pending', () => start.isDisabled());
    const error = page.getByTestId('empty-error');
    await waitFor('the error in the empty state', async () => (await error.textContent()) === 'tmux is busy right now');
    assert.equal(await error.isVisible(), true);
    assert.equal(await start.isDisabled(), false, 'the button is usable again');
    assert.equal(
      (await ui.toasts(page)).some((text) => /busy/i.test(text)),
      false,
      'the button does not toast its failure',
    );
    assert.equal(env.tmux.hasServer(), false);

    // The third try goes through, and clears the error.
    await start.click();
    await waitFor('main to be created', () => env.tmux.sessions().join() === 'main', { timeout: 30_000 });
    await waitFor('the error to go', async () => !(await error.isVisible()));
    await waitFor('main in the empty state', async () => (await listed(page)).join() === 'main');
    assert.equal(attempts, 3);
    assert.deepEqual(page.__errors, []);
  });
});
