/**
 * REST client for the tmux-web backend (CONTRACT §7).
 *
 * Every URL is relative and resolved against the document URL, so the app also works
 * behind a reverse proxy that mounts it on a sub-path. Mutating requests carry the
 * CSRF header `X-Tmux-Web: 1`; error bodies `{"error","message"}` become ApiError.
 */

const MUTATING_METHODS = new Set(['POST', 'PUT', 'PATCH', 'DELETE']);

export class ApiError extends Error {
  /**
   * @param {number} status HTTP status, 0 for a network failure
   * @param {string} code machine readable error code from the server
   * @param {string} message human readable text
   */
  constructor(status, code, message) {
    super(message || code || `HTTP ${status}`);
    this.name = 'ApiError';
    this.status = status;
    this.code = code;
  }
}

export class Api {
  /** @param {string} [baseUrl] defaults to the current document URL */
  constructor(baseUrl = document.baseURI) {
    this.baseUrl = baseUrl;
  }

  /** Absolute URL for a path that is relative to the app root (e.g. "api/status"). */
  url(path) {
    return new URL(path, this.baseUrl).href;
  }

  /** ws:/wss: URL for a relative path, protocol chosen from the page protocol. */
  wsUrl(path, params = {}) {
    const url = new URL(path, this.baseUrl);
    url.protocol = url.protocol === 'https:' ? 'wss:' : 'ws:';
    for (const [key, value] of Object.entries(params)) url.searchParams.set(key, String(value));
    return url.href;
  }

  /**
   * @param {string} method
   * @param {string} path relative path, e.g. "api/layout"
   * @param {{json?: any, body?: BodyInit, headers?: Record<string, string>, keepalive?: boolean}} [options]
   */
  async request(method, path, { json, body, headers = {}, keepalive = false } = {}) {
    /** @type {RequestInit} */
    const init = {
      method,
      credentials: 'same-origin',
      cache: 'no-store',
      headers: { Accept: 'application/json', ...headers },
    };
    if (MUTATING_METHODS.has(method)) init.headers['X-Tmux-Web'] = '1';
    if (json !== undefined) {
      init.headers['Content-Type'] = 'application/json';
      init.body = JSON.stringify(json);
    } else if (body !== undefined) {
      init.body = body;
    }
    if (keepalive) init.keepalive = true;

    let response;
    try {
      response = await fetch(this.url(path), init);
    } catch {
      throw new ApiError(0, 'network', 'Could not reach the tmux-web server');
    }

    let data = null;
    const text = await response.text().catch(() => '');
    if (text) {
      try {
        data = JSON.parse(text);
      } catch {
        data = null;
      }
    }
    if (!response.ok) {
      throw new ApiError(
        response.status,
        data?.error ?? `http_${response.status}`,
        data?.message ?? (response.statusText || `HTTP ${response.status}`),
      );
    }
    return data;
  }

  info() {
    return this.request('GET', 'api/info');
  }

  status() {
    return this.request('GET', 'api/status');
  }

  createSession(name, cwd) {
    return this.request('POST', 'api/sessions', { json: cwd ? { name, cwd } : { name } });
  }

  renameSession(name, newName) {
    return this.request('POST', 'api/sessions/rename', { json: { name, new_name: newName } });
  }

  /** Terminate a tmux session (POST api/sessions/kill). Resolves to {killed}. */
  killSession(name) {
    return this.request('POST', 'api/sessions/kill', { json: { name } });
  }

  /**
   * Stop the Claude Code processes of a session but remember their conversations
   * (POST api/sessions/snooze). Resolves to {snoozed, freed_bytes}.
   */
  snoozeSession(name) {
    return this.request('POST', 'api/sessions/snooze', { json: { name } });
  }

  /** Reopen the snoozed conversations with --resume (POST api/sessions/wake). Resolves to {woken}. */
  wakeSession(name) {
    return this.request('POST', 'api/sessions/wake', { json: { name } });
  }

  /**
   * Scroll the tmux history of a session (POST api/sessions/scroll): `lines` < 0 goes back
   * to earlier output, > 0 forward. Resolves to {in_mode, scroll_position}.
   */
  scrollSession(name, lines) {
    return this.request('POST', 'api/sessions/scroll', { json: { name, lines } });
  }

  getLayout() {
    return this.request('GET', 'api/layout');
  }

  putLayout(layout, clientId, { keepalive = false } = {}) {
    return this.request('PUT', 'api/layout', { json: { layout, client_id: clientId }, keepalive });
  }

  /** Upload one file; resolves to {path, bytes, mime}. */
  upload(file) {
    const query = file.name ? `?name=${encodeURIComponent(file.name)}` : '';
    return this.request('POST', `api/upload${query}`, {
      body: file,
      headers: { 'Content-Type': file.type || 'application/octet-stream' },
    });
  }

  /**
   * Start tmux when it is not running (POST /api/server/start): the default session is
   * created. Resolves to {started: false} when tmux was running already, else to
   * {started: true, session}.
   */
  startServer() {
    return this.request('POST', 'api/server/start');
  }
}

