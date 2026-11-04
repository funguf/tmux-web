/** Shared node:test wiring: one isolated environment per test file. */
import { after, afterEach, before } from 'node:test';
import { createEnv } from './env.js';

/** Options for a single test. */
export const TEST = Object.freeze({ timeout: 90_000 });

/**
 * Create the environment before the first test and dispose of it after the last one (also when
 * a test failed). A failed test prints a screenshot path and the tmux-web log.
 * @param {string} name
 * @param {{serveArgs?: string[]}} [options] extra `serve` options for tmux-web
 * @returns {{env: import('./env.js').Env}} `ctx.env` is set once the tests run
 */
export function useEnv(name, options) {
  const ctx = { env: null };
  before(
    async () => {
      ctx.env = await createEnv(name, options);
    },
    { timeout: 60_000 },
  );
  after(
    async () => {
      await ctx.env?.dispose();
    },
    { timeout: 60_000 },
  );
  afterEach((t) => ctx.env?.reportIfFailed(t));
  return ctx;
}
