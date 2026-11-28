import { useEffect, useState } from 'react';

let quickAccessVisible = false;
const listeners = new Set<(visible: boolean) => void>();

/** Called by the Quick Access Menu host when its visibility changes. */
export function setQuickAccessVisible(visible: boolean): void {
  if (quickAccessVisible === visible) return;
  quickAccessVisible = visible;
  for (const listener of listeners) {
    listener(visible);
  }
}

/** React hook compatible with `@decky/api`'s `useQuickAccessVisible`. */
export function useQuickAccessVisible(): boolean {
  const [visible, setVisible] = useState(quickAccessVisible);
  useEffect(() => {
    listeners.add(setVisible);
    setVisible(quickAccessVisible);
    return () => {
      listeners.delete(setVisible);
    };
  }, []);
  return visible;
}
