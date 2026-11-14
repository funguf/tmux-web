/**
 * Keep #app the size of the *visual* viewport. When the on-screen keyboard opens, iOS Safari
 * shrinks only the visual viewport and leaves the layout viewport alone, so the bottom of the
 * page (the prompt, the extra-keys bar) would sit behind the keyboard. The style sheet sizes
 * #app from `--app-height` / `--app-top`; this module keeps them current. Without
 * `window.visualViewport` nothing is set and #app fills the window as before.
 */
export function startViewportFit(root = document.documentElement) {
  const viewport = window.visualViewport;
  if (!viewport) return () => {};
  const update = () => {
    root.style.setProperty('--app-height', `${viewport.height}px`);
    root.style.setProperty('--app-top', `${viewport.offsetTop}px`);
  };
  viewport.addEventListener('resize', update);
  viewport.addEventListener('scroll', update);
  update();
  return () => {
    viewport.removeEventListener('resize', update);
    viewport.removeEventListener('scroll', update);
    root.style.removeProperty('--app-height');
    root.style.removeProperty('--app-top');
  };
}
