/**
 * window.__tmuxWeb: a small, stable handle for end-to-end tests (CONTRACT §8.6 item 16).
 *
 *   ready            Promise that resolves once the saved layout has been restored
 *   api              the dockview API
 *   panels()         [{id, session, groupId, connected, maximized, attention}]
 *   screenText(id)   visible terminal rows joined by "\n" (trailing spaces trimmed)
 *   terminal(id)     the xterm.js Terminal of a panel (null until it has been opened)
 */
import { Workspace } from './workspace.js';

/**
 * @param {{ready: Promise<void>, workspace: Workspace, registry: Map<string, any>}} options
 */
export function installTestHook({ ready, workspace, registry }) {
  window.__tmuxWeb = {
    ready,
    api: workspace.api,
    panels: () =>
      workspace.api.panels.map((panel) => ({
        id: panel.id,
        session: Workspace.sessionOf(panel),
        groupId: panel.group.id,
        connected: registry.get(panel.id)?.connected ?? false,
        maximized: panel.group.api.isMaximized(),
        attention: registry.get(panel.id)?.attention ?? false,
      })),
    screenText: (panelId) => registry.get(panelId)?.screenText() ?? '',
    terminal: (panelId) => registry.get(panelId)?.term ?? null,
  };
}
