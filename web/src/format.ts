import type { Schema } from './lib/api/client';

function displayName(type: 'language' | 'region', code: string, locale: string) {
  try {
    return new Intl.DisplayNames([locale], { type, fallback: 'code' }).of(code) ?? code;
  } catch {
    return code;
  }
}

/* "English (US)": language as a name, region kept as the short code collectors use. */
export function editionName(edition: Schema['Edition'], locale: string) {
  const language = displayName('language', edition.language, locale);
  return edition.region ? `${language} (${edition.region})` : language;
}

export function editionLabel(edition: Schema['Edition'], locale: string) {
  return [editionName(edition, locale), edition.publisher].filter(Boolean).join(', ');
}

export function formatUnitDate(unit: Schema['Unit'], locale: string) {
  if (!unit.date) return undefined;
  const [year, month = 1, day = 1] = unit.date.split('-').map(Number);
  const precision = unit.date_precision ?? (unit.date.length === 4 ? 'year' : unit.date.length === 7 ? 'month' : 'day');
  if (precision === 'year' || !Number.isFinite(month)) return String(year);
  const date = new Date(Date.UTC(year, month - 1, day));
  if (Number.isNaN(date.getTime())) return unit.date;
  return new Intl.DateTimeFormat(locale, {
    timeZone: 'UTC',
    ...(precision === 'month' ? { year: 'numeric', month: 'long' } : { dateStyle: 'medium' }),
  }).format(date);
}
