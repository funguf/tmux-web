/**
 * Image upload: paste, drop and file-chooser all end up in uploadImages(), which
 * POSTs every image to /api/upload and resolves to the absolute server paths.
 * The caller then pastes those paths into the terminal.
 */
import { toast } from './toast.js';
import { errorMessage, plural } from './dom.js';

const IMAGE_NAME = /\.(png|jpe?g|gif|webp)$/i;

/** True for files the backend can accept (by declared type or file extension). */
export function isImageFile(file) {
  return (file.type ?? '').startsWith('image/') || IMAGE_NAME.test(file.name ?? '');
}

/**
 * Image files found in clipboardData.items (kind === 'file', type image/*).
 * Must be called synchronously inside the paste event handler.
 * @param {DataTransfer|null} clipboardData
 * @returns {File[]}
 */
export function imagesFromClipboard(clipboardData) {
  const files = [];
  for (const item of Array.from(clipboardData?.items ?? [])) {
    if (item.kind !== 'file' || !(item.type ?? '').startsWith('image/')) continue;
    const file = item.getAsFile();
    if (file) files.push(file);
  }
  return files;
}

/** Whether a drag carries OS files (as opposed to a dockview tab / text drag). */
export function dragHasFiles(dataTransfer) {
  return Array.from(dataTransfer?.types ?? []).includes('Files');
}

/**
 * Upload images in parallel, keeping the input order for the result.
 * Shows a toast for the outcome.
 * @param {import('./api.js').Api} api
 * @param {File[]} files
 * @returns {Promise<string[]>} absolute paths of the uploaded images
 */
export async function uploadImages(api, files) {
  const images = files.filter(isImageFile);
  const rejected = files.length - images.length;
  if (rejected > 0) {
    toast.warn(
      images.length === 0
        ? 'Only image files (PNG, JPEG, GIF, WebP) can be uploaded'
        : `${plural(rejected, 'file')} skipped: only images can be uploaded`,
    );
  }
  if (images.length === 0) return [];

  const results = await Promise.allSettled(images.map((file) => api.upload(file)));
  const paths = [];
  const failures = [];
  results.forEach((result, index) => {
    if (result.status === 'fulfilled' && result.value?.path) paths.push(result.value.path);
    else failures.push(`${images[index].name || 'image'}: ${errorMessage(result.reason ?? 'no path returned')}`);
  });
  if (paths.length > 0) toast.success(`Uploaded ${plural(paths.length, 'image')}`);
  for (const failure of failures) toast.error(`Upload failed – ${failure}`);
  return paths;
}

let pendingResolve = null;
let input = null;

/**
 * Open the browser's file chooser for images (must run inside a user gesture).
 * Resolves to the chosen files, or [] when the dialog is cancelled.
 * @returns {Promise<File[]>}
 */
export function pickImages() {
  if (pendingResolve) {
    pendingResolve([]);
    pendingResolve = null;
  }
  if (!input) {
    input = document.createElement('input');
    input.type = 'file';
    input.accept = 'image/*';
    input.multiple = true;
    input.hidden = true;
    input.setAttribute('data-testid', 'upload-input');
    document.body.append(input);
    const finish = () => {
      const files = Array.from(input.files ?? []);
      input.value = '';
      const resolve = pendingResolve;
      pendingResolve = null;
      resolve?.(files);
    };
    input.addEventListener('change', finish);
    input.addEventListener('cancel', finish);
  }
  return new Promise((resolve) => {
    pendingResolve = resolve;
    input.value = '';
    input.click();
  });
}
