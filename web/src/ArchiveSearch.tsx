import { useEffect, useRef, useState } from 'react';
import { IconExternalLink, IconSearch } from '@tabler/icons-react';
import { useI18n } from './i18n';
import { request, type Schema } from './lib/api/client';
import { Button, EmptyState, ErrorNotice, Icon, Loading, StatusBadge, formatSize } from './ui';

type SearchInput = {
  integration_id: string;
  query: string;
  page: number;
  limit: number;
};
type Action =
  | { kind: 'search'; input: SearchInput }
  | { kind: 'item'; input: { integration_id: string; identifier: string } };
const reasonLabels = {
  metadata_eligible: 'archiveReasonEligible',
  access_restricted: 'archiveReasonRestricted',
  item_unavailable: 'archiveReasonUnavailable',
  unsupported_format: 'archiveReasonFormat',
  incomplete_identity: 'archiveReasonIdentity',
} as const;

export function ArchiveSearch({
  choices,
  defaultQuery = '',
}: {
  choices: Schema['IntegrationChoice'][];
  defaultQuery?: string;
}) {
  const { t, locale } = useI18n();
  const sources = choices.filter((choice) => choice.kind === 'internetarchive');
  const [source, setSource] = useState(sources.length === 1 ? sources[0].id : '');
  const [query, setQuery] = useState(defaultQuery);
  const [search, setSearch] = useState<SearchInput>();
  const [page, setPage] = useState<Schema['ArchivePage']>();
  const [inspecting, setInspecting] = useState<string>();
  const [item, setItem] = useState<Schema['ArchiveItem']>();
  const [lastAction, setLastAction] = useState<Action>();
  const [error, setError] = useState<unknown>();
  const [busy, setBusy] = useState(false);
  const eligibleFiles = item?.files.filter((file) => file.eligibility === 'available') ?? [];
  const otherFiles = item?.files.filter((file) => file.eligibility !== 'available') ?? [];
  const active = useRef<AbortController | null>(null);
  useEffect(() => () => active.current?.abort(), []);

  function clearResults() {
    setSearch(undefined);
    setPage(undefined);
    setInspecting(undefined);
    setItem(undefined);
    setLastAction(undefined);
    setError(undefined);
  }

  async function run(action: Action) {
    if (active.current) return;
    const controller = new AbortController();
    active.current = controller;
    setBusy(true);
    setError(undefined);
    setLastAction(action);
    setItem(undefined);
    try {
      if (action.kind === 'search') {
        setSearch(action.input);
        setPage(undefined);
        setInspecting(undefined);
        const params = new URLSearchParams({
          ...action.input,
          page: String(action.input.page),
          limit: String(action.input.limit),
        });
        const result = await request<Schema['ArchivePage']>(`/search/archive?${params}`, {
          signal: controller.signal,
        });
        if (!controller.signal.aborted) setPage(result);
      } else {
        setInspecting(action.input.identifier);
        const params = new URLSearchParams(action.input);
        const result = await request<Schema['ArchiveItem']>(`/search/archive/item?${params}`, {
          signal: controller.signal,
        });
        if (!controller.signal.aborted) setItem(result);
      }
    } catch (failure) {
      if (!controller.signal.aborted) setError(failure);
    } finally {
      if (active.current === controller) {
        active.current = null;
        if (!controller.signal.aborted) setBusy(false);
      }
    }
  }


  if (sources.length === 0)
    return (
      <div className="releases">
        <EmptyState
          title={t('archiveNotConfigured')}
          action={
            <a className="button" href="#/settings?section=sources">
              {t('openSourceSettings')}
            </a>
          }
        >
          {t('archiveNoSource')}
        </EmptyState>
      </div>
    );
  return (
    <div className="releases" aria-busy={busy}>
      <p className="muted releases-hint">{t('archiveSearchHint')}</p>
      <fieldset disabled={busy}>
        <form
          className="release-search-form"
          onSubmit={(event) => {
            event.preventDefault();
            void run({
              kind: 'search',
              input: { integration_id: source, query: query.trim(), page: 1, limit: 20 },
            });
          }}
        >
          <label className="field">
            <span>{t('source')}</span>
            <select
              name="archive_source"
              required
              value={source}
              onChange={(event) => {
                if (active.current) return;
                setSource(event.target.value);
                clearResults();
              }}
            >
              <option value="" disabled>
                {t('chooseSource')}
              </option>
              {sources.map((choice) => (
                <option key={choice.id} value={choice.id}>
                  {choice.label}
                </option>
              ))}
            </select>
          </label>
          <label className="field release-query">
            <span>{t('searchQuery')}</span>
            <input
              name="archive_query"
              required
              maxLength={512}
              value={query}
              onChange={(event) => {
                if (active.current) return;
                setQuery(event.target.value);
                clearResults();
              }}
            />
          </label>
          <Button type="submit" variant="primary" icon={IconSearch}>
            {t('search')}
          </Button>
        </form>
        <ErrorNotice
          error={error}
          retry={lastAction ? () => void run(lastAction) : undefined}
        />
        {inspecting !== undefined ? (
          <div className="release-detail">
            <div className="section-header">
              <h3>{item?.magazine.title ?? inspecting}</h3>
              <Button
                variant="ghost"
                size="sm"
                onClick={() => {
                  if (active.current) return;
                  setInspecting(undefined);
                  setItem(undefined);
                  setError(undefined);
                  setLastAction(undefined);
                }}
              >
                {t('archiveBackResults')}
              </Button>
            </div>
            {item && (
              <>
                <ArchiveMetadata magazine={item.magazine} />
                <ArchiveSource identifier={item.magazine.identifier} />
                <h4 className="release-subheading">
                  {t('files')} ({eligibleFiles.length.toLocaleString(locale)})
                </h4>
                {eligibleFiles.length > 0 && <ArchiveFiles files={eligibleFiles} />}
                {otherFiles.length > 0 && (
                  <details>
                    <summary>
                      {t('archiveOtherFiles')} ({otherFiles.length.toLocaleString(locale)})
                    </summary>
                    <ArchiveFiles files={otherFiles} />
                  </details>
                )}
                {item.files.length === 0 && <p className="muted">{t('archiveNoFiles')}</p>}
              </>
            )}
          </div>
        ) : (
          page &&
          search && (
            <>
              {page.items.length ? (
                <>
                  <p className="muted release-count">
                    {t('archiveTotal')}: {page.total.toLocaleString(locale)}
                  </p>
                  <ul className="release-list">
                    {page.items.map((magazine) => (
                      <li key={magazine.identifier}>
                        <div>
                          <span className="release-name">{magazine.title}</span>
                          <ArchiveMetadata magazine={magazine} />
                        </div>
                        <Button
                          size="sm"
                          onClick={() =>
                            void run({
                              kind: 'item',
                              input: {
                                integration_id: search.integration_id,
                                identifier: magazine.identifier,
                              },
                            })
                          }
                        >
                          {t('archiveInspectFiles')}
                        </Button>
                      </li>
                    ))}
                  </ul>
                </>
              ) : (
                <EmptyState title={t('noReleasesFound')}>{t('noReleasesFoundHint')}</EmptyState>
              )}
              {(search.page > 1 || page.next_page !== null) && (
                <div className="pagination">
                  <Button
                    size="sm"
                    disabled={search.page <= 1}
                    onClick={() =>
                      void run({
                        kind: 'search',
                        input: { ...search, page: search.page - 1, limit: page.page_size },
                      })
                    }
                  >
                    {t('previous')}
                  </Button>
                  <span>
                    {t('page')} {search.page.toLocaleString(locale)}
                  </span>
                  <Button
                    size="sm"
                    disabled={page.next_page === null}
                    onClick={() =>
                      page.next_page !== null &&
                      void run({
                        kind: 'search',
                        input: { ...search, page: page.next_page, limit: page.page_size },
                      })
                    }
                  >
                    {t('next')}
                  </Button>
                </div>
              )}
            </>
          )
        )}
      </fieldset>
      {busy && <Loading />}
    </div>
  );
}

