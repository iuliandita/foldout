import { useSyncExternalStore } from 'react';
import { preference, savePreference } from './i18n';

const preferenceKey = 'library.printable-shortcuts';
let enabled = preference(preferenceKey, 'false') === 'true';
const listeners = new Set<() => void>();

export const printableShortcutsEnabled = () => enabled;

export function setPrintableShortcuts(next: boolean) {
  enabled = next;
  savePreference(preferenceKey, String(next));
  listeners.forEach((listener) => listener());
}

// The reader has no subscribers, but changes in another tab still need to update the cache.
window.addEventListener('storage', (event: StorageEvent) => {
  if (event.key !== preferenceKey && event.key !== null) return;
  const next = event.key === null ? false : event.newValue === 'true';
  if (next === enabled) return;
  enabled = next;
  listeners.forEach((notify) => notify());
});

export function subscribePrintableShortcuts(listener: () => void) {
  listeners.add(listener);
  return () => {
    listeners.delete(listener);
  };
}

export function usePrintableShortcuts() {
  return useSyncExternalStore(subscribePrintableShortcuts, printableShortcutsEnabled);
}
