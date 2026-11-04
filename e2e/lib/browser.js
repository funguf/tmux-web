/**
 * Headless Chromium through playwright-core, with the clipboard permissions the copy and
 * paste tests need. One browser per test file; each test gets its own context and page.
 */
import { chromium } from 'playwright-core';

export const CHROME = process.env.TMUX_WEB_CHROME ?? chromium.executablePath();

export const VIEWPORT = Object.freeze({ width: 1280, height: 800 });

/**
 * @param {{tag: string}} options tag: put on the command line (an ignored switch), so the
 *   processes of one test run can be found with `pgrep -f`.
 */
export async function launchBrowser({ tag }) {
  const browser = await chromium.launch({
    executablePath: CHROME,
    headless: true,
    // `tmux.test` resolves to the local server: a plain-http host name that is not a secure context.
    args: [`--tmw-e2e-run=${tag}`, '--host-resolver-rules=MAP tmux.test 127.0.0.1'],
  });
  return browser;
}

/**
 * A fresh context (own storage and clipboard permissions) with one page.
 * Uncaught page errors, console errors and WebSockets are collected on `page.__errors`,
 * `page.__console` and `page.__sockets`.
 */
export async function newContext(browser, { viewport = VIEWPORT } = {}) {
  const context = await browser.newContext({
    viewport: { ...viewport },
    permissions: ['clipboard-read', 'clipboard-write'],
  });
  const page = await context.newPage();
  page.__errors = [];
  page.__console = [];
  page.__sockets = [];
  page.on('websocket', (socket) => {
    const record = { url: socket.url(), closed: false };
    page.__sockets.push(record);
    socket.on('close', () => {
      record.closed = true;
    });
  });
  page.on('pageerror', (err) => page.__errors.push(String(err?.stack ?? err)));
  page.on('console', (msg) => {
    if (msg.type() === 'error' || msg.type() === 'warning') page.__console.push(`${msg.type()}: ${msg.text()}`);
  });
  return { context, page };
}
