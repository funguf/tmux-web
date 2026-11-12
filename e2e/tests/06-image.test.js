/**
 * Requirement 7: image and file paste. A pasted PNG lands in `<state-dir>/uploads/` and its path
 * appears at the shell prompt; the same for a dropped file (and the header upload button).
 */
import assert from 'node:assert/strict';
import fs from 'node:fs';
import path from 'node:path';
import { before, describe, it } from 'node:test';
import * as ui from '../lib/app.js';
import { makePng, pngSize } from '../lib/png.js';
import { TEST, useEnv } from '../lib/suite.js';
import { waitFor } from '../lib/wait.js';

const UPLOAD_NAME = /^img-\d+-[0-9a-f]{8}\.png$/;

describe('image paste and drop', () => {
  const ctx = useEnv('image');

  let sessions = 0;

  /** A page with one focused terminal at its prompt, on a session of its own (a clean prompt line). */
  async function openTerminal() {
    const { env } = ctx;
    sessions += 1;
    const session = `im-${sessions}`;
    env.tmux.newSession(session, { cwd: env.dir(session) });
    const page = await env.freshPage();
    const panel = await ui.addPanel(page, session);
    await ui.waitForPrompt(page, panel.id);
    await ui.clickTerminal(page, panel.id);
    await ui.waitForTerminalFocus(page, panel.id);
    return { page, panel, session };
  }

  /** Drop files on a terminal: drag events that carry a real DataTransfer holding real Files. */
  function dropFiles(page, panelId, files) {
    return page.evaluate(
      ({ id, files }) => {
        const transfer = new DataTransfer();
        for (const file of files) transfer.items.add(new File([new Uint8Array(file.bytes)], file.name, { type: file.type }));
        const host = document.querySelector(`[data-testid="terminal-panel"][data-panel-id="${id}"] .xterm-screen`);
        for (const type of ['dragenter', 'dragover', 'drop']) host.dispatchEvent(new DragEvent(type, { bubbles: true, cancelable: true, dataTransfer: transfer }));
      },
      { id: panelId, files },
    );
  }

  /** Wait until `count` uploads exist; resolves to their absolute paths. */
  async function uploadedPaths(count) {
    const { env } = ctx;
    const names = await waitFor(`${count} file(s) in the uploads directory`, () => (env.uploads().length === count ? env.uploads() : null), { timeout: 10_000 });
    return names.map((name) => path.join(env.uploadsDir, name));
  }

  it('pasting a PNG uploads it to the state dir and pastes its path at the prompt', TEST, async () => {
    const { env } = ctx;
    const { page, panel, session } = await openTerminal();
    const before = env.uploads().length;
    const png = makePng({ width: 16, height: 9, rgb: [30, 120, 220] });

    await ui.writeClipboardPng(page, png);
    await page.keyboard.press('Control+V');

    const [file] = await uploadedPaths(before + 1).then((all) => all.slice(before));
    assert.match(path.basename(file), UPLOAD_NAME);
    assert.equal(path.dirname(file), env.uploadsDir, 'the file is in <state-dir>/uploads');
    // Chromium re-encodes an image it reads from the clipboard, so compare what the file shows.
    assert.deepEqual(pngSize(fs.readFileSync(file)), { width: 16, height: 9 }, 'the stored file is the pasted image');
    assert.equal(fs.statSync(file).mode & 0o777, 0o600, 'uploads are private');
    assert.ok(path.isAbsolute(file));

    // The prompt line now reads "$ <absolute path> " (with a trailing space): the path was
    // pasted, not executed.
    await ui.waitForScreen(page, panel.id, (line) => line === `$ ${file} `, { what: `the prompt to show ${file} followed by a space` });
    const lines = await ui.screenLines(page, panel.id);
    assert.ok(!lines.some((line) => /not found|denied|No such/i.test(line)), `the path was not run: ${JSON.stringify(lines.filter(Boolean))}`);
    await waitFor('the toast', async () => (await ui.toasts(page)).some((text) => /Uploaded 1 file/.test(text)));
    // tmux has the same text on its prompt line (it reached the shell).
    await waitFor('tmux to show the path on the prompt', () => env.tmux.capture(`=${session}:`).split('\n').some((line) => line.trimEnd() === `$ ${file}`));

    // The text is followed by a space, so a second paste lands next to it.
    await ui.writeClipboardPng(page, makePng({ width: 4, height: 4, rgb: [250, 250, 0] }));
    await page.keyboard.press('Control+V');
    const second = (await uploadedPaths(before + 2)).find((candidate) => candidate !== file);
    await ui.waitForScreen(page, panel.id, (line) => line === `$ ${file} ${second} `, { what: `both paths on the prompt line ($ ${file} ${second} )` });
    assert.deepEqual(fs.readdirSync(env.uploadsDir).sort(), env.uploads(), 'no temp files are left in the uploads directory');
    assert.deepEqual(page.__errors, []);
  });

  it('pasting plain text still pastes the text and uploads nothing', TEST, async () => {
    const { env } = ctx;
    const { page, panel, session } = await openTerminal();
    const before = env.uploads().length;
    await ui.writeClipboard(page, 'plain-paste 1234');
    await page.keyboard.press('Control+V');
    await ui.waitForScreen(page, panel.id, (line) => line === '$ plain-paste 1234');
    assert.equal(env.uploads().length, before, 'no upload for text');
    assert.deepEqual(page.__errors, []);
  });

  it('dropping an image file on the terminal uploads it and pastes its path', TEST, async () => {
    const { env } = ctx;
    const { page, panel, session } = await openTerminal();
    const before = env.uploads().length;
    const png = makePng({ width: 12, height: 12, rgb: [10, 200, 90] });

    // An OS file drag cannot be scripted, so the drag events carry a real DataTransfer with a File.
    const drag = (types) =>
      page.evaluate(
        ({ id, base64, types }) => {
          const bytes = Uint8Array.from(atob(base64), (c) => c.charCodeAt(0));
          const transfer = new DataTransfer();
          transfer.items.add(new File([bytes], 'dropped.png', { type: 'image/png' }));
          const host = document.querySelector(`[data-testid="terminal-panel"][data-panel-id="${id}"] .xterm-screen`);
          for (const type of types) host.dispatchEvent(new DragEvent(type, { bubbles: true, cancelable: true, dataTransfer: transfer }));
          return document.querySelector(`[data-testid="terminal-panel"][data-panel-id="${id}"]`).classList.contains('drop-active');
        },
        { id: panel.id, base64: png.toString('base64'), types },
      );
    assert.equal(await drag(['dragenter', 'dragover']), true, 'the terminal shows the drop highlight while a file is over it');
    assert.equal(await drag(['drop']), false, 'the highlight is gone after the drop');

    const [file] = await uploadedPaths(before + 1).then((all) => all.slice(before));
    assert.match(path.basename(file), UPLOAD_NAME);
    assert.ok(png.equals(fs.readFileSync(file)), 'the stored file is byte for byte the dropped PNG');
    await ui.waitForScreen(page, panel.id, (line) => line === `$ ${file} `, { what: `the prompt to show ${file} followed by a space` });
    await waitFor('the toast', async () => (await ui.toasts(page)).some((text) => /Uploaded 1 file/.test(text)));
    assert.deepEqual(page.__errors, []);
  });

  it('dropping a file that is not an image uploads it under its sanitised name and pastes the path', TEST, async () => {
    const { env } = ctx;
    const { page, panel } = await openTerminal();
    const before = env.uploads().length;
    await dropFiles(page, panel.id, [{ name: 'notes v2.txt', type: 'text/plain', bytes: [...Buffer.from('hello file')] }]);
    const file = (await uploadedPaths(before + 1)).find((candidate) => path.basename(candidate).endsWith('-notes_v2.txt'));
    assert.match(path.basename(file ?? ''), /^file-\d+-[0-9a-f]{8}-notes_v2\.txt$/);
    assert.equal(path.dirname(file), env.uploadsDir);
    assert.equal(fs.readFileSync(file, 'utf8'), 'hello file');
    assert.equal(fs.statSync(file).mode & 0o777, 0o600, 'uploads are private');
    await ui.waitForScreen(page, panel.id, (line) => line === `$ ${file} `, { what: `the prompt to show ${file} followed by a space` });
    await waitFor('the toast', async () => (await ui.toasts(page)).some((text) => /Uploaded 1 file/.test(text)));
    assert.deepEqual(page.__errors, []);
  });

  it('dropped JPEG, GIF and WebP files keep their type, and their paths come in the order dropped', TEST, async () => {
    const { env } = ctx;
    const { page, panel } = await openTerminal();
    const before = env.uploads().length;
    // The server decides by magic bytes, so minimal headers are enough.
    const files = [
      { name: 'a.webp', type: 'image/webp', bytes: [0x52, 0x49, 0x46, 0x46, 4, 0, 0, 0, 0x57, 0x45, 0x42, 0x50, 0x56, 0x50, 0x38, 0x20] },
      { name: 'b.jpg', type: 'image/jpeg', bytes: [0xff, 0xd8, 0xff, 0xe0, 0, 16, 0x4a, 0x46, 0x49, 0x46, 0, 1, 1, 0, 0, 1] },
      { name: 'c.gif', type: 'image/gif', bytes: [...Buffer.from('GIF89a'), 1, 0, 1, 0, 0, 0, 0, 0x3b] },
    ];
    await dropFiles(page, panel.id, files);
    await waitFor('three uploads', () => env.uploads().length === before + 3, { timeout: 10_000 });
    const stored = env.uploads().slice(before).map((name) => path.join(env.uploadsDir, name));
    assert.deepEqual(stored.map((file) => path.extname(file)).sort(), ['.gif', '.jpg', '.webp']);
    // The pasted line lists the paths in the order of the dropped files, whatever order they finished in.
    const expected = ['.webp', '.jpg', '.gif'];
    await waitFor(
      'the three paths in the dropped order',
      async () => {
        const line = (await ui.screenLines(page, panel.id)).find((candidate) => candidate.startsWith('$ ') && candidate.includes('/uploads/'));
        if (!line) return false;
        const paths = line.slice(2).trim().split(' ');
        return paths.length === 3 && paths.every((file, index) => file.endsWith(expected[index]));
      },
      { timeout: 10_000 },
    );
    assert.deepEqual(page.__errors, []);
  });

  it('a file that only claims to be an image is stored as a plain file under its name', TEST, async () => {
    const { env } = ctx;
    const { page, panel } = await openTerminal();
    const before = env.uploads().length;
    await dropFiles(page, panel.id, [{ name: 'evil.png', type: 'image/png', bytes: [...Buffer.from('this is not a png, it is a shell script')] }]);
    const file = (await uploadedPaths(before + 1)).find((candidate) => path.basename(candidate).endsWith('-evil.png'));
    assert.match(path.basename(file ?? ''), /^file-\d+-[0-9a-f]{8}-evil\.png$/, 'not an img- upload: the magic bytes decide');
    await ui.waitForScreen(page, panel.id, (line) => line === `$ ${file} `, { what: `the prompt to show ${file}` });
    assert.deepEqual(page.__errors, []);
  });

  it('the context menu offers Upload file… with the same effect', TEST, async () => {
    const { env } = ctx;
    const { page, panel } = await openTerminal();
    const before = env.uploads().length;
    const png = makePng({ width: 7, height: 3, rgb: [9, 9, 99] });
    const box = await ui.terminalPanel(page, panel.id).boundingBox();
    await page.mouse.click(box.x + 200, box.y + 200, { button: 'right' });
    const [chooser] = await Promise.all([page.waitForEvent('filechooser'), page.locator('[data-testid="context-menu-item"][data-action="upload"]').click()]);
    await chooser.setFiles([{ name: 'menu.png', mimeType: 'image/png', buffer: png }]);
    const [file] = (await uploadedPaths(before + 1)).slice(before);
    assert.ok(png.equals(fs.readFileSync(file)), 'a chosen file is stored unchanged');
    await ui.waitForScreen(page, panel.id, (line) => line === `$ ${file} `, { what: `the prompt to show ${file}` });
  });

  it('the header upload button uploads the chosen images and pastes their paths', TEST, async () => {
    const { env } = ctx;
    const { page, panel, session } = await openTerminal();
    const before = env.uploads().length;
    const first = makePng({ width: 5, height: 5, rgb: [1, 2, 3] });
    const second = makePng({ width: 6, height: 6, rgb: [4, 5, 6] });

    const button = await ui.groupControl(page, session, 'header-upload');
    const [chooser] = await Promise.all([page.waitForEvent('filechooser'), button.click()]);
    assert.equal(chooser.isMultiple(), true, 'several images can be picked');
    await chooser.setFiles([
      { name: 'one.png', mimeType: 'image/png', buffer: first },
      { name: 'two.png', mimeType: 'image/png', buffer: second },
    ]);

    const files = (await uploadedPaths(before + 2)).slice(before);
    assert.deepEqual(files.map((file) => fs.readFileSync(file).length).sort(), [first.length, second.length].sort());
    await ui.waitForScreen(page, panel.id, (line) => files.every((file) => line.includes(file)), { what: 'both paths on the prompt line' });
    assert.deepEqual(page.__errors, []);
  });
});

