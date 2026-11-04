/**
 * Page helpers for the tmux-web UI. They talk to the app through `window.__tmuxWeb` and the
 * `data-testid`s of CONTRACT §8.6, and drive it with real mouse and keyboard input.
 */
import { sleep, waitFor, waitUntil } from './wait.js';

/** A CSS string literal for an attribute selector. */
const css = (value) => `"${String(value).replaceAll('\\', '\\\\').replaceAll('"', '\\"')}"`;

// ---- locators ---------------------------------------------------------------------------

export const tabFor = (page, session) => page.locator(`[data-testid="tab"][data-session=${css(session)}]`);
export const pickerItem = (page, session) => page.locator(`[data-testid="session-picker-item"][data-session=${css(session)}]`);
export const terminalPanel = (page, panelId) => page.locator(`[data-testid="terminal-panel"][data-panel-id=${css(panelId)}]`);

/**
 * The control `testid` (e.g. "header-split-right") in the header of the group that holds the
 * panel of `session`. The element is marked with a throw-away attribute to get a locator.
 */
export async function groupControl(page, session, testid) {
  const marker = `m${Math.random().toString(36).slice(2)}`;
  const found = await page.evaluate(
    ({ session, testid, marker }) => {
      const panel = window.__tmuxWeb.api.panels.find((p) => (p.params?.session ?? p.title) === session);
      const node = panel?.group.element.querySelector(`[data-testid="${testid}"]`);
      if (!node) return false;
      node.setAttribute('data-e2e-marker', marker);
      return true;
    },
    { session, testid, marker },
  );
  if (!found) throw new Error(`no ${testid} control for the group of session ${session}`);
  return page.locator(`[data-e2e-marker="${marker}"]`);
}

// ---- state ----------------------------------------------------------------------------------

export const panels = (page) => page.evaluate(() => window.__tmuxWeb.panels());

export async function panelOf(page, session) {
  return (await panels(page)).find((panel) => panel.session === session) ?? null;
}

/** Wait until a panel for `session` exists (and is connected, by default). Resolves to it. */
export function waitForPanel(page, session, { connected = true, timeout = 15_000 } = {}) {
  return waitUntil(
    `a panel for session ${JSON.stringify(session)}${connected ? ' that is connected' : ''}`,
    () => panelOf(page, session),
    (panel) => Boolean(panel) && (!connected || panel.connected),
    { timeout },
  );
}

/** Wait until every panel is connected. */
export function waitAllConnected(page, { timeout = 20_000 } = {}) {
  return waitUntil(
    'every panel connected',
    () => panels(page),
    (all) => all.length > 0 && all.every((panel) => panel.connected),
    { timeout },
  );
}

/** Every dockview group: id, panels, active panel, maximised flag and its rectangle. */
export function groups(page) {
  return page.evaluate(() =>
    window.__tmuxWeb.api.groups.map((group) => {
      const rect = group.element.getBoundingClientRect();
      return {
        id: group.id,
        panelIds: group.panels.map((panel) => panel.id),
        sessions: group.panels.map((panel) => panel.params?.session ?? panel.title),
        activePanelId: group.activePanel?.id ?? null,
        maximized: group.api.isMaximized(),
        rect: { x: rect.x, y: rect.y, width: rect.width, height: rect.height },
        width: group.api.width,
        height: group.api.height,
      };
    }),
  );
}

export async function groupOf(page, session) {
  const all = await groups(page);
  return all.find((group) => group.sessions.includes(session)) ?? null;
}

/** Titles of the tabs in the DOM, left to right, per group (reading the DOM, not the model). */
export function tabTitles(page) {
  return page.getByTestId('tab').evaluateAll((tabs) => tabs.map((tab) => tab.textContent));
}

/** Rectangle of the work area (the element dockview fills). */
export async function workArea(page) {
  return page.locator('#workspace').boundingBox();
}

/** Reload the page and wait for the app (and its saved layout) to be ready again. */
export async function reloadApp(page) {
  await page.reload({ waitUntil: 'load' });
  await page.waitForFunction(() => window.__tmuxWeb !== undefined, undefined, { timeout: 15_000 });
  await page.evaluate(() => window.__tmuxWeb.ready);
}

// ---- opening and moving panels ----------------------------------------------------------

/** Add a panel from the top bar's "Add panel" picker. */
export async function addPanel(page, session, options) {
  await page.getByTestId('add-panel-btn').click();
  await pickerItem(page, session).click();
  return waitForPanel(page, session, options);
}

