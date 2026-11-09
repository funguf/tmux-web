/**
 * Multi-row tabs: when a group's header is too narrow for all of its tabs, they wrap onto more
 * rows instead of hiding behind dockview's overflow dropdown.
 *
 * dockview-core already ships everything this needs except the switch: its CSS for
 * `.dv-tabs-container--wrap` wraps the tab strip and lets the header grow, the group re-measures
 * the taller header and shrinks its content to match, and tab drag and drop honours the class.
 * Only the module that sets the class (MultiRowTabs) is in the paid dockview-enterprise, so this
 * sets it on every group's tab strip itself.
 */
import { OVERFLOW_WRAP_TABS_CLASS } from '../vendor/dockview-core/dockview-core.esm.min.mjs';

/** @param {any} group a dockview group */
function wrapTabs(group) {
  group.element.querySelector('.dv-tabs-container')?.classList.add(OVERFLOW_WRAP_TABS_CLASS);
}

/**
 * Wrap the tabs of every group, now and as groups are added.
 * @param {any} api the dockview api
 * @returns {{dispose: () => void}}
 */
export function wrapAllTabs(api) {
  const wrapAll = () => api.groups.forEach(wrapTabs);
  const disposables = [
    api.onDidAddGroup(wrapTabs),
    // Also catches groups a restored layout creates.
    api.onDidLayoutChange(wrapAll),
  ];
  wrapAll();
  return {
    dispose: () => {
      for (const disposable of disposables.splice(0)) disposable.dispose();
    },
  };
}
