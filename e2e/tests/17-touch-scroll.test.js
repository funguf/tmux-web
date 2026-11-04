/**
 * Finger scrolling: a vertical swipe on a terminal panel scrolls the tmux history (tmux copy
 * mode), a tap does not, and nothing of the gesture is typed into the terminal. The tmux
 * `mouse` option stays off throughout.
 */
import assert from 'node:assert/strict';
import fs from 'node:fs';
import path from 'node:path';
import { describe, it } from 'node:test';
import * as ui from '../lib/app.js';
import { TEST, useEnv } from '../lib/suite.js';
import { sleep, staysFalse, waitFor } from '../lib/wait.js';

describe('touch scrolling', () => {
  const ctx = useEnv('touch-scroll');
  let sessions = 0;

  /** A phone-sized touch page with a panel that has printed 300 lines. */
  async function openTouchTerminal({ mouseApp = false } = {}) {
    const { env } = ctx;
    sessions += 1;
    const session = `ts-${sessions}`;
    env.tmux.newSession(session, { cwd: env.dir(session) });
    assert.equal(env.tmux.run('show-options', '-gv', 'mouse').trim(), 'off');
    await env.closePage();
    await env.resetLayout();
    const context = await env.browser.newContext({
      viewport: { width: 390, height: 664 },
      hasTouch: true,
      isMobile: true,
    });
    const page = await context.newPage();
    env.context = context;
    env.page = page;
    page.__errors = [];
    page.on('pageerror', (err) => page.__errors.push(String(err?.stack ?? err)));
    page.on('console', (msg) => {
      if (msg.type() === 'error') page.__errors.push(`console: ${msg.text()}`);
    });
    await env.openApp(page);
    const panel = await ui.addPanel(page, session);
    await ui.waitForPrompt(page, panel.id);
    await ui.clickTerminal(page, panel.id);
    const received = path.join(env.dir(session), 'received.hex');
    if (mouseApp) {
      // A full-screen app that asks for SGR mouse reports and logs every byte it receives.
      const script = path.join(env.dir(session), 'mouse-app.py');
      fs.writeFileSync(
        script,
        [
          'import os, sys, termios, tty',
          'tty.setraw(0)',
          "sys.stdout.write('\\x1b[?1049h\\x1b[?1000h\\x1b[?1006h'); sys.stdout.flush()",
          `log = open(${JSON.stringify(received)}, 'ab', buffering=0)`,
          'while True:',
          '    data = os.read(0, 4096)',
          '    if not data: break',
          '    log.write(data)',
          '',
        ].join('\n'),
      );
      await ui.typeLine(page, `python3 ${script}`);
      await waitFor('the app to turn the mouse mode on', async () => (await ui.mouseMode(page, panel.id)) !== 'none');
    } else {
      await ui.typeLine(page, 'seq 1 300');
      await ui.waitForScreen(page, panel.id, (line) => line === '300');
    }
    await sleep(300);
    await page.evaluate((id) => {
      window.__sent = [];
      window.__tmuxWeb.terminal(id).onData((data) => window.__sent.push(JSON.stringify(data)));
    }, panel.id);
    const box = await ui.terminalPanel(page, panel.id).boundingBox();
    const cdp = await context.newCDPSession(page);
    const x = box.x + box.width / 2;
    const touch = (type, y) =>
      cdp.send('Input.dispatchTouchEvent', { type, touchPoints: type === 'touchEnd' ? [] : [{ x, y }] });
    /** One-finger swipe from `fromY` by `dy` px (positive = finger down) in 20px steps. */
    const swipe = async (fromY, dy) => {
      await touch('touchStart', fromY);
      const steps = Math.ceil(Math.abs(dy) / 20);
      for (let i = 1; i <= steps; i += 1) {
        await touch('touchMove', fromY + Math.sign(dy) * Math.min(Math.abs(dy), i * 20));
        await sleep(16);
      }
      await touch('touchEnd', fromY + dy);
    };
    const scrollOffsets = () =>
      page.evaluate(() => ({
        y: window.scrollY,
        top: document.querySelector('.terminal-panel').getBoundingClientRect().top,
      }));
    const state = () =>
      env.tmux.run('display-message', '-p', '-t', `=${session}:`, '#{pane_in_mode} #{scroll_position}').split(' ');
    return { page, panel, box, swipe, scrollOffsets, received, touch, state, sent: () => page.evaluate(() => window.__sent.slice()) };
  }

  it('a swipe with the finger down scrolls back, a swipe up scrolls forward and leaves copy mode', TEST, async () => {
    const { page, panel, box, swipe, scrollOffsets, state, sent } = await openTouchTerminal();
    assert.equal(state()[0], '0');
    assert.equal(
      await ui.terminalPanel(page, panel.id).locator('.terminal-host').evaluate((e) => getComputedStyle(e).touchAction),
      'none',
      'the terminal host takes no native pan or zoom',
    );
    const before = await scrollOffsets();

    await swipe(box.y + 100, 300);
    await waitFor('tmux to enter copy mode', async () => state()[0] === '1' && Number(state()[1]) > 0);
    const back = Number(state()[1]);

    // Swiping up a bit scrolls forward again, but stays in copy mode.
    await swipe(box.y + 400, -60);
    await waitFor('the history to scroll forward', async () => Number(state()[1]) < back);
    assert.equal(state()[0], '1');

    // Far enough up reaches the live screen, which ends copy mode.
    await swipe(box.y + 500, -400);
    await waitFor('copy mode to end at the bottom', async () => state()[0] === '0');

    assert.deepEqual(await scrollOffsets(), before, 'the page and the panel did not move under the finger');
    assert.deepEqual(await sent(), [], 'the swipes typed nothing into the terminal');
    assert.deepEqual(page.__errors, []);
  });

  it('a tap does not scroll', TEST, async () => {
    const { page, box, touch, state, sent } = await openTouchTerminal();
    const y = box.y + 200;
    await touch('touchStart', y);
    await touch('touchEnd', y);
    await staysFalse('a tap entered copy mode', async () => state()[0] === '1', { duration: 700 });
    assert.deepEqual(await sent(), []);
    assert.deepEqual(page.__errors, []);
  });

  it('a program that tracks the mouse gets the swipe as wheel reports, not tmux copy mode', TEST, async () => {
    const { page, box, swipe, state, sent, received } = await openTouchTerminal({ mouseApp: true });
    const bytes = () => (fs.existsSync(received) ? fs.readFileSync(received, 'latin1') : '');
    /** Every report in the log must be an SGR wheel report of the given button (64 up, 65 down). */
    const onlyWheel = (button) => {
      const reports = bytes().match(/\x1b\[<\d+;\d+;\d+[Mm]/g) ?? [];
      assert.ok(reports.length > 0, 'the app received reports');
      assert.equal(reports.join(''), bytes(), 'nothing but SGR reports (no arrow keys)');
      assert.ok(reports.every((r) => r.startsWith(`\x1b[<${button};`)), `all wheel ${button}: ${JSON.stringify(reports)}`);
    };

    await swipe(box.y + 100, 300);
    await waitFor('a wheel-up report', async () => bytes().includes('\x1b[<64;'));
    onlyWheel(64);
    assert.equal(state()[0], '0', 'tmux stayed out of copy mode');

    fs.writeFileSync(received, '');
    await swipe(box.y + 500, -300);
    await waitFor('a wheel-down report', async () => bytes().includes('\x1b[<65;'));
    onlyWheel(65);
    assert.equal(state()[0], '0');
    // What the terminal itself emitted is the wheel reports and nothing else.
    const emitted = (await sent()).map((data) => JSON.parse(data));
    assert.ok(emitted.length > 0 && emitted.every((data) => /^\x1b\[<6[45];\d+;\d+M$/.test(data)), JSON.stringify(emitted));
    assert.deepEqual(page.__errors, []);
  });
});