/** Split from the header of the group holding `fromSession`, placing `session` in the new group. */
export async function splitFrom(page, fromSession, direction, session, options) {
  const testid = direction === 'right' ? 'header-split-right' : 'header-split-down';
  await (await groupControl(page, fromSession, testid)).click();
  await pickerItem(page, session).click();
  return waitForPanel(page, session, options);
}

/** Drag from one point to another with the real mouse (HTML5 drag and drop). */
export async function dragMouse(page, from, to, { steps = 24, settle = 250 } = {}) {
  await page.mouse.move(from.x, from.y);
  await page.mouse.down();
  await page.mouse.move(from.x + 8, from.y + 8, { steps: 3 });
  await page.mouse.move(to.x, to.y, { steps });
  await sleep(settle);
  await page.mouse.up();
}

export const centreOf = (box) => ({ x: box.x + box.width / 2, y: box.y + box.height / 2 });

/** Drag the tab of `session` to a point inside the rectangle `box` (fractions of its size). */
export async function dragTabTo(page, session, box, { fx = 0.5, fy = 0.5, inset } = {}) {
  const tab = await tabFor(page, session).boundingBox();
  if (!tab) throw new Error(`no visible tab for ${session}`);
  const to = {
    x: inset?.x !== undefined ? (inset.x >= 0 ? box.x + inset.x : box.x + box.width + inset.x) : box.x + box.width * fx,
    y: inset?.y !== undefined ? (inset.y >= 0 ? box.y + inset.y : box.y + box.height + inset.y) : box.y + box.height * fy,
  };
  await dragMouse(page, centreOf(tab), to);
}

// ---- terminals ----------------------------------------------------------------------------

/** The visible rows of a terminal, as the app's `screenText` reports them. */
export const screenText = (page, panelId) => page.evaluate((id) => window.__tmuxWeb.screenText(id), panelId);

/** The visible rows with soft-wrapped rows joined, so a long path is one line. */
export function screenLines(page, panelId) {
  return page.evaluate((id) => {
    const term = window.__tmuxWeb.terminal(id);
    if (!term) return [];
    const buffer = term.buffer.active;
    const lines = [];
    for (let y = 0; y < term.rows; y += 1) {
      const line = buffer.getLine(buffer.viewportY + y);
      if (!line) continue;
      const text = line.translateToString(true);
      if (line.isWrapped && lines.length > 0) lines[lines.length - 1] += text;
      else lines.push(text);
    }
    return lines;
  }, panelId);
}

/** Wait until some screen line satisfies `accept` (a string means "contains"). Resolves to the lines. */
export function waitForScreen(page, panelId, accept, { timeout = 15_000, what } = {}) {
  const test = typeof accept === 'string' ? (line) => line.includes(accept) : accept;
  return waitUntil(
    what ?? `the terminal screen to show ${typeof accept === 'string' ? JSON.stringify(accept) : 'the expected line'}`,
    () => screenLines(page, panelId),
    (lines) => lines.some(test),
    { timeout },
  );
}

/** Wait until the shell has drawn its prompt (a line starting with "$"). */
export function waitForPrompt(page, panelId, options) {
  return waitForScreen(page, panelId, (line) => line.startsWith('$'), { what: 'a shell prompt', ...options });
}

/** Click into a terminal (real mouse) so it has the keyboard focus. */
export async function clickTerminal(page, panelId) {
  const box = await terminalPanel(page, panelId).boundingBox();
  if (!box) throw new Error(`panel ${panelId} is not visible`);
  await page.mouse.click(box.x + box.width / 2, box.y + box.height - 60);
}

/** Wait until the terminal of `panelId` holds the keyboard focus (so typed keys reach it). */
export function waitForTerminalFocus(page, panelId, { timeout = 5000 } = {}) {
  return waitFor(
    `the terminal of ${panelId} to have the keyboard focus`,
    () =>
      page.evaluate((id) => {
        const term = window.__tmuxWeb.terminal(id);
        return Boolean(term?.textarea) && document.activeElement === term.textarea;
      }, panelId),
    { timeout },
  );
}

/** Type a line with the real keyboard and press Enter. */
export async function typeLine(page, text) {
  await page.keyboard.type(text);
  await page.keyboard.press('Enter');
}

/** The terminal's size in cells. */
export function termSize(page, panelId) {
  return page.evaluate((id) => {
    const term = window.__tmuxWeb.terminal(id);
    return term ? { cols: term.cols, rows: term.rows } : null;
  }, panelId);
}

