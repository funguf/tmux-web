/**
 * A page served over plain http on a host name that is not localhost is not a secure context:
 * `navigator.clipboard` does not exist there. Copy must fall back to a hidden textarea and
 * `document.execCommand('copy')`, paste must work through the browser's own paste event, and
 * the menu's Paste must say so instead of failing silently (CONTRACT §8.6 items 8, 9, 12).
 *
 * `tmux.test` is mapped to 127.0.0.1 in the browser and allowed on the server (--allow-host).
 * The clipboard is read and written from a second page on 127.0.0.1, which is a secure context.
 */
import assert from 'node:assert/strict';
import fs from 'node:fs';
import { describe, it } from 'node:test';
import * as ui from '../lib/app.js';
import { makePng, pngSize } from '../lib/png.js';
import { TEST, useEnv } from '../lib/suite.js';
import { sleep, staysFalse, waitFor } from '../lib/wait.js';

const HOST = 'tmux.test';

describe('a plain-http host name (no secure context)', () => {
  const ctx = useEnv('insecure', { serveArgs: ['--allow-host', HOST] });
  let counter = 0;

  /** The terminal page on http://tmux.test, plus helpers for the clipboard on a secure page. */
  async function openTerminal() {
    const { env } = ctx;
    counter += 1;
    const name = `ins-${counter}`;
    env.tmux.newSession(name, { cwd: env.dir(name) });
    const page = await env.freshPage({ url: `http://${HOST}:${new URL(env.url).port}/` });
    assert.equal(await page.evaluate(() => window.isSecureContext), false, 'the page is not a secure context');
    assert.equal(await page.evaluate(() => navigator.clipboard === undefined), true, 'so there is no async clipboard API');
    const panel = await ui.addPanel(page, name);
    await ui.waitForPrompt(page, panel.id);
    await ui.clickTerminal(page, panel.id);
    await ui.waitForTerminalFocus(page, panel.id);

    /** Run `work(securePage)` on a page of 127.0.0.1, then bring the terminal page back. */
    async function viaSecurePage(work) {
      const secure = await env.context.newPage();
      try {
        await secure.goto(`${env.url}/api/info`);
        await secure.bringToFront();
        return await work(secure);
      } finally {
        await secure.close();
        await page.bringToFront();
      }
    }
    return {
      page,
      panel,
      readClipboard: () => viaSecurePage((secure) => secure.evaluate(() => navigator.clipboard.readText())),
      writeClipboard: (text) => viaSecurePage((secure) => secure.evaluate((value) => navigator.clipboard.writeText(value), text)),
      writeClipboardPng: (png) =>
        viaSecurePage((secure) =>
          secure.evaluate(async (base64) => {
            const bytes = Uint8Array.from(atob(base64), (c) => c.charCodeAt(0));
            await navigator.clipboard.write([new ClipboardItem({ 'image/png': new Blob([bytes], { type: 'image/png' }) })]);
          }, png.toString('base64')),
        ),
    };
  }

  it('copy-on-select and Ctrl+C copy through the textarea fallback', TEST, async () => {
    const { page, panel, readClipboard, writeClipboard } = await openTerminal();
    await ui.typeLine(page, 'echo legacy-copy-word');
    await ui.waitForScreen(page, panel.id, (line) => line === 'legacy-copy-word');

    await writeClipboard('sentinel');
    await ui.dragSelect(page, panel.id, 'legacy-copy-word');
    assert.equal(await ui.selectionText(page, panel.id), 'legacy-copy-word');
    await sleep(600); // copy-on-select runs 200 ms after the selection settled, while this page still has the focus
    await waitFor('copy-on-select to reach the clipboard', async () => (await readClipboard()) === 'legacy-copy-word', { timeout: 10_000, interval: 300 });
    // The copy did not disturb the terminal: the selection is intact and the keyboard is still its own.
    assert.equal(await ui.selectionText(page, panel.id), 'legacy-copy-word');
    await ui.waitForTerminalFocus(page, panel.id);

    // Ctrl+C with a selection copies (and clears it); nothing is sent to the shell.
    await writeClipboard('sentinel 2');
    await page.bringToFront();
    await ui.doubleClickWord(page, panel.id, 'legacy-copy-word');
    await page.keyboard.press('Control+Shift+C');
    await waitFor('Ctrl+Shift+C to copy', async () => (await readClipboard()) === 'legacy-copy-word', { timeout: 10_000, interval: 300 });
    assert.ok(!(await ui.screenLines(page, panel.id)).some((line) => line.includes('^')));
    assert.deepEqual(page.__errors, []);
  });

  it('Ctrl+V pastes text and images; the menu Paste explains that it cannot read the clipboard', TEST, async () => {
    const { env } = ctx;
    const { page, panel, writeClipboard, writeClipboardPng } = await openTerminal();

    await writeClipboard('legacy-paste');
    await ui.clickTerminal(page, panel.id);
    await ui.waitForTerminalFocus(page, panel.id);
    await page.keyboard.press('Control+V');
    await ui.waitForScreen(page, panel.id, (line) => line === '$ legacy-paste');

    const before = env.uploads().length;
    await writeClipboardPng(makePng({ width: 11, height: 5, rgb: [3, 3, 200] }));
    await ui.clickTerminal(page, panel.id);
    await page.keyboard.press('Control+V');
    await waitFor('the pasted image to be stored', () => env.uploads().length === before + 1, { timeout: 10_000 });
    const [file] = env.uploads().slice(before).map((name) => `${env.uploadsDir}/${name}`);
    assert.deepEqual(pngSize(fs.readFileSync(file)), { width: 11, height: 5 });
    await ui.waitForScreen(page, panel.id, (line) => line === `$ legacy-paste${file} `, { what: 'the image path next to the text' });

    // The menu's Paste cannot read the clipboard here, and says what to do.
    const box = await ui.terminalPanel(page, panel.id).boundingBox();
    await page.mouse.click(box.x + 300, box.y + 300, { button: 'right' });
    await page.locator('[data-testid="context-menu-item"][data-action="paste"]').click();
    await waitFor('the hint', async () => (await ui.toasts(page)).some((text) => /not available here: use Ctrl\+V/.test(text)));
    assert.deepEqual(page.__errors, []);
  });

  it('says so when the browser refuses the copy', TEST, async () => {
    const { page, panel } = await openTerminal();
    await page.evaluate(() => {
      document.execCommand = () => false; // the browser refuses
    });
    await ui.typeLine(page, 'echo refused-copy');
    await ui.waitForScreen(page, panel.id, (line) => line === 'refused-copy');
    await ui.doubleClickWord(page, panel.id, 'refused-copy');
    // copy-on-select is quiet; the explicit shortcut is not.
    await staysFalse('copy-on-select showed an error', async () => (await ui.toasts(page)).some((text) => /Copy failed/.test(text)), { duration: 700 });
    await page.keyboard.press('Control+Shift+C');
    await waitFor('the failure toast', async () => (await ui.toasts(page)).some((text) => /Copy failed/.test(text)));
  });
});
