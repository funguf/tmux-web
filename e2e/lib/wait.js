/** Polling helpers: every wait has a deadline and says what it was waiting for. */

export const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

/** Blocking sleep for the synchronous exit handlers. */
export function sleepSync(ms) {
  Atomics.wait(new Int32Array(new SharedArrayBuffer(4)), 0, 0, ms);
}

function describeValue(value) {
  let text;
  try {
    text = typeof value === 'string' ? value : JSON.stringify(value);
  } catch {
    text = String(value);
  }
  if (text === undefined) text = String(value);
  return text.length > 1500 ? `${text.slice(0, 1500)}…` : text;
}

/**
 * Call `produce` until `accept(value)` is true; resolves to that value. On timeout the error
 * names `what` and shows the last value seen, so a failing wait explains itself.
 *
 * @template T
 * @param {string} what
 * @param {() => T | Promise<T>} produce
 * @param {(value: T) => boolean} accept
 * @param {{timeout?: number, interval?: number}} [options]
 * @returns {Promise<T>}
 */
export async function waitUntil(what, produce, accept, { timeout = 10_000, interval = 50 } = {}) {
  const deadline = Date.now() + timeout;
  let last;
  let lastError = null;
  for (;;) {
    try {
      last = await produce();
      lastError = null;
      if (accept(last)) return last;
    } catch (err) {
      lastError = err;
    }
    if (Date.now() >= deadline) {
      const seen = lastError ? `last error: ${lastError.message}` : `last value: ${describeValue(last)}`;
      throw new Error(`timed out after ${timeout} ms waiting for ${what}; ${seen}`);
    }
    await sleep(interval);
  }
}

/** Poll until `check` returns something truthy; resolves to it. */
export function waitFor(what, check, options) {
  return waitUntil(what, check, Boolean, options);
}

/** Assert that `check` stays falsy for `duration` ms (used for "nothing happens" claims). */
export async function staysFalse(what, check, { duration = 800, interval = 50 } = {}) {
  const deadline = Date.now() + duration;
  for (;;) {
    const value = await check();
    if (value) throw new Error(`${what} (value: ${describeValue(value)})`);
    if (Date.now() >= deadline) return;
    await sleep(interval);
  }
}