/**
 * Page coordinates of `needle` on the visible screen (the last row that contains it):
 * `{x0, x1, y}` span the text horizontally at its row's vertical centre; `cw`, `ch` are the cell size.
 */
export async function locateText(page, panelId, needle) {
  const info = await page.evaluate(
    ({ id, needle }) => {
      const term = window.__tmuxWeb.terminal(id);
      if (!term) return null;
      const buffer = term.buffer.active;
      let found = null;
      for (let y = 0; y < term.rows; y += 1) {
        const line = buffer.getLine(buffer.viewportY + y);
        const index = line ? line.translateToString(true).lastIndexOf(needle) : -1;
        if (index >= 0) found = { row: y, col: index };
      }
      if (!found) return null;
      const screen = term.element.querySelector('.xterm-screen').getBoundingClientRect();
      return { ...found, cols: term.cols, rows: term.rows, left: screen.left, top: screen.top, width: screen.width, height: screen.height };
    },
    { id: panelId, needle },
  );
  if (!info) throw new Error(`${JSON.stringify(needle)} is not on the screen of panel ${panelId}`);
  const cw = info.width / info.cols;
  const ch = info.height / info.rows;
  return {
    x0: info.left + info.col * cw,
    x1: info.left + (info.col + needle.length) * cw,
    y: info.top + (info.row + 0.5) * ch,
    row: info.row,
    col: info.col,
    cw,
    ch,
  };
}

/** Select `needle` on screen by dragging the real mouse across it (optionally holding Shift). */
export async function dragSelect(page, panelId, needle, { shift = false } = {}) {
  const pos = await locateText(page, panelId, needle);
  if (shift) await page.keyboard.down('Shift');
  try {
    await page.mouse.move(pos.x0 + 2, pos.y);
    await page.mouse.down();
    await page.mouse.move(pos.x1 - 2, pos.y, { steps: 8 });
    await page.mouse.up();
  } finally {
    if (shift) await page.keyboard.up('Shift');
  }
  return pos;
}

/** Select the word under the middle of `needle` with a real double click. */
export async function doubleClickWord(page, panelId, needle) {
  const pos = await locateText(page, panelId, needle);
  await page.mouse.dblclick((pos.x0 + pos.x1) / 2, pos.y);
  return pos;
}

/** xterm's current selection text. */
export const selectionText = (page, panelId) =>
  page.evaluate((id) => window.__tmuxWeb.terminal(id)?.getSelection() ?? '', panelId);

/** Mouse tracking mode of the terminal ("none", "x10", "vt200", "drag", "any"). */
export const mouseMode = (page, panelId) =>
  page.evaluate((id) => window.__tmuxWeb.terminal(id)?.modes.mouseTrackingMode ?? null, panelId);

// ---- clipboard -------------------------------------------------------------------------------

export const readClipboard = (page) => page.evaluate(() => navigator.clipboard.readText());

export const writeClipboard = (page, text) => page.evaluate((value) => navigator.clipboard.writeText(value), text);

/** Put a PNG (Buffer) on the clipboard as an image, like "copy image" in a browser. */
export function writeClipboardPng(page, png) {
  return page.evaluate(async (base64) => {
    const bytes = Uint8Array.from(atob(base64), (c) => c.charCodeAt(0));
    const blob = new Blob([bytes], { type: 'image/png' });
    await navigator.clipboard.write([new ClipboardItem({ 'image/png': blob })]);
  }, png.toString('base64'));
}

// ---- window.open ---------------------------------------------------------------------------------

/** Replace window.open with a recorder (the page then never opens a real tab). */
export function spyOnWindowOpen(page) {
  return page.evaluate(() => {
    window.__opened = [];
    window.open = (...args) => {
      window.__opened.push(args);
      return null;
    };
  });
}

/** Calls recorded by spyOnWindowOpen: `[[url, target, features], …]`. */
export const openedWindows = (page) => page.evaluate(() => window.__opened ?? []);

// ---- misc -------------------------------------------------------------------------------------------

/** Texts of the toasts currently on screen. */
export const toasts = (page) => page.getByTestId('toast').allTextContents();

/** `/ws/attach` sockets the page has opened so far: [{url, closed}]. */
export const attachSockets = (page) => (page.__sockets ?? []).filter((socket) => socket.url.includes('/ws/attach'));

export { sleep, waitFor, waitUntil };