describe('image upload limit', () => {
  const ctx = useEnv('image-limit', { serveArgs: ['--max-upload-mb', '1'] });

  it('an image over the server limit is refused with a message and nothing is pasted', TEST, async () => {
    const { env } = ctx;
    env.tmux.newSession('big', { cwd: env.dir('big') });
    const page = await env.freshPage();
    const panel = await ui.addPanel(page, 'big');
    await ui.waitForPrompt(page, panel.id);
    assert.equal((await env.api('GET', '/api/info')).json.upload_max_bytes, 1024 * 1024);

    // A PNG header followed by 1.5 MiB of anything: over the limit, whatever it contains.
    const bytes = [...makePng().subarray(0, 64), ...new Uint8Array(1.5 * 1024 * 1024)];
    await page.evaluate(
      ({ id, bytes }) => {
        const transfer = new DataTransfer();
        transfer.items.add(new File([new Uint8Array(bytes)], 'big.png', { type: 'image/png' }));
        const host = document.querySelector(`[data-testid="terminal-panel"][data-panel-id="${id}"] .xterm-screen`);
        for (const type of ['dragenter', 'dragover', 'drop']) host.dispatchEvent(new DragEvent(type, { bubbles: true, cancelable: true, dataTransfer: transfer }));
      },
      { id: panel.id, bytes },
    );
    await waitFor('the failure toast', async () => (await ui.toasts(page)).some((text) => /Upload failed.*big\.png/.test(text)));
    assert.deepEqual(env.uploads(), [], 'nothing was stored');
    assert.ok((await ui.screenLines(page, panel.id)).every((line) => !line.includes('/uploads/')));
  });
});
