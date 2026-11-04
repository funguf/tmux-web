/**
 * The events client (web/js/events-client.js, CONTRACT §7.1) must ignore whatever a socket
 * delivers once that socket is no longer the current socket of a running client: a late
 * `open` must not flip the connection state, a late frame must not reach the listeners, and a
 * late `close` must not drop the socket that replaced it or schedule a reconnect.
 *
 * A real browser never calls a handler that stop() has cleared, so the page imports the very
 * module the app runs and drives a client with a fake WebSocket that remembers the callbacks
 * it was given and can replay them late. The fake is installed only for one synchronous run,
 * so the app's own sockets are never touched.
 */
import assert from 'node:assert/strict';
import { describe, it } from 'node:test';
import { TEST, useEnv } from '../lib/suite.js';
import { waitFor } from '../lib/wait.js';

/**
 * Runs in the page (it must not use anything from this file). Resolves to what the client
 * looked like after each step, as plain data.
 */
async function runScenarios() {
  const { EventsClient } = await import('/js/events-client.js');

  class FakeWebSocket {
    static instances = [];

    constructor(url) {
      this.url = url;
      this.closeCalls = 0;
      this.kept = {};
      FakeWebSocket.instances.push(this);
      for (const name of ['onopen', 'onmessage', 'onclose', 'onerror']) {
        let current = null;
        Object.defineProperty(this, name, {
          get: () => current,
          set: (callback) => {
            current = callback;
            if (callback) this.kept[name] = callback; // still there after stop() cleared the property
          },
        });
      }
    }

    close() {
      this.closeCalls += 1;
    }

    /** Call the callback the client registered, as a late event would. */
    late(name, event = {}) {
      this.kept[name](event);
    }
  }

  const frame = (type) => ({ data: JSON.stringify({ type, running: true }) });
  /** Everything a socket can still deliver, in the order a real one would. */
  const replayEverything = (socket) => {
    socket.late('onopen');
    for (const type of ['status', 'layout']) socket.late('onmessage', frame(type));
    socket.late('onerror');
    socket.late('onclose');
  };

  const steps = {};
  const RealWebSocket = window.WebSocket;
  let client = null;
  try {
    window.WebSocket = FakeWebSocket;
    client = new EventsClient({ wsUrl: (path) => `ws://events.invalid/${path}` });
    const seen = [];
    client.on('connection', ({ connected }) => seen.push(`connection:${connected}`));
    for (const type of ['status', 'layout']) client.on(type, () => seen.push(type));
    const look = () => ({
      seen: [...seen],
      connected: client.connected,
      attempt: client.attempt,
      reconnectScheduled: client.timer !== null,
      sockets: FakeWebSocket.instances.length,
      current: FakeWebSocket.instances.indexOf(client.socket),
    });

    // 1. A socket that was stopped: it must not revive the client.
    client.start();
    const stopped = FakeWebSocket.instances[0];
    client.stop();
    replayEverything(stopped);
    steps.lateEventsOfAStoppedSocket = look();

    // 2. The same old socket once a newer one is current: it must not touch the newer one.
    client.attempt = 3; // an `open` that is not ignored would reset this to 0
    client.start();
    const current = FakeWebSocket.instances[1];
    replayEverything(stopped);
    steps.lateEventsOfAReplacedSocket = look();

    // 3. The current socket still works (the checks above are not just a dead client).
    current.late('onopen');
    current.late('onmessage', frame('status'));
    steps.currentSocketOpened = look();
    current.late('onclose');
    steps.currentSocketClosed = look();
    client.stop();

    // 4. A client that is stopped ignores even the socket it still holds. (stop() also lets go
    // of the socket, so the flag is set by hand to check it on its own.)
    client.attempt = 0;
    seen.length = 0;
    client.start();
    const held = FakeWebSocket.instances[2];
    client.stopped = true;
    replayEverything(held);
    steps.lateEventsWhileStopped = look();
  } finally {
    client?.stop();
    window.WebSocket = RealWebSocket;
  }
  return steps;
}

describe('events client', () => {
  const ctx = useEnv('events-client');

  it('ignores everything a stopped or replaced socket still delivers', TEST, async () => {
    const { env } = ctx;
    const page = await env.freshPage();
    await waitFor('the events socket', () => page.__sockets.some((socket) => socket.url.includes('/ws/events')));
    const socketsBefore = page.__sockets.map((socket) => socket.url);

    const steps = await page.evaluate(runScenarios);

    assert.deepEqual(
      steps.lateEventsOfAStoppedSocket,
      { seen: [], connected: false, attempt: 0, reconnectScheduled: false, sockets: 1, current: -1 },
      'a stopped socket changed the client',
    );
    assert.deepEqual(
      steps.lateEventsOfAReplacedSocket,
      { seen: [], connected: false, attempt: 3, reconnectScheduled: false, sockets: 2, current: 1 },
      'a replaced socket changed the client',
    );
    assert.deepEqual(
      steps.currentSocketOpened,
      { seen: ['connection:true', 'status'], connected: true, attempt: 0, reconnectScheduled: false, sockets: 2, current: 1 },
      'the current socket must still open the client and deliver frames',
    );
    assert.deepEqual(
      steps.currentSocketClosed,
      { seen: ['connection:true', 'status', 'connection:false'], connected: false, attempt: 1, reconnectScheduled: true, sockets: 2, current: -1 },
      'the current socket must still close the client and schedule a reconnect',
    );
    assert.deepEqual(
      steps.lateEventsWhileStopped,
      { seen: [], connected: false, attempt: 0, reconnectScheduled: false, sockets: 3, current: 2 },
      'a client marked stopped must ignore its socket (which it still holds)',
    );

    // The fake never reached the app: its own sockets are the ones it had, and nothing failed.
    assert.deepEqual(
      page.__sockets.map((socket) => socket.url),
      socketsBefore,
    );
    assert.ok(page.__sockets.every((socket) => !socket.closed));
    assert.deepEqual(page.__errors, []);
  });
});
