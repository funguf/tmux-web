/**
 * Requirement 6: typing works (`echo tmw-$((40+2))` shows `tmw-42`), and resizing the browser
 * resizes the tmux window.
 */
import assert from 'node:assert/strict';
import { before, describe, it } from 'node:test';
import * as ui from '../lib/app.js';
import { TEST, useEnv } from '../lib/suite.js';
import { sleep, waitFor } from '../lib/wait.js';

describe('typing and resizing', () => {
  const ctx = useEnv('typing');

  before(() => {
    const { env } = ctx;
    for (const name of ['ty-a', 'ty-b', 'ty-resize']) env.tmux.newSession(name, { cwd: env.dir(name) });
  });

  it('typed commands run in the tmux session and their output shows on screen', TEST, async () => {
    const { env } = ctx;
    const page = await ctx.env.freshPage();
    const a = await ui.addPanel(page, 'ty-a');
    await ui.waitForPrompt(page, a.id);
    await ui.clickTerminal(page, a.id);
    await ui.waitForTerminalFocus(page, a.id);

    await ui.typeLine(page, 'echo tmw-$((40+2))');
    // The command line itself contains "$((40+2))": a line that is exactly "tmw-42" proves the
    // shell evaluated it.
    await ui.waitForScreen(page, a.id, (line) => line === 'tmw-42');
    await waitFor('tmux to hold the output too', () => env.tmux.capture('=ty-a:').split('\n').includes('tmw-42'));

    // Editing keys and non-ASCII text.
    await page.keyboard.type('echo abcX');
    await page.keyboard.press('Backspace');
    await page.keyboard.type('d');
    await page.keyboard.press('Enter');
    await ui.waitForScreen(page, a.id, (line) => line === 'abcd');
    await ui.typeLine(page, 'echo café-✓-ünï');
    await ui.waitForScreen(page, a.id, (line) => line === 'café-✓-ünï');

    // Ctrl+C with nothing selected still interrupts the foreground program.
    await ui.typeLine(page, 'sleep 120');
    await waitFor('sleep to run', () => env.tmux.format('=ty-a:', '#{pane_current_command}') === 'sleep');
    await page.keyboard.press('Control+C');
    await waitFor('sleep to be interrupted', () => env.tmux.format('=ty-a:', '#{pane_current_command}') === 'sh');
    assert.deepEqual(page.__errors, []);
  });

  it('control and navigation keys reach the program as the terminal sends them', TEST, async () => {
    const { env } = ctx;
    const name = 'ty-keys';
    env.tmux.newSession(name, { cwd: env.dir(name) });
    const page = await env.freshPage();
    const panel = await ui.addPanel(page, name);
    await ui.waitForPrompt(page, panel.id);
    await ui.clickTerminal(page, panel.id);
    await ui.waitForTerminalFocus(page, panel.id);
    // `cat -vT` prints every control character it reads in caret notation: ESC is ^[, Tab is ^I.
    await ui.typeLine(page, 'cat -vT');
    await waitFor('cat -vT to run', () => env.tmux.format(`=${name}:`, '#{pane_current_command}') === 'cat');

    const cases = [
      ['Tab', '^I'],
      ['Escape', '^['],
      ['Control+A', '^A'],
      ['Control+E', '^E'],
      ['ArrowUp', '^[[A'],
      ['ArrowDown', '^[[B'],
      ['ArrowRight', '^[[C'],
      ['ArrowLeft', '^[[D'],
      ['Alt+x', '^[x'],
    ];
    for (const [key, shown] of cases) {
      await page.keyboard.press(key);
      await page.keyboard.press('Enter');
      await ui.waitForScreen(page, panel.id, (line) => line === shown, { what: `${key} to arrive as ${shown}` });
    }
    // None of it was taken by the page: no toast, no menu, the picker did not open.
    assert.equal(await page.getByTestId('session-picker').isVisible(), false);
    assert.equal(await page.getByTestId('context-menu').count(), 0);
    assert.deepEqual(page.__errors, []);
  });

  it('clicking a tab, the active one or another, gives its terminal the keyboard', TEST, async () => {
    const { env } = ctx;
    const page = await env.freshPage();
    const a = await ui.addPanel(page, 'ty-a');
    const b = await ui.addPanel(page, 'ty-b'); // ty-b is active, ty-a hidden
    await ui.waitForPrompt(page, b.id);

    // The active tab: the tab takes the focus on mouse down, the terminal must get it back.
    await ui.tabFor(page, 'ty-b').click();
    await ui.waitForTerminalFocus(page, b.id);
    await ui.typeLine(page, 'echo from-active-tab');
    await ui.waitForScreen(page, b.id, (line) => line === 'from-active-tab');

    // Another tab: it becomes visible a moment after the click, and gets the focus then.
    await ui.tabFor(page, 'ty-a').click();
    await ui.waitForTerminalFocus(page, a.id);
    await ui.typeLine(page, 'echo from-other-tab');
    await ui.waitForScreen(page, a.id, (line) => line === 'from-other-tab');
    await waitFor('the text to be in ty-a only', () => env.tmux.capture('=ty-a:').includes('from-other-tab') && !env.tmux.capture('=ty-b:').includes('from-other-tab'));
    assert.deepEqual(page.__errors, []);
  });

  it('resizing the browser resizes the terminal and the tmux window', TEST, async () => {
    const { env } = ctx;
    const page = await env.freshPage();
    const panel = await ui.addPanel(page, 'ty-resize');
    await ui.waitForPrompt(page, panel.id);
    const statusLines = env.tmux.run('show-options', '-gv', 'status').trim() === 'on' ? 1 : 0;

    /** Wait until the terminal, the attach client and the tmux window all agree, and return the size. */
    async function settled(label) {
      return waitFor(
        `${label}: terminal, tmux client and tmux window to agree`,
        async () => {
          const term = await ui.termSize(page, panel.id);
          const client = env.tmux.clientSize('ty-resize');
          const window = env.tmux.windowSize('ty-resize');
          const agree =
            term && client && client.width === term.cols && client.height === term.rows && window.width === term.cols && window.height === term.rows - statusLines;
          return agree ? { term, window } : null;
        },
        { timeout: 10_000 },
      );
    }

    const first = await settled('initial size');
    const viewports = [
      { width: 900, height: 600 },
      { width: 1500, height: 900 },
      { width: 1100, height: 520 },
    ];
    const observed = [{ ...(await page.evaluate(() => ({ width: window.innerWidth, height: window.innerHeight }))), ...first.term }];
    for (const viewport of viewports) {
      const previous = observed.at(-1);
      await page.setViewportSize(viewport);
      await waitFor(
        `the terminal to follow the ${viewport.width}x${viewport.height} window`,
        async () => {
          const term = await ui.termSize(page, panel.id);
          return term.cols !== previous.cols && term.rows !== previous.rows;
        },
        { timeout: 10_000 },
      );
      const now = await settled(`${viewport.width}x${viewport.height}`);
      observed.push({ ...viewport, ...now.term });
    }
    // A bigger browser window gives a bigger terminal, a smaller one a smaller terminal.
    for (const x of observed) {
      for (const y of observed) {
        if (x.width < y.width) assert.ok(x.cols < y.cols, `columns follow the width: ${JSON.stringify({ x, y })}`);
        if (x.height < y.height) assert.ok(x.rows < y.rows, `rows follow the height: ${JSON.stringify({ x, y })}`);
      }
    }

    // The shell inside the pane sees the new size too: `stty size` prints "<rows> <cols>" of the
    // pane, which is the window minus the status line. tmux passes a resize on to a pane up to
    // 250 ms late when resizes come in quick succession, so ask again until it agrees.
    await ui.clickTerminal(page, panel.id);
    await ui.waitForTerminalFocus(page, panel.id);
    const { cols, rows } = observed.at(-1);
    const expected = `${rows - statusLines} ${cols}`;
    await waitFor(
      `the shell to see a terminal of ${expected} rows and columns`,
      async () => {
        await ui.typeLine(page, 'stty size');
        await sleep(600);
        return (await ui.screenLines(page, panel.id)).includes(expected);
      },
      { timeout: 15_000, interval: 100 },
    );
    assert.deepEqual(page.__errors, []);
  });
});
