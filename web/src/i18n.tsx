import { createContext, useContext, useEffect, useState, type ReactNode } from 'react';
import { en } from './locales/en';
import { es } from './locales/es';
const catalogs = { en, es };
export type Locale = keyof typeof catalogs;
export type MessageKey = keyof typeof en;
export function preference(key: string, fallback: string): string {
  try {
    return localStorage.getItem(key) ?? fallback;
  } catch {
    return fallback;
  }
}
export function savePreference(key: string, value: string) {
  try {
    localStorage.setItem(key, value);
  } catch {
    /* Preferences remain active for this visit. */
  }
}
const Context = createContext({
  locale: 'en' as Locale,
  setLocale: (_: Locale) => {},
  t: (key: MessageKey): string => en[key],
});
export function I18nProvider({ children }: { children: ReactNode }) {
  const [locale, setLocale] = useState<Locale>(() => {
    const preferred = preference('library.locale', navigator.language.split('-')[0]);
    return Object.hasOwn(catalogs, preferred) ? (preferred as Locale) : 'en';
  });
  useEffect(() => {
    document.documentElement.lang = locale;
    savePreference('library.locale', locale);
  }, [locale]);
  return (
    <Context value={{ locale, setLocale, t: (key) => catalogs[locale][key] ?? en[key] }}>
      {children}
    </Context>
  );
}
export const useI18n = () => useContext(Context);
