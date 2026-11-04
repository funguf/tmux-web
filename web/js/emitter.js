/**
 * Tiny named-event hub. Handlers run synchronously; a throwing handler is reported
 * and never prevents the others from running.
 */
export class EventHub {
  #handlers = new Map();

  /** Subscribe to `name`. Returns an unsubscribe function. */
  on(name, handler) {
    let set = this.#handlers.get(name);
    if (!set) {
      set = new Set();
      this.#handlers.set(name, set);
    }
    set.add(handler);
    return () => this.off(name, handler);
  }

  off(name, handler) {
    this.#handlers.get(name)?.delete(handler);
  }

  emit(name, ...args) {
    const set = this.#handlers.get(name);
    if (!set) return;
    for (const handler of [...set]) {
      try {
        handler(...args);
      } catch (err) {
        console.error(`tmux-web: handler for "${name}" failed`, err);
      }
    }
  }
}