function formatArchiveMonth(raw: string, locale: string): string | undefined {
  if (raw.length !== 7 || !/^[0-9]{4}-(0[1-9]|1[0-2])$/.test(raw)) return undefined;
  const year = Number(raw.slice(0, 4));
  if (year === 0) return undefined;
  const date = new Date(0);
  date.setUTCFullYear(year, Number(raw.slice(5)) - 1, 1);
  return new Intl.DateTimeFormat(locale, {
    year: 'numeric',
    month: 'long',
    timeZone: 'UTC',
  }).format(date);
}

function ArchiveMetadata({ magazine }: { magazine: Schema['ArchiveMagazine'] }) {
  const { t, locale } = useI18n();
  const dates = magazine.dates.map((raw) => ({
    raw,
    month: formatArchiveMonth(raw, locale),
  }));
  const values = (items: string[]) => (items.length ? items.join(' / ') : t('unknown'));
  const summary = [
    dates.map((date) => date.month ?? date.raw).join(', '),
    magazine.languages.join(', '),
  ]
    .filter(Boolean)
    .join(' · ');
  return (
    <>
      {summary && <p className="release-row-meta">{summary}</p>}
      <details>
        <summary>{t('sourceDetails')}</summary>
        <dl className="job-details">
          <dt>{t('archiveIdentifier')}</dt>
          <dd>{magazine.identifier}</dd>
          <dt>{t('archiveCountries')}</dt>
          <dd>{values(magazine.countries)}</dd>
          <dt>{t('archiveCoverage')}</dt>
          <dd>{values(magazine.coverage)}</dd>
          <dt>{t('volume')}</dt>
          <dd>{values(magazine.volumes)}</dd>
          <dt>{t('issue')}</dt>
          <dd>{values(magazine.issues)}</dd>
          <dt>{t('archiveRawDates')}</dt>
          <dd>{values(magazine.dates)}</dd>
          <dt>{t('archiveDatePrecision')}</dt>
          <dd>
            {values(
              dates.map(
                (date) =>
                  `${date.raw}: ${t(date.month ? 'archiveMonthPrecision' : 'unknown')}`,
              ),
            )}
          </dd>
        </dl>
      </details>
    </>
  );
}

