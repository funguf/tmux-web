/**
 * Inline SVG icons, built with the DOM API from static path data (no innerHTML).
 * All icons share a 16x16 viewBox and are drawn with currentColor strokes.
 */
const SVG_NS = 'http://www.w3.org/2000/svg';

/** Each icon is a list of [tag, attributes] primitives. */
const ICONS = {
  plus: [['path', { d: 'M8 3v10M3 8h10' }]],
  close: [['path', { d: 'M4 4l8 8M12 4l-8 8' }]],
  check: [['path', { d: 'M3 8.5l3.2 3.2L13 4.8' }]],
  minus: [['path', { d: 'M3 8h10' }]],
  settings: [
    ['path', { d: 'M2 4.5h5M11.5 4.5H14M2 8h1.5M7.5 8H14M2 11.5h6M12 11.5h2' }],
    ['circle', { cx: 9.2, cy: 4.5, r: 1.7 }],
    ['circle', { cx: 5.5, cy: 8, r: 1.7 }],
    ['circle', { cx: 10, cy: 11.5, r: 1.7 }],
  ],
  fullscreen: [['path', { d: 'M2 6V2h4M10 2h4v4M14 10v4h-4M6 14H2v-4' }]],
  'fullscreen-exit': [['path', { d: 'M6 2v4H2M14 6h-4V2M10 14v-4h4M2 10h4v4' }]],
  'split-right': [
    ['rect', { x: 2, y: 3, width: 12, height: 10, rx: 1 }],
    ['path', { d: 'M8 3v10' }],
  ],
  'split-down': [
    ['rect', { x: 2, y: 3, width: 12, height: 10, rx: 1 }],
    ['path', { d: 'M2 8h12' }],
  ],
  upload: [['path', { d: 'M8 10.5V3M5 6l3-3 3 3M3 13h10' }]],
  maximize: [['rect', { x: 3, y: 3, width: 10, height: 10, rx: 1 }]],
  restore: [
    ['rect', { x: 5.5, y: 2.5, width: 8, height: 8, rx: 1 }],
    ['path', { d: 'M3 5.5v7.5a0.5 0.5 0 0 0 0.5 0.5H11' }],
  ],
  claude: [['path', { d: 'M8 2v12M2.8 5l10.4 6M2.8 11l10.4-6' }]],
  terminal: [['path', { d: 'M2.5 4l4 4-4 4M8 12.5h5.5' }]],
  refresh: [['path', { d: 'M13 8a5 5 0 1 1-1.6-3.7M13 2.5v3h-3' }]],
  copy: [
    ['rect', { x: 5.5, y: 5.5, width: 8, height: 8, rx: 1 }],
    ['path', { d: 'M3 10.5v-7a0.5 0.5 0 0 1 0.5-0.5h7' }],
  ],
  paste: [
    ['rect', { x: 3, y: 3, width: 10, height: 11, rx: 1 }],
    ['path', { d: 'M6 3V2h4v1M6 7h4M6 10h4' }],
  ],
  moon: [['path', { d: 'M13 9.6A5.5 5.5 0 0 1 6.4 3a5.5 5.5 0 1 0 6.6 6.6z' }]],
  sun: [
    ['circle', { cx: 8, cy: 8, r: 2.8 }],
    ['path', { d: 'M8 1.5v1.7M8 12.8v1.7M1.5 8h1.7M12.8 8h1.7M3.4 3.4l1.2 1.2M11.4 11.4l1.2 1.2M12.6 3.4l-1.2 1.2M4.6 11.4l-1.2 1.2' }],
  ],
  pencil: [['path', { d: 'M3 13l0.8-3L10.7 3.1a1 1 0 0 1 1.4 0l0.8 0.8a1 1 0 0 1 0 1.4L6 12.2z' }]],
};

/**
 * Build an <svg> element for the named icon.
 * @param {string} name
 * @param {{size?: number, className?: string}} [options]
 */
export function svgIcon(name, { size = 16, className = '' } = {}) {
  const spec = ICONS[name];
  const svg = document.createElementNS(SVG_NS, 'svg');
  svg.setAttribute('viewBox', '0 0 16 16');
  svg.setAttribute('width', String(size));
  svg.setAttribute('height', String(size));
  svg.setAttribute('fill', 'none');
  svg.setAttribute('stroke', 'currentColor');
  svg.setAttribute('stroke-width', '1.5');
  svg.setAttribute('stroke-linecap', 'round');
  svg.setAttribute('stroke-linejoin', 'round');
  svg.setAttribute('aria-hidden', 'true');
  svg.setAttribute('focusable', 'false');
  svg.setAttribute('class', `icon icon-${name}${className ? ` ${className}` : ''}`);
  if (!spec) return svg;
  for (const [tag, attrs] of spec) {
    const node = document.createElementNS(SVG_NS, tag);
    for (const [key, value] of Object.entries(attrs)) node.setAttribute(key, String(value));
    svg.appendChild(node);
  }
  return svg;
}
