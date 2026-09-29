import { useState } from 'react';
import { IconBook } from '@tabler/icons-react';
import { type Schema, request } from './lib/api/client';
import { useI18n } from './i18n';
import { Button, ErrorNotice, Loading, formatSize, useResource } from './ui';

/* The reader loads its own title from the file context; back returns to this exact view. */
export function readerHref(fileId: string) {
  const params = new URLSearchParams({ back: location.hash.slice(1) });
  return `#/reader/${encodeURIComponent(fileId)}?${params}`;
}

/** Resolves the unit's files on click only (no per-row prefetch); several files open the list instead. */
export function ReadButton({ unitId, showFiles }: { unitId: string; showFiles: () => void }) {
  const { t } = useI18n();
  const [busy, setBusy] = useState(false);
  async function open() {
    setBusy(true);
    try {
      const files = await request<Schema['UnitFiles']>(`/units/${encodeURIComponent(unitId)}/files`);
      if (files.items.length === 1) location.hash = readerHref(files.items[0].id);
      else showFiles();
    } catch {
      showFiles();
    } finally {
      setBusy(false);
    }
  }
  return (
    <Button size="sm" variant="ghost" icon={IconBook} disabled={busy} aria-busy={busy} onClick={() => void open()}>
      {t('readFile')}
    </Button>
  );
}

export function FileList({ unitId }: { unitId: string }) {
  const { t, locale } = useI18n();
  const result = useResource<Schema['UnitFiles']>(`/units/${encodeURIComponent(unitId)}/files`);
  if (result.loading) return <Loading />;
  if (result.error) return <ErrorNotice error={result.error} retry={result.reload} />;
  return result.data?.items.length ? (
    <ul className="file-list">
      {result.data.items.map((file) => (
        <li key={file.id}>
          <span>
            {file.format.toUpperCase()}, {formatSize(file.size_bytes, locale)}
          </span>
          <a className="button sm" href={readerHref(file.id)}>
            {t('readFile')}
          </a>
        </li>
      ))}
    </ul>
  ) : (
    <p className="muted">{t('noFiles')}</p>
  );
}