function ArchiveFiles({ files }: { files: Schema['ArchiveFile'][] }) {
  const { t, locale } = useI18n();
  return (
    <ul className="release-list">
      {files.map((file) => (
        <li key={file.name}>
          <div>
            <span className="release-name path">{file.name}</span>
            <p className="release-row-meta">
              {[file.format, file.size === null ? undefined : formatSize(file.size, locale)]
                .filter(Boolean)
                .join(' · ')}
            </p>
            <details>
              <summary>{t('sourceDetails')}</summary>
              <dl className="job-details">
                <dt>{t('reason')}</dt>
                <dd>{t(reasonLabels[file.reason])}</dd>
                <dt>{t('archiveSha1')}</dt>
                <dd>{file.sha1 ?? t('unknown')}</dd>
                <dt>{t('archiveMd5')}</dt>
                <dd>{file.md5 ?? t('unknown')}</dd>
              </dl>
            </details>
          </div>
          <StatusBadge
            kind={
              file.eligibility === 'available'
                ? 'info'
                : file.eligibility === 'restricted'
                  ? 'warning'
                  : 'error'
            }
            label={t(
              file.eligibility === 'available'
                ? 'archiveEligible'
                : file.eligibility === 'restricted'
                  ? 'archiveRestricted'
                  : 'archiveUnavailable',
            )}
          />
        </li>
      ))}
    </ul>
  );
}

function ArchiveSource({ identifier }: { identifier: string }) {
  const { t } = useI18n();
  if (
    identifier.length < 1 ||
    identifier.length > 100 ||
    !/^[A-Za-z0-9]/.test(identifier) ||
    /[^A-Za-z0-9_.-]/.test(identifier)
  )
    return null;
  return (
    <p>
      <a
        href={`https://archive.org/details/${encodeURIComponent(identifier)}`}
        target="_blank"
        rel="noopener noreferrer"
      >
        {t('archiveSourcePage')} <Icon icon={IconExternalLink} size={16} />
      </a>
    </p>
  );
}
