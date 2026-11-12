/**
 * File upload: paste, drop and file-chooser all end up in uploadFiles(), which
 * POSTs every file to /api/upload and resolves to the absolute server paths.
 * The caller then pastes those paths into the terminal.
 */
import { toast } from './toast.js';
import { errorMessage, plural } from './dom.js';

/**
 * Files found in clipboardData.items (every item with kind === 'file', any type).
 * Must be called synchronously inside the paste event handler.
 * @param {DataTransfer|null} clipboardData
 * @returns {File[]}
 */
export function filesFromClipboard(clipboardData) {
  const files = [];
  for (const item of Array.from(clipboardData?.items ?? [])) {
    if (item.kind !== 'file') continue;
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
 * Upload files in parallel, keeping the input order for the result.
 * Shows a toast for the outcome.
 * @param {import('./api.js').Api} api
 * @param {File[]} files
 * @returns {Promise<string[]>} absolute paths of the uploaded files
 */
export async function uploadFiles(api, files) {
  if (files.length === 0) return [];
  const results = await Promise.allSettled(files.map((file) => api.upload(file)));
  const paths = [];
  const failures = [];
  results.forEach((result, index) => {
    if (result.status === 'fulfilled' && result.value?.path) paths.push(result.value.path);
    else failures.push(`${files[index].name || 'file'}: ${errorMessage(result.reason ?? 'no path returned')}`);
  });
  if (paths.length > 0) toast.success(`Uploaded ${plural(paths.length, 'file')}`);
  for (const failure of failures) toast.error(`Upload failed – ${failure}`);
  return paths;
}

let pendingResolve = null;
let input = null;

/**
 * Open the browser's file chooser for any file (must run inside a user gesture).
 * Resolves to the chosen files, or [] when the dialog is cancelled.
 * @returns {Promise<File[]>}
 */
export function pickFiles() {
  if (pendingResolve) {
    pendingResolve([]);
    pendingResolve = null;
  }
  if (!input) {
    input = document.createElement('input');
    input.type = 'file';
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
