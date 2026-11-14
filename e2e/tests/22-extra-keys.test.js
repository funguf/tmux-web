/**
 * The extra-keys bar for phones: Esc, Tab, Ctrl, Alt, arrows and symbols that the soft keyboard
 * lacks, sticky Ctrl/Alt, the "Extra keys" setting, and a layout that keeps the terminal above
 * the bar (and the bar inside the visual viewport).
 */
import assert from 'node:assert/strict';
import fs from 'node:fs';
import path from 'node:path';
import { describe, it } from 'node:test';
import * as ui from '../lib/app.js';
import { TEST, useEnv } from '../lib/suite.js';
import { sleep, waitFor } from '../lib/wait.js';

const KEY_IDS = ['esc', 'tab', 'ctrl', 'alt', 'left', 'down', 'up', 'right', 'pipe', 'slash', 'dash', 'tilde', 'ctrlc'];
/** Set TMUX_WEB_SHOT_DIR to save screenshots of the bar (none by default). */
const SHOTS = process.env.TMUX_WEB_SHOT_DIR ?? '';

describe('extra keys bar', () => {
  const ctx = useEnv('extra-keys');
  let sessions = 0;

  /** A phone-sized touch page (like 17-touch-scroll) with one panel at a prompt. */
  async function openMobile({ viewport = { width: 390, height: 664 } } = {}) {
    const { env } = ctx;
    sessions += 1;
    const session = `xk-${sessions}`;
    env.tmux.newSession(session, { cwd: env.dir(session) });
    await env.closePage();
    await env.resetLayout();
    const context = await env.browser.newContext({ viewport, hasTouch: true, isMobile: true });
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
    await ui.waitForTerminalFocus(page, panel.id);
    return { page, panel, session };
  }

  const key = (page, id) => page.getByTestId(`extra-key-${id}`);
  const tap = (page, id) => key(page, id).tap();
  const barVisible = (page) => page.getByTestId('extra-keys').isVisible();
  const lines = (page, panel) => ui.screenLines(page, panel.id);

  /** Run `cat -v` on a cleared screen and wait for it to be reading. */
  async function startCat(page, panel) {
    await ui.typeLine(page, 'clear');
    await waitFor('the screen to clear', async () => (await lines(page, panel)).filter((l) => l.startsWith('$')).length === 1);
    await ui.typeLine(page, 'cat -v');
    await sleep(300);
  }
  async function endCat(page, panel) {
    await tap(page, 'ctrlc');
    await waitFor('cat to end', async () => (await lines(page, panel)).some((l) => l.includes('^C')) && (await lines(page, panel)).some((l) => l.startsWith('$')));
  }

  it('is hidden on a desktop context and shows all 13 keys on a phone', TEST, async () => {
    const { env } = ctx;
    const page = await env.freshPage();
    env.tmux.newSession('xk-desktop', { cwd: env.dir('xk-desktop') });
    await ui.addPanel(page, 'xk-desktop');
    assert.equal(await barVisible(page), false, 'no bar on a mouse device');
    assert.equal(await page.getByTestId('extra-keys').count(), 1);

    const mobile = await openMobile();
    const coarse = await mobile.page.evaluate(() => matchMedia('(pointer: coarse)').matches);
    console.log(`# (pointer: coarse) matches in the mobile context: ${coarse}`);
    assert.equal(await barVisible(mobile.page), true);
    const ids = await mobile.page.locator('.extra-keys .extra-key').evaluateAll((nodes) => nodes.map((n) => n.dataset.key));
    assert.deepEqual(ids, KEY_IDS);
    for (const id of KEY_IDS) await key(mobile.page, id).scrollIntoViewIfNeeded();
    const bar = mobile.page.getByTestId('extra-keys');
    assert.equal(await bar.getAttribute('role'), 'toolbar');
    assert.equal(await bar.getAttribute('aria-label'), 'Extra keys');
    for (const id of KEY_IDS) assert.ok(await key(mobile.page, id).getAttribute('aria-label'));
    assert.deepEqual(mobile.page.__errors, []);
  });

  it('esc, arrows (normal and application cursor mode) and ^C reach the terminal', TEST, async () => {
    const { page, panel } = await openMobile();
    await startCat(page, panel);
    await tap(page, 'esc');
    await tap(page, 'up');
    await waitFor('^[ then ^[[A', async () => (await lines(page, panel)).some((l) => l.includes('^[^[[A')));
    await tap(page, 'pipe');
    await tap(page, 'slash');
    await tap(page, 'dash');
    await tap(page, 'tilde');
    await waitFor('the symbols', async () => (await lines(page, panel)).some((l) => l.includes('|/-~')));
    await endCat(page, panel);

    await ui.typeLine(page, "printf '\\033[?1h'");
    await startCat(page, panel);
    await tap(page, 'up');
    await waitFor('^[OA', async () => (await lines(page, panel)).some((l) => l.includes('^[OA')));
    await endCat(page, panel);
    await ui.typeLine(page, "printf '\\033[?1l'");
    assert.deepEqual(page.__errors, []);
  });

  it('Ctrl and Alt are sticky for one key, or locked by a double tap', TEST, async () => {
    const { page, panel } = await openMobile();
    await startCat(page, panel);
    const has = async (text) => (await lines(page, panel)).some((l) => l === text);

    await tap(page, 'ctrl');
    assert.equal(await key(page, 'ctrl').getAttribute('aria-pressed'), 'true');
    assert.ok((await key(page, 'ctrl').getAttribute('class')).includes('is-armed'));
    await page.keyboard.type('g');
    await waitFor('^G', async () => has('^G'));
    assert.equal(await key(page, 'ctrl').getAttribute('aria-pressed'), 'false');
    await page.keyboard.type('g');
    await waitFor('^G then a plain g', async () => has('^Gg'));
    await page.keyboard.press('Enter');

    await tap(page, 'alt');
    await page.keyboard.type('x');
    await waitFor('^[x', async () => has('^[x'));
    await page.keyboard.press('Enter');

    await tap(page, 'ctrl');
    await tap(page, 'up');
    await waitFor('ctrl+up', async () => (await lines(page, panel)).some((l) => l.includes('^[[1;5A')));
    assert.equal(await key(page, 'ctrl').getAttribute('aria-pressed'), 'false');
    await tap(page, 'alt');
    await tap(page, 'ctrl');
    await tap(page, 'left');
    await waitFor('ctrl+alt+left', async () => (await lines(page, panel)).some((l) => l.includes('^[[1;7D')));
    await page.keyboard.press('Enter');

    // Double tap locks.
    await key(page, 'ctrl').dispatchEvent('click');
    await key(page, 'ctrl').dispatchEvent('click');
    assert.ok((await key(page, 'ctrl').getAttribute('class')).includes('is-locked'));
    await page.keyboard.type('gg');
    await waitFor('^G^G', async () => has('^G^G'));
    assert.ok((await key(page, 'ctrl').getAttribute('class')).includes('is-locked'), 'still locked');
    await sleep(400);
    await tap(page, 'ctrl');
    assert.equal(await key(page, 'ctrl').getAttribute('aria-pressed'), 'false');
    await page.keyboard.type('g');
    await waitFor('a plain g', async () => (await lines(page, panel)).some((l) => l.startsWith('^G^Gg')));
    await page.keyboard.press('Enter');
    await endCat(page, panel);
    assert.deepEqual(page.__errors, []);
  });

  it('tapping a key leaves the keyboard focus in the terminal', TEST, async () => {
    const { page, panel } = await openMobile();
    for (const id of ['esc', 'ctrl', 'up', 'pipe', 'ctrl']) {
      await tap(page, id);
      const focused = await page.evaluate(() => document.activeElement?.className ?? '');
      assert.ok(focused.includes('xterm-helper-textarea'), `${id} stole the focus: ${focused}`);
    }
    await ui.waitForTerminalFocus(page, panel.id);
    await tap(page, 'ctrlc');
  });

  it('the Extra keys setting (Auto / Always / Never) persists across a reload', TEST, async () => {
    const { env } = ctx;
    const mobile = await openMobile();
    const { page } = mobile;
    const choose = async (value) => {
      await page.getByTestId('settings-btn').tap();
      await page.getByTestId('extra-keys-mode').selectOption(value);
      await page.keyboard.press('Escape');
    };
    await choose('off');
    assert.equal(await barVisible(page), false);
    assert.equal(await page.evaluate(() => localStorage.getItem('tmux-web.extraKeys')), 'off');
    await ui.reloadApp(page);
    assert.equal(await barVisible(page), false, 'Never survives a reload');
    await choose('auto');
    assert.equal(await barVisible(page), true);
    await ui.reloadApp(page);
    assert.equal(await barVisible(page), true);

    // Desktop: Always shows the bar and the choice persists.
    const desktop = await env.freshPage();
    assert.equal(await barVisible(desktop), false);
    await desktop.getByTestId('settings-btn').click();
    assert.equal(await desktop.getByTestId('extra-keys-mode').inputValue(), 'auto');
    await desktop.getByTestId('extra-keys-mode').selectOption('on');
    assert.equal(await barVisible(desktop), true);
    await ui.reloadApp(desktop);
    assert.equal(await barVisible(desktop), true);
    await desktop.getByTestId('settings-btn').click();
    assert.equal(await desktop.getByTestId('extra-keys-mode').inputValue(), 'on');
    await desktop.getByTestId('extra-keys-mode').selectOption('auto');
    assert.equal(await barVisible(desktop), false);
    assert.equal(await desktop.evaluate(() => localStorage.getItem('tmux-web.extraKeys')), 'auto');
  });

  it('the terminal ends above the bar, also in a short viewport', TEST, async () => {
    const { page, panel } = await openMobile();
    const geometry = async () => {
      const screen = await ui.terminalPanel(page, panel.id).locator('.xterm-screen').boundingBox();
      const bar = await page.getByTestId('extra-keys').boundingBox();
      const view = await page.evaluate(() => ({ h: window.visualViewport?.height ?? innerHeight }));
      return { screenBottom: screen.y + screen.height, barTop: bar.y, barBottom: bar.y + bar.height, height: view.h };
    };
    let g = await geometry();
    assert.ok(g.screenBottom <= g.barTop + 0.5, `terminal overlaps the bar: ${JSON.stringify(g)}`);
    assert.ok(g.barBottom <= g.height + 0.5, `bar outside the viewport: ${JSON.stringify(g)}`);

    if (SHOTS) {
      fs.mkdirSync(SHOTS, { recursive: true });
      await page.screenshot({ path: path.join(SHOTS, 'extra-keys.png') });
      await tap(page, 'ctrl');
      await page.screenshot({ path: path.join(SHOTS, 'extra-keys-ctrl.png') });
      await tap(page, 'ctrl');
    }

    await page.setViewportSize({ width: 390, height: 400 });
    await waitFor('the layout to follow the viewport', async () => {
      g = await geometry();
      return g.barBottom <= 400.5 && g.screenBottom <= g.barTop + 0.5 && g.height <= 400.5;
    });
    assert.deepEqual(page.__errors, []);
  });
});
