import type { Schema } from './lib/api/client';

export type Kind = Schema['NewPublication']['content_type'];
export type Origin = { manual: true } | { manual: false; candidate: Schema['MetadataCandidate'] };
export type Details = {
  title: string;
  run_label: string;
  sort_title: string;
  known_unit_count: string;
  language: string;
  region: string;
  publisher: string;
};
export type Step = 1 | 2 | 3 | 4;
export type Recovery = {
  publicationId?: string;
  editionDone: boolean;
  existingLink?: boolean;
  existingTitle?: string;
  attempted?: 'publication' | 'edition' | 'link';
  complete?: boolean;
};
export type AddDraft = {
  version: 2;
  userId: string;
  kind?: Kind;
  manualPath: boolean;
  step: Step;
  search: { q: string; source: string };
  origin?: Origin;
  details: Details;
  recovery: Recovery;
};

export const emptyDetails: Details = {
  title: '', run_label: '', sort_title: '', known_unit_count: '', language: '', region: '', publisher: '',
};
export const commonLanguages = ['en', 'es', 'fr', 'de', 'it', 'pt', 'ja', 'ko', 'zh', 'ar', 'ru', 'pl', 'nl', 'ro'] as const;
const key = (userId: string) => `library:add-publication-draft:${userId}`;
const record = (value: unknown): value is Record<string, unknown> => Boolean(value) && typeof value === 'object' && !Array.isArray(value);
const text = (value: unknown, length: number): value is string => typeof value === 'string' && value.length <= length;
export const validId = (value: unknown): value is string =>
  typeof value === 'string' && /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i.test(value);
const isKind = (value: unknown): value is Kind => typeof value === 'string' && ['comic', 'manga', 'magazine'].includes(value);

export function validDraft(value: unknown, userId: string): value is AddDraft {
  if (!record(value) || value.version !== 2 || value.userId !== userId ||
      (value.kind !== undefined && !isKind(value.kind)) || typeof value.manualPath !== 'boolean' ||
      ![1, 2, 3, 4].includes(Number(value.step)) || typeof value.step !== 'number' ||
      !record(value.search) || !text(value.search.q, 512) || !text(value.search.source, 1000) ||
      !record(value.details) || !record(value.recovery)) return false;
  for (const field of Object.keys(emptyDetails) as (keyof Details)[]) {
    if (!text(value.details[field], field === 'language' || field === 'region' ? 100 : 1000)) return false;
  }
  if (value.origin !== undefined) {
    if (!record(value.origin) || typeof value.origin.manual !== 'boolean' || value.origin.manual !== value.manualPath) return false;
    if (!value.origin.manual) {
      const candidate = value.origin.candidate;
      if (!record(candidate) || typeof candidate.provider !== 'string' || !['comic_vine', 'manga_updates', 'manga_dex', 'local_manual', 'issn'].includes(candidate.provider) ||
          !text(candidate.external_id, 1000) || !candidate.external_id.trim() || !text(candidate.title, 1000) ||
          candidate.content_type !== value.kind || !(candidate.date === null || text(candidate.date, 1000))) return false;
    }
  }
  const recovery = value.recovery;
  if (typeof recovery.editionDone !== 'boolean' ||
      (recovery.publicationId !== undefined && !validId(recovery.publicationId)) ||
      (recovery.attempted !== undefined && (typeof recovery.attempted !== 'string' || !['publication', 'edition', 'link'].includes(recovery.attempted))) ||
      (recovery.existingLink !== undefined && typeof recovery.existingLink !== 'boolean') ||
      (recovery.existingTitle !== undefined && !text(recovery.existingTitle, 1000)) ||
      (recovery.complete !== undefined && typeof recovery.complete !== 'boolean')) return false;
  if ((recovery.editionDone || recovery.complete || recovery.attempted === 'edition' || recovery.attempted === 'link') && !recovery.publicationId) return false;
  if (recovery.existingLink) {
    if (!recovery.publicationId || !text(recovery.existingTitle, 1000) || !recovery.existingTitle.trim() ||
        recovery.editionDone || (recovery.attempted !== undefined && recovery.attempted !== 'link') ||
        !record(value.origin) || value.origin.manual !== false) return false;
  } else {
    if (recovery.existingTitle !== undefined || ((recovery.attempted === 'link' || recovery.complete) && !recovery.editionDone)) return false;
  }
  if (recovery.attempted === 'publication' && recovery.publicationId) return false;
  if (recovery.complete && recovery.attempted) return false;
  if (value.step >= 3 && (!value.kind || !value.origin)) return false;
  if ((recovery.publicationId || recovery.attempted) && (value.step !== 4 || !value.kind || !value.origin)) return false;
  return true;
}

export function readDraft(userId: string, storage: Storage): AddDraft | undefined {
  const raw = storage.getItem(key(userId));
  if (raw === null) return undefined;
  if (raw.length > 32000) throw new Error('Invalid draft');
  const value: unknown = JSON.parse(raw);
  if (!validDraft(value, userId)) throw new Error('Invalid draft');
  return value;
}

export function saveDraft(draft: AddDraft, storage: Storage) {
  if (!validDraft(draft, draft.userId)) throw new Error('Invalid draft');
  const raw = JSON.stringify(draft);
  if (raw.length > 32000) throw new Error('Draft too large');
  storage.setItem(key(draft.userId), raw);
  if (storage.getItem(key(draft.userId)) !== raw) throw new Error('Draft was not saved');
}

export function clearDraft(userId: string, storage: Storage) {
  storage.removeItem(key(userId));
  if (storage.getItem(key(userId)) !== null) throw new Error('Draft was not cleared');
}
