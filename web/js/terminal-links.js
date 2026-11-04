/**
 * Link handling for terminals: plain-text URLs (WebLinksAddon) and OSC 8 hyperlinks
 * (Terminal `linkHandler`). A link opens only when Ctrl (Cmd on macOS) is held and the
 * scheme is http or https; hovering shows a "Ctrl+click to open" tooltip.
 */

/** Open an http(s) URL in a new tab without giving it access to this window. */
export function openExternalUrl(uri) {
  let url;
  try {
    url = new URL(uri);
  } catch {
    return false;
  }
  if (url.protocol !== 'http:' && url.protocol !== 'https:') return false;
  window.open(url.href, '_blank', 'noopener,noreferrer');
  return true;
}

function activate(event, uri) {
  if (!(event.ctrlKey || event.metaKey)) return;
  openExternalUrl(uri);
}

/**
 * @param {import('./link-tooltip.js').LinkTooltip} tooltip
 */
export function createLinkHandlers(tooltip) {
  const hover = (event) => tooltip.show(event.clientX, event.clientY);
  const leave = () => tooltip.hide();
  return {
    /** First constructor argument of WebLinksAddon. */
    webLinksHandler: (event, uri) => activate(event, uri),
    /** Second constructor argument of WebLinksAddon. */
    webLinksOptions: { hover, leave },
    /** Value for the Terminal `linkHandler` option (OSC 8). */
    oscLinkHandler: {
      allowNonHttpProtocols: false,
      activate: (event, uri) => activate(event, uri),
      hover,
      leave,
    },
  };
}
