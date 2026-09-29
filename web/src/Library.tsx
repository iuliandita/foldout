import { useEffect, useId, useMemo, useRef, useState } from 'react';
import {
  IconAdjustmentsHorizontal,
  IconChevronDown,
  IconChevronLeft,
  IconPlugConnected,
  IconX,
  IconChevronRight,
  IconFiles,
  IconLayoutGrid,
  IconList,
  IconPencil,
  IconPlus,
  IconBell,
  IconBellOff,
  IconBook,
  IconSearch,
  IconListSearch,
} from '@tabler/icons-react';
import { Cover } from './Cover';
import { editionName, formatUnitDate } from './format';
import { FileList, ReadButton, readerHref } from './UnitFiles';
import { unitStatus } from './status';
import { ApiError, type Schema, post, request } from './lib/api/client';
import { type MessageKey, preference, savePreference, useI18n } from './i18n';
import {
  Button,
  EmptyState,
  domId,
  ErrorNotice,
  Field,
  HelpTip,
  Icon,
  IconButton,
  Loading,
  Menu,
  type MenuItem,
  PageHeader,
  SaveForm,
  SearchField,
  SegmentedControl,
  Skeleton,
  StatusBadge,
  optional,
  usePagedList,
  useResource,
  useRowKeys,
  value,
} from './ui';
import './styles/library.css';

export const contentTypes = ['comic', 'manga', 'magazine'] as const;
type ContentType = (typeof contentTypes)[number];
type Translate = (key: MessageKey) => string;
const typeNames = { comic: 'typeComic', manga: 'typeManga', magazine: 'typeMagazine' } as const;
type View = 'list' | 'covers';
const viewKey = 'library.view';
const sorts = ['title', 'recently_added'] as const;
type Sort = (typeof sorts)[number];
const sortNames = { title: 'sortByTitle', recently_added: 'sortRecentlyAdded' } as const;
const availabilities = ['all', 'has_files', 'no_files'] as const;
type Availability = (typeof availabilities)[number];
const availabilityNames = { all: 'all', has_files: 'availabilityHasFiles', no_files: 'availabilityNoFiles' } as const;
type ListState = {
  kind?: ContentType | null;
  q?: string;
  sort?: Sort;
  availability?: Availability;
  view?: View | null;
  cursor?: string;
};

function go(hash: string, replace: boolean) {
  if (replace) {
    history.replaceState(history.state, '', hash);
    window.dispatchEvent(new HashChangeEvent('hashchange'));
  } else location.hash = hash;
}

/* Files owned and the declared total are different measures, so they are never compared with "of":
   "1 file, 75 expected", "No files, 14 expected", or just "3 files" when no total is declared. */
export function availabilityText(
  t: Translate,
  locale: string,
  files: number,
  known: number | null,
  group?: string,
) {
  const number = new Intl.NumberFormat(locale);
  let owned = !files
    ? t('noFilesYet')
    : files === 1
      ? t('fileCountOne')
      : t('fileCountOther').replace('{count}', number.format(files));
  if (group) owned = t('availabilityIn').replace('{files}', owned).replace('{group}', group);
  return known === null ? owned : t('availabilityExpected').replace('{files}', owned).replace('{known}', number.format(known));
}
function metaLine(t: Translate, item: Schema['PublicationSummary']) {
  return [t(typeNames[item.content_type]), item.run_label].filter(Boolean).join(', ');
}

export function Library({ query, canManage, isAdmin }: { query: URLSearchParams; canManage: boolean; isAdmin: boolean }) {
  const { t, locale } = useI18n();
  const kind = contentTypes.find((kind) => kind === query.get('kind'));
  const cursor = query.get('cursor');
  const q = query.get('q')?.trim() ?? '';
  const sort = sorts.find((sort) => sort === query.get('sort')) ?? 'title';
  const availability = availabilities.find((value) => value === query.get('availability')) ?? 'all';
  const params = new URLSearchParams({ limit: '50' });
  if (kind) params.set('kind', kind);
  if (q) params.set('q', q);
  if (sort !== 'title') params.set('sort', sort);
  if (availability !== 'all') params.set('availability', availability);
  if (cursor) params.set('cursor', cursor);
  const result = useResource<Schema['PublicationPage']>(`/publications?${params}`);
  const [previous, setPrevious] = useState<string[]>([]);
  /* ?view= makes the layout linkable; without it the stored preference applies. */
  const urlView = query.get('view') === 'covers' ? 'covers' : query.get('view') === 'list' ? 'list' : null;
  const [storedView, setStoredView] = useState<View>(() =>
    preference(viewKey, 'list') === 'covers' ? 'covers' : 'list',
  );
  const view = urlView ?? storedView;
  const [filtersOpen, setFiltersOpen] = useState(false);
  const listRef = useRef<HTMLUListElement>(null);
  useRowKeys(listRef);
  /* Content type, view, and sort stay visible; availability sits behind the Filters disclosure. */
  const activeFilters = availability !== 'all' ? 1 : 0;
  const filtersId = useId();
  useEffect(() => setPrevious([]), [kind, q, sort, availability]);
  /* Defaults stay out of the URL; any change without an explicit cursor starts from the first page. */
  function listQuery(next: ListState = {}) {
    const state = { kind, q, sort, availability, view: urlView, cursor: undefined, ...next };
    const target = new URLSearchParams();
    if (state.kind) target.set('kind', state.kind);
    if (state.q) target.set('q', state.q);
    if (state.sort !== 'title') target.set('sort', state.sort);
    if (state.availability !== 'all') target.set('availability', state.availability);
    if (state.view) target.set('view', state.view);
    if (state.cursor) target.set('cursor', state.cursor);
    return target;
  }
  const libraryHref = (next?: ListState) => `#/?${listQuery(next)}`;
  /* A cursor only fits the sort and filters it came from; a stale one (400) restarts at the first page. */
  const staleCursor = !!cursor && result.error instanceof ApiError && result.error.status === 400;
  useEffect(() => {
    if (staleCursor) {
      setPrevious([]);
      go(libraryHref(), true);
    }
  }, [staleCursor]);
  const detailQuery = listQuery({ cursor: cursor ?? undefined });
  const items = result.data?.items ?? [];
  const filtered = !!kind || availability !== 'all';
  const emptyLibrary = !!result.data && items.length === 0 && !q && !filtered && !cursor;
  return (
    <>
      <PageHeader
        title={t('library')}
        actions={
          !emptyLibrary && (
            <>
              <label className="library-select library-sort">
                <span>{t('librarySort')}</span>
                <select
                  value={sort}
                  onChange={(event) =>
                    go(libraryHref({ sort: sorts.find((value) => value === event.target.value) ?? 'title' }), false)
                  }
                >
                  {sorts.map((value) => (
                    <option key={value} value={value}>
                      {t(sortNames[value])}
                    </option>
                  ))}
                </select>
              </label>
              {canManage && (
                <a className="button primary library-add" href={`#/new${kind ? `?kind=${kind}` : ''}`}>
                  <Icon icon={IconPlus} size={18} />
                  {t('add')}
                </a>
              )}
            </>
          )
        }
      />
      {!emptyLibrary && <div className="library-toolbar">
        <SearchField
          label={t('searchPublications')}
          value={q}
          onChange={(search) => {
            setPrevious([]);
            go(libraryHref({ q: search.trim() }), true);
          }}
        />
        <div className="library-controls">
          <SegmentedControl
            label={t('contentType')}
            value={kind ?? 'all'}
            options={[
              { value: 'all', label: t('all') },
              ...contentTypes.map((type) => ({ value: type, label: t(type) })),
            ]}
            onChange={(type) => go(libraryHref({ kind: type === 'all' ? null : type }), false)}
          />
          <SegmentedControl<View>
            label={t('libraryView')}
            value={view}
            options={[
              { value: 'list', label: t('viewList'), icon: IconList },
              { value: 'covers', label: t('viewCovers'), icon: IconLayoutGrid },
            ]}
            onChange={(next) => {
              setStoredView(next);
              savePreference(viewKey, next);
              go(libraryHref({ view: next, cursor: cursor ?? undefined }), true);
            }}
          />
          <button
            type="button"
            className="library-filters-toggle"
            aria-expanded={filtersOpen}
            aria-controls={filtersId}
            onClick={() => setFiltersOpen((open) => !open)}
          >
            <Icon icon={IconAdjustmentsHorizontal} size={18} />
            {activeFilters
              ? t('libraryFiltersActive').replace('{count}', String(activeFilters))
              : t('libraryFilters')}
            <Icon icon={IconChevronDown} size={16} />
          </button>
        </div>
        <div className="library-filters" id={filtersId} hidden={!filtersOpen}>
          <label className="library-select">
            <span>{t('libraryAvailability')}</span>
            <select
              value={availability}
              onChange={(event) =>
                go(
                  libraryHref({
                    availability: availabilities.find((value) => value === event.target.value) ?? 'all',
                  }),
                  false,
                )
              }
            >
              {availabilities.map((value) => (
                <option key={value} value={value}>
                  {t(availabilityNames[value])}
                </option>
              ))}
            </select>
          </label>
        </div>
      </div>}
      {result.loading || staleCursor ? (
        <Loading />
      ) : result.error ? (
        <ErrorNotice error={result.error} retry={result.reload} />
      ) : (
        result.data && (
          <>
            {emptyLibrary ? (
              <section className="library-start" aria-labelledby="library-start-title">
                <h2 id="library-start-title">{t('emptyLibrary')}</h2>
                {canManage && <p>{t(isAdmin ? 'libraryStartIntro' : 'libraryStartManageIntro')}</p>}
                {canManage ? (
                  <>
                    <ul className="library-start-options">
                      {([
                        ...(isAdmin ? [{ href: '#/settings?section=storage', icon: IconFiles, title: 'libraryStartScan', hint: 'libraryStartScanHint' }] as const : []),
                        { href: '#/new', icon: IconSearch, title: 'libraryStartFind', hint: 'libraryStartFindHint' },
                        { href: '#/new?manual=1', icon: IconPencil, title: 'libraryStartManual', hint: 'libraryStartManualHint' },
                      ] as const).map((option) => (
                        <li key={option.href}>
                          <a href={option.href}>
                            <Icon icon={option.icon} size={22} />
                            <span>
                              <strong>{t(option.title)}</strong>
                              <span>{t(option.hint)}</span>
                            </span>
                            <Icon icon={IconChevronRight} size={18} />
                          </a>
                        </li>
                      ))}
                    </ul>
                    <p className="library-start-source">
                      {isAdmin ? <>
                        {t('libraryStartSourceHint')}{' '}
                        <a href="#/settings?section=sources">{t('addSetUpSource')}</a>
                      </> : t('libraryStartAdminHint')}
                    </p>
                  </>
                ) : <p>{t('libraryStartManagerHint')}</p>}
              </section>
            ) : items.length === 0 ? (
              <EmptyState
                title={t(
                  q
                    ? 'noSearchResults'
                    : availability !== 'all'
                      ? 'emptyNoMatch'
                      : kind
                        ? 'emptyFiltered'
                        : 'emptyLibrary',
                )}
                action={
                  q ? (
                    <a className="button" href={libraryHref({ q: '' })}>
                      {t('clearSearch')}
                    </a>
                  ) : filtered ? (
                    <a className="button" href={libraryHref({ kind: null, availability: 'all' })}>
                      {t(availability !== 'all' ? 'resetFilters' : 'clearFilters')}
                    </a>
                  ) : undefined
                }
              >
                {!q && !filtered ? t('emptyHint') : undefined}
              </EmptyState>
            ) : view === 'covers' ? (
              <ul className="library-grid">
                {items.map((item) => (
                  <li key={item.id}>
                    <a
                      className="library-tile"
                      href={`#/publication/${item.id}?${detailQuery}`}
                      title={item.title}
                      aria-labelledby={domId('tile', item.id, 'title')}
                      aria-describedby={domId('tile', item.id, 'availability')}
                    >
                      <span className="library-shelf">
                        <Cover
                          fileId={item.cover_file_id}
                          title={item.title}
                          contentType={item.content_type}
                          width="fill"
                        />
                      </span>
                      <strong id={domId('tile', item.id, 'title')}>{item.title}</strong>
                      <span className="muted" id={domId('tile', item.id, 'availability')}>
                        {availabilityText(t, locale, item.file_count, item.known_unit_count)}
                      </span>
                    </a>
                  </li>
                ))}
              </ul>
            ) : (
              <ul className="library-list" ref={listRef}>
                {items.map((item) => (
                  <li key={item.id}>
                    <a
                      className="library-row"
                      data-row
                      href={`#/publication/${item.id}?${detailQuery}`}
                      aria-labelledby={domId('row', item.id, 'title')}
                      aria-describedby={[
                        domId('row', item.id, 'meta'),
                        domId('row', item.id, 'availability'),
                        ...(item.monitored_unit_count > 0 ? [domId('row', item.id, 'monitoring')] : []),
                      ].join(' ')}
                    >
                      <Cover
                        fileId={item.cover_file_id}
                        title={item.title}
                        contentType={item.content_type}
                        width={56}
                      />
                      <span className="library-row-main">
                        <strong className="library-row-title" id={domId('row', item.id, 'title')} title={item.title}>
                          {item.title}
                        </strong>
                        <span className="library-row-meta" id={domId('row', item.id, 'meta')}>
                          {metaLine(t, item)}
                        </span>
                      </span>
                      <span className="library-row-status">
                        <span className="library-row-availability" id={domId('row', item.id, 'availability')}>
                          {availabilityText(t, locale, item.file_count, item.known_unit_count)}
                        </span>
                        {item.monitored_unit_count > 0 && (
                          <span className="library-row-monitoring" id={domId('row', item.id, 'monitoring')}>
                            <Icon icon={IconBell} size={16} />
                            {t('statusMonitored')}
                          </span>
                        )}
                      </span>
                      <Icon icon={IconChevronRight} size={18} />
                    </a>
                  </li>
                ))}
              </ul>
            )}
            {(cursor || result.data.next_cursor) && (
              <footer className="pagination">
                <div className="actions">
                  {previous.length > 0 && (
                    <a
                      className="button"
                      href={libraryHref({ cursor: previous[previous.length - 1] || undefined })}
                      onClick={() => setPrevious((items) => items.slice(0, -1))}
                    >
                      {t('previous')}
                    </a>
                  )}
                  {cursor && (
                    <a className="button" href={libraryHref()} onClick={() => setPrevious([])}>
                      {t('firstPage')}
                    </a>
                  )}
                  {result.data.next_cursor && (
                    <a
                      className="button"
                      href={libraryHref({ cursor: result.data.next_cursor })}
                      onClick={() => setPrevious((items) => [...items, cursor ?? ''])}
                    >
                      {t('next')}
                    </a>
                  )}
                </div>
              </footer>
            )}
          </>
        )
      )}
    </>
  );
}
function editionLabels(editions: Schema['Edition'][], locale: string) {
  const names = editions.map((edition) => editionName(edition, locale));
  return editions.map((edition, index) =>
    names.filter((name) => name === names[index]).length > 1 && edition.publisher
      ? `${names[index]}, ${edition.publisher}`
      : names[index],
  );
}
/* All cataloged units of a publication with availability, in pages of 100 (one pass, not per unit). */
function useWantedUnits(publicationId: string) {
  const [version, setVersion] = useState(0);
  const [state, setState] = useState<{
    items: Schema['WantedUnit'][];
    complete: boolean;
    loading: boolean;
    error?: unknown;
  }>({ items: [], complete: false, loading: true });
  useEffect(() => {
    const controller = new AbortController();
    setState((current) => ({ ...current, loading: true, error: undefined }));
    (async () => {
      const items: Schema['WantedUnit'][] = [];
      let cursor: string | null = null;
      for (let page = 0; page < 20; page++) {
        const params = new URLSearchParams({
          publication_id: publicationId,
          availability: 'all',
          monitoring: 'all',
          limit: '100',
        });
        if (cursor) params.set('cursor', cursor);
        const result: Schema['WantedPage'] = await request<Schema['WantedPage']>(`/wanted?${params}`, {
          signal: controller.signal,
        });
        items.push(...result.items);
        cursor = result.next_cursor;
        if (!cursor) break;
      }
      if (!controller.signal.aborted) setState({ items, complete: !cursor, loading: false });
    })().catch((error) => {
      if (!controller.signal.aborted) setState({ items: [], complete: false, loading: false, error });
    });
    return () => controller.abort();
  }, [publicationId, version]);
  const byUnit = useMemo(
    () => new Map(state.items.map((item) => [item.context.unit.id, item])),
    [state.items],
  );
  return { ...state, byUnit, reload: () => setVersion((value) => value + 1) };
}

/* Enabled Prowlarr sources that can run monitors; loaded only for managers. */
function useProwlarrSources(enabled: boolean) {
  const [state, setState] = useState<{ items?: Schema['IntegrationChoice'][]; error?: unknown }>({});
  useEffect(() => {
    if (!enabled) return;
    const controller = new AbortController();
    request<Schema['IntegrationChoiceList']>('/search/integrations', { signal: controller.signal })
      .then((list) => {
        if (!controller.signal.aborted)
          setState({ items: list.items.filter((item) => item.kind === 'prowlarr' && item.release_search) });
      })
      .catch((error) => {
        if (!controller.signal.aborted) setState({ error });
      });
    return () => controller.abort();
  }, [enabled]);
  return state;
}

const intervalChoices = ['21600', '86400', '604800'] as const;
type IntervalChoice = (typeof intervalChoices)[number];
function intervalLabel(seconds: number, locale: string) {
  const [amount, unit] =
    seconds % 604800 === 0
      ? [seconds / 604800, 'week']
      : seconds % 86400 === 0
        ? [seconds / 86400, 'day']
        : [seconds / 3600, 'hour'];
  return new Intl.NumberFormat(locale, { style: 'unit', unit, unitDisplay: 'long' }).format(amount);
}
/* Mirrors the API rule: nonblank, at most 512 UTF-8 bytes, no control characters or "://". */
function queryValid(query: string) {
  const trimmed = query.trim();
  return (
    !!trimmed &&
    new TextEncoder().encode(trimmed).length <= 512 &&
    !/\p{Cc}/u.test(trimmed) &&
    !trimmed.includes('://')
  );
}
type RowResult =
  | { status: 'running' | 'done' | 'exists' | 'uncertain' }
  | { status: 'failed'; message: string };
function failure(t: Translate, error: unknown): RowResult {
  if (error instanceof ApiError && error.code === 'uncertain_result') return { status: 'uncertain' };
  return { status: 'failed', message: (error instanceof ApiError && error.message) || t('requestFailed') };
}
function ResultBadge({ result, done }: { result?: RowResult; done: MessageKey }) {
  const { t } = useI18n();
  if (!result) return null;
  if (result.status === 'running') return <StatusBadge kind="info" label={t('bulkWorking')} />;
  if (result.status === 'done') return <StatusBadge kind="monitored" label={t(done)} />;
  if (result.status === 'exists') return <StatusBadge kind="monitored" label={t('bulkAlreadyMonitored')} />;
  if (result.status === 'uncertain') return <StatusBadge kind="warning" label={t('bulkNotConfirmed')} />;
  return <StatusBadge kind="error" label={t('bulkFailed')} />;
}
function usePanelFocus() {
  const heading = useRef<HTMLHeadingElement>(null);
  const alive = useRef(true);
  useEffect(() => {
    alive.current = true;
    heading.current?.focus();
    return () => {
      alive.current = false;
    };
  }, []);
  return { heading, alive };
}

/* One review-only monitor per selected missing unit, created one at a time; failures are shown, never retried automatically. */
function BulkMonitor({
  publication,
  candidates,
  loading,
  complete,
  sources,
  sourcesError,
  close,
}: {
  publication: Schema['PublicationSummary'];
  candidates: Schema['WantedUnit'][];
  loading: boolean;
  complete: boolean;
  sources?: Schema['IntegrationChoice'][];
  sourcesError?: unknown;
  close: (changed: boolean) => void;
}) {
  const { t, locale } = useI18n();
  const headingId = useId();
  const { heading, alive } = usePanelFocus();
  const [units, setUnits] = useState(candidates);
  const [snapshot, setSnapshot] = useState(!loading);
  if (!snapshot && !loading) {
    setSnapshot(true);
    setUnits(candidates);
  }
  const [queries, setQueries] = useState<Record<string, string>>({});
  const [excluded, setExcluded] = useState<ReadonlySet<string>>(new Set());
  const [chosenSource, setSource] = useState<string>();
  const [interval, setIntervalChoice] = useState<IntervalChoice>('86400');
  const [results, setResults] = useState<Record<string, RowResult>>({});
  const [running, setRunning] = useState(false);
  const [progress, setProgress] = useState({ done: 0, total: 0 });
  const [showInvalid, setShowInvalid] = useState(false);
  const stop = useRef(false);
  const busy = useRef(false);
  const sourceId = chosenSource ?? sources?.[0]?.id;
  const editions = new Set(units.map((unit) => unit.context.edition.id));
  const queryOf = (unit: Schema['WantedUnit']) =>
    queries[unit.context.unit.id] ?? `${publication.title} ${unit.context.unit.label}`.trim();
  const finished = (id: string) => results[id]?.status === 'done' || results[id]?.status === 'exists';
  const pending = units.filter((unit) => !excluded.has(unit.context.unit.id) && !finished(unit.context.unit.id));
  const created = Object.values(results).filter((result) => result.status === 'done').length;
  const failedCount = Object.values(results).filter(
    (result) => result.status === 'failed' || result.status === 'uncertain',
  ).length;
  const attempted = Object.keys(results).length;
  async function run() {
    if (busy.current || !sourceId) return;
    if (pending.some((unit) => !queryValid(queryOf(unit)))) {
      setShowInvalid(true);
      return;
    }
    busy.current = true;
    stop.current = false;
    setShowInvalid(false);
    setRunning(true);
    setProgress({ done: 0, total: pending.length });
    for (const unit of pending) {
      if (stop.current || !alive.current) break;
      const id = unit.context.unit.id;
      setResults((current) => ({ ...current, [id]: { status: 'running' } }));
      let result: RowResult;
      try {
        await post<Schema['MonitorView']>('/monitors', {
          unit_id: id,
          integration_id: sourceId,
          query: queryOf(unit).trim(),
          interval_seconds: Number(interval),
          enabled: true,
          selection_policy: 'review_only',
        } satisfies Schema['CreateMonitor']);
        result = { status: 'done' };
      } catch (error) {
        result =
          error instanceof ApiError && error.status === 409 && error.code === 'monitor_conflict'
            ? { status: 'exists' }
            : failure(t, error);
      }
      if (!alive.current) break;
      setResults((current) => ({ ...current, [id]: result }));
      setProgress((current) => ({ ...current, done: current.done + 1 }));
    }
    busy.current = false;
    if (alive.current) setRunning(false);
  }
  const changed = created > 0 || Object.values(results).some((result) => result.status === 'uncertain');
  const allIncluded = units.every((unit) => !excluded.has(unit.context.unit.id));
  return (
    <section className="bulk-panel" aria-labelledby={headingId}>
      <header className="bulk-panel-header">
        <h2 id={headingId} ref={heading} tabIndex={-1}>
          {t('monitorMissing')}
        </h2>
        <IconButton icon={IconX} aria-label={t('close')} disabled={running} onClick={() => close(changed)} />
      </header>
      <p className="bulk-panel-intro">{t('bulkMonitorIntro')}</p>
      {sourcesError !== undefined ? (
        <ErrorNotice error={sourcesError} />
      ) : !sources || (loading && !snapshot) ? (
        <Loading />
      ) : sources.length === 0 ? (
        <p className="notice">
          {t('addMonitorNeedsProwlarr')}{' '}
          <a href="#/settings?section=sources">{t('monitorsOpenSources')}</a>
        </p>
      ) : units.length === 0 ? (
        <p className="empty-inline">{t('bulkNothingMissing')}</p>
      ) : (
        <>
          <div className="bulk-options">
            {sources.length > 1 ? (
              <div className="filter-group">
                <span className="filter-label" aria-hidden="true">
                  {t('source')}
                </span>
                {sources.length <= 4 ? (
                  <SegmentedControl
                    label={t('source')}
                    value={sourceId ?? ''}
                    disabled={running}
                    options={sources.map((source) => ({ value: source.id, label: source.label }))}
                    onChange={setSource}
                  />
                ) : (
                  <select
                    aria-label={t('source')}
                    value={sourceId}
                    disabled={running}
                    onChange={(event) => setSource(event.target.value)}
                  >
                    {sources.map((source) => (
                      <option key={source.id} value={source.id}>
                        {source.label}
                      </option>
                    ))}
                  </select>
                )}
              </div>
            ) : (
              <p className="bulk-source">
                {t('source')}: <strong>{sources[0].label}</strong>
              </p>
            )}
            <div className="filter-group">
              <span className="filter-label" aria-hidden="true">
                {t('bulkInterval')}
              </span>
              <SegmentedControl
                label={t('bulkInterval')}
                value={interval}
                disabled={running}
                options={intervalChoices.map((choice) => ({
                  value: choice,
                  label: intervalLabel(Number(choice), locale),
                }))}
                onChange={setIntervalChoice}
              />
            </div>
          </div>
          <p className="bulk-policy">{t('bulkReviewOnly')}</p>
          {!complete && <p className="muted">{t('bulkPartial')}</p>}
          <label className="bulk-check bulk-select-all">
            <input
              type="checkbox"
              checked={allIncluded}
              disabled={running}
              onChange={() =>
                setExcluded(allIncluded ? new Set(units.map((unit) => unit.context.unit.id)) : new Set())
              }
            />
            <span>{t('bulkSelectAll').replace('{count}', String(units.length))}</span>
          </label>
          <ul className="bulk-list">
            {units.map((unit) => {
              const id = unit.context.unit.id;
              const result = results[id];
              const locked = running || finished(id);
              const query = queryOf(unit);
              const invalid = showInvalid && !excluded.has(id) && !finished(id) && !queryValid(query);
              const meta = [
                t(unit.context.unit.kind),
                formatUnitDate(unit.context.unit, locale),
                editions.size > 1 ? editionName(unit.context.edition, locale) : null,
              ]
                .filter(Boolean)
                .join(', ');
              return (
                <li key={id} className="bulk-row">
                  <label className="bulk-check">
                    <input
                      type="checkbox"
                      checked={!excluded.has(id) && !finished(id)}
                      disabled={locked}
                      onChange={(event) => {
                        const next = new Set(excluded);
                        if (event.target.checked) next.delete(id);
                        else next.add(id);
                        setExcluded(next);
                      }}
                    />
                    <span className="bulk-unit">
                      <strong>{unit.context.unit.label}</strong>
                      <span className="unit-meta">{meta}</span>
                    </span>
                  </label>
                  <input
                    className="bulk-query"
                    aria-label={t('bulkQueryFor').replace('{label}', unit.context.unit.label)}
                    aria-invalid={invalid || undefined}
                    aria-describedby={invalid ? domId('bulk', id, 'error') : undefined}
                    value={query}
                    maxLength={512}
                    disabled={locked}
                    onChange={(event) => setQueries((current) => ({ ...current, [id]: event.target.value }))}
                  />
                  <div className="bulk-result">
                    <ResultBadge result={result} done="bulkCreated" />
                    {invalid && (
                      <span className="bulk-message" id={domId('bulk', id, 'error')}>
                        {t('bulkQueryInvalid')}
                      </span>
                    )}
                    {result?.status === 'failed' && <span className="bulk-message">{result.message}</span>}
                    {result?.status === 'uncertain' && (
                      <span className="bulk-message">{t('bulkUncertainHint')}</span>
                    )}
                  </div>
                </li>
              );
            })}
          </ul>
          <p className="bulk-summary" role="status">
            {running
              ? t('bulkProgress').replace('{done}', String(progress.done)).replace('{total}', String(progress.total))
              : attempted
                ? [
                    t('bulkCreatedCount').replace('{count}', String(created)),
                    failedCount ? t('bulkFailedCount').replace('{count}', String(failedCount)) : '',
                  ]
                    .filter(Boolean)
                    .join(' ')
                : ''}
          </p>
          <div className="actions">
            {running ? (
              <Button onClick={() => (stop.current = true)}>{t('bulkStop')}</Button>
            ) : (
              <>
                <Button
                  variant="primary"
                  icon={IconBell}
                  disabled={!pending.length || !sourceId}
                  onClick={() => void run()}
                >
                  {(attempted ? t('bulkMonitorRemaining') : t('bulkMonitorCount')).replace(
                    '{count}',
                    String(pending.length),
                  )}
                </Button>
                <Button onClick={() => close(changed)}>{t(attempted ? 'bulkDone' : 'cancel')}</Button>
              </>
            )}
          </div>
        </>
      )}
      {(sources?.length === 0 || units.length === 0 || sourcesError !== undefined) && (
        <div className="actions">
          <Button onClick={() => close(false)}>{t('close')}</Button>
        </div>
      )}
    </section>
  );
}

/* Pauses (PATCH enabled=false) the caller's enabled monitors for this publication after confirmation. */
function StopMonitoring({ publicationId, close }: { publicationId: string; close: (changed: boolean) => void }) {
  const { t } = useI18n();
  const headingId = useId();
  const { heading, alive } = usePanelFocus();
  const [version, setVersion] = useState(0);
  const [list, setList] = useState<{ items?: Schema['MonitorView'][]; error?: unknown }>({});
  const [results, setResults] = useState<Record<string, RowResult>>({});
  const [running, setRunning] = useState(false);
  const [progress, setProgress] = useState({ done: 0, total: 0 });
  const stop = useRef(false);
  const busy = useRef(false);
  useEffect(() => {
    const controller = new AbortController();
    (async () => {
      const items: Schema['MonitorView'][] = [];
      let cursor: string | null = null;
      for (let page = 0; page < 10; page++) {
        const params = new URLSearchParams({ publication_id: publicationId, enabled: 'true', limit: '100' });
        if (cursor) params.set('cursor', cursor);
        const result: Schema['MonitorPage'] = await request<Schema['MonitorPage']>(`/monitors?${params}`, {
          signal: controller.signal,
        });
        items.push(...result.items.filter((item) => item.enabled));
        cursor = result.next_cursor;
        if (!cursor) break;
      }
      if (!controller.signal.aborted) setList({ items });
    })().catch((error) => {
      if (!controller.signal.aborted) setList({ error });
    });
    return () => controller.abort();
  }, [publicationId, version]);
  const items = list.items ?? [];
  const pending = items.filter((item) => results[item.id]?.status !== 'done');
  const paused = Object.values(results).filter((result) => result.status === 'done').length;
  const failedItems = items.filter((item) => {
    const status = results[item.id]?.status;
    return status === 'failed' || status === 'uncertain';
  });
  const attempted = Object.keys(results).length;
  const unitCount = new Set(items.map((item) => item.unit_id)).size;
  async function run() {
    if (busy.current) return;
    busy.current = true;
    stop.current = false;
    setRunning(true);
    setProgress({ done: 0, total: pending.length });
    for (const monitor of pending) {
      if (stop.current || !alive.current) break;
      setResults((current) => ({ ...current, [monitor.id]: { status: 'running' } }));
      let result: RowResult;
      try {
        await request<Schema['MonitorView']>(`/monitors/${encodeURIComponent(monitor.id)}`, {
          method: 'PATCH',
          body: JSON.stringify({ revision: monitor.revision, enabled: false } satisfies Schema['UpdateMonitor']),
        });
        result = { status: 'done' };
      } catch (error) {
        result =
          error instanceof ApiError && error.status === 409
            ? { status: 'failed', message: t('bulkChangedElsewhere') }
            : failure(t, error);
      }
      if (!alive.current) break;
      setResults((current) => ({ ...current, [monitor.id]: result }));
      setProgress((current) => ({ ...current, done: current.done + 1 }));
    }
    busy.current = false;
    if (alive.current) setRunning(false);
  }
  const changed = paused > 0 || failedItems.length > 0;
  return (
    <section className="bulk-panel" aria-labelledby={headingId}>
      <header className="bulk-panel-header">
        <h2 id={headingId} ref={heading} tabIndex={-1}>
          {t('stopMonitoring')}
        </h2>
        <IconButton icon={IconX} aria-label={t('close')} disabled={running} onClick={() => close(changed)} />
      </header>
      {list.error !== undefined ? (
        <ErrorNotice error={list.error} retry={() => setVersion((value) => value + 1)} />
      ) : !list.items ? (
        <Loading />
      ) : items.length === 0 ? (
        <p className="empty-inline">{t('stopNothing')}</p>
      ) : (
        <>
          {!attempted && (
            <p className="bulk-panel-intro">
              {t('stopConfirm')
                .replace('{count}', String(items.length))
                .replace('{units}', String(unitCount))}
            </p>
          )}
          {failedItems.length > 0 && !running && (
            <ul className="bulk-list">
              {failedItems.map((item) => {
                const result = results[item.id];
                return (
                  <li key={item.id} className="bulk-row stop-row">
                    <span className="bulk-unit">
                      <strong>{item.target.unit.label}</strong>
                      <span className="unit-meta">{item.integration_label}</span>
                    </span>
                    <div className="bulk-result">
                      <ResultBadge result={result} done="bulkPaused" />
                      {result?.status === 'failed' && <span className="bulk-message">{result.message}</span>}
                      {result?.status === 'uncertain' && (
                        <span className="bulk-message">{t('bulkUncertainHint')}</span>
                      )}
                    </div>
                  </li>
                );
              })}
            </ul>
          )}
          <p className="bulk-summary" role="status">
            {running
              ? t('bulkProgress').replace('{done}', String(progress.done)).replace('{total}', String(progress.total))
              : attempted
                ? [
                    t('stopPausedCount').replace('{count}', String(paused)),
                    failedItems.length ? t('bulkFailedCount').replace('{count}', String(failedItems.length)) : '',
                  ]
                    .filter(Boolean)
                    .join(' ')
                : ''}
          </p>
        </>
      )}
      <div className="actions">
        {running ? (
          <Button onClick={() => (stop.current = true)}>{t('bulkStop')}</Button>
        ) : (
          <>
            {pending.length > 0 && !list.error && (
              <Button variant="danger" icon={IconBellOff} onClick={() => void run()}>
                {(attempted ? t('stopRemaining') : t('stopCount')).replace('{count}', String(pending.length))}
              </Button>
            )}
            <Button onClick={() => close(changed)}>{t(attempted ? 'bulkDone' : 'cancel')}</Button>
          </>
        )}
      </div>
    </section>
  );
}

export function PublicationDetail({
  id,
  query,
  canManage,
}: {
  id: string;
  query: URLSearchParams;
  canManage: boolean;
}) {
  const { t, locale } = useI18n();
  const publication = useResource<Schema['PublicationSummary']>(`/publications/${encodeURIComponent(id)}`);
  const wanted = useWantedUnits(id);
  const sources = useProwlarrSources(canManage);
  const [panel, setPanel] = useState<'monitor' | 'stop'>();
  const [monitorsChanged, setMonitorsChanged] = useState(false);
  const editions = usePagedList<Schema['Edition']>(`/publications/${encodeURIComponent(id)}/editions?limit=50`);
  const [editing, setEditing] = useState(false);
  const [saved, setSaved] = useState(false);
  const [adding, setAdding] = useState(false);
  const [editingEdition, setEditingEdition] = useState(false);
  const [editionSaved, setEditionSaved] = useState(false);
  const [selected, setSelected] = useState<string>();
  const [retainedEdition, setRetainedEdition] = useState<Schema['Edition']>();
  const [editionRefreshStarted, setEditionRefreshStarted] = useState(false);
  const [unitView, setUnitView] = useState<'volume' | 'chapter'>();
  const visibleEditions = retainedEdition
    ? [retainedEdition, ...editions.items.filter((edition) => edition.id !== retainedEdition.id)]
    : editions.items;
  useEffect(() => {
    if (retainedEdition && editions.loading) {
      setEditionRefreshStarted(true);
    } else if (
      retainedEdition &&
      editionRefreshStarted &&
      !editions.loading &&
      editions.items.some(
        (edition) =>
          edition.id === retainedEdition.id &&
          edition.language === retainedEdition.language &&
          edition.region === retainedEdition.region &&
          edition.publisher === retainedEdition.publisher,
      )
    ) {
      setRetainedEdition(undefined);
    }
  }, [editionRefreshStarted, editions.loading, editions.items, retainedEdition]);
  const title = publication.data?.title;
  useEffect(() => {
    if (title) document.title = `${title} | ${t('app')}`;
  }, [title, t]);
  const activeEdition =
    visibleEditions.find((edition) => edition.id === selected) ?? visibleEditions[0];
  if (publication.loading) return <Loading />;
  if (publication.error)
    return <ErrorNotice error={publication.error} retry={publication.reload} />;
  if (!publication.data) return null;
  const item = publication.data;
  const backQuery = new URLSearchParams(query);
  backQuery.delete('limit');
  const encodedId = encodeURIComponent(id);
  const identity = [
    t(typeNames[item.content_type]),
    item.run_label,
    activeEdition && editionName(activeEdition, locale),
    activeEdition?.publisher,
  ]
    .filter(Boolean)
    .join(', ');
  /* After bulk changes the Wanted feed is reloaded; its per-unit counts keep the header current without a page reload. */
  const monitored =
    monitorsChanged && wanted.complete && !wanted.error
      ? wanted.items.filter((unit) => unit.enabled_monitor_count > 0).length
      : item.monitored_unit_count;
  function closePanel(changed: boolean) {
    setPanel(undefined);
    if (changed) {
      setMonitorsChanged(true);
      wanted.reload();
    }
    requestAnimationFrame(() =>
      document.querySelector<HTMLElement>('.publication-monitoring [aria-haspopup="menu"]')?.focus(),
    );
  }
  const total = wanted.complete && !wanted.error ? wanted.items.length : null;
  const monitoringLabel =
    monitored === 0
      ? t('statusNotMonitored')
      : total
        ? t('monitoringOf').replace('{count}', String(monitored)).replace('{total}', String(total))
        : t('monitoringCount').replace('{count}', String(monitored));
  const matchingSources = sources.items?.filter((source) => source.content_types.includes(item.content_type));
  const monitoringItems: MenuItem[] = [
    ...(canManage
      ? [
          matchingSources && matchingSources.length === 0
            ? { label: t('monitorNeedsSource'), icon: IconPlugConnected, href: '#/settings?section=sources' }
            : { label: t('monitorMissing'), icon: IconBell, onSelect: () => setPanel('monitor') },
          ...(monitored > 0
            ? [{ label: t('stopMonitoring'), icon: IconBellOff, onSelect: () => setPanel('stop') }]
            : []),
        ]
      : []),
    {
      label: t('showInWanted'),
      icon: IconListSearch,
      href: `#/wanted?publication_id=${encodedId}&monitoring=all&availability=attention`,
    },
  ];
  const labels = editionLabels(visibleEditions, locale);
  const editionItems: MenuItem[] = canManage && !adding && !editingEdition
    ? [
        {
          label: t('addEdition'),
          icon: IconPlus,
          onSelect: () => {
            setAdding(true);
            setEditingEdition(false);
          },
        },
        ...(activeEdition
          ? [
              {
                label: t('editEdition'),
                icon: IconPencil,
                onSelect: () => {
                  setEditingEdition(true);
                  setAdding(false);
                  setEditionSaved(false);
                },
              },
            ]
          : []),
      ]
    : [];
  function chooseEdition(editionId: string) {
    setSelected(editionId);
    setEditingEdition(false);
  }
  /* With one edition its actions join the publication menu instead of a separate icon-only menu. */
  const multiEdition = visibleEditions.length > 1 || !!editions.next;
  const singleEdition = !multiEdition && !editions.error && !(editions.loading && !retainedEdition);
  const publicationItems: MenuItem[] = [
    ...(editing
      ? []
      : [
          {
            label: t('editPublication'),
            icon: IconPencil,
            onSelect: () => {
              setEditing(true);
              setSaved(false);
            },
          },
        ]),
    ...(singleEdition ? editionItems : []),
  ];
  /* A manga view filtered to volumes or chapters that shows no files says where the files are. */
  const hasFilesIn = (kind: 'volume' | 'chapter') =>
    wanted.items.some(
      (unit) =>
        unit.context.edition.id === activeEdition?.id &&
        unit.context.unit.kind === kind &&
        unit.association_status === 'associated',
    );
  const otherView = unitView === 'volume' ? 'chapter' : 'volume';
  const filesElsewhere =
    !!unitView && wanted.complete && !wanted.error && !hasFilesIn(unitView) && hasFilesIn(otherView);
  const availability = availabilityText(
    t,
    locale,
    item.file_count,
    item.known_unit_count,
    filesElsewhere ? t(otherView === 'volume' ? 'volumes' : 'chapters') : undefined,
  );
  return (
    <>
      <a className="back" href={`#/?${backQuery}`}>
        <Icon icon={IconChevronLeft} size={18} />
        {t('back')}
      </a>
      <header className="publication-header">
        <Cover
          fileId={item.cover_file_id}
          title={item.title}
          contentType={item.content_type}
          width={120}
        />
        <div className="publication-header-body">
          <div className="publication-heading">
            <h1>{item.title}</h1>
            {canManage && publicationItems.length > 0 && (
              <Menu label={t('publicationActions')} items={publicationItems} />
            )}
          </div>
          <p className="publication-identity">{identity}</p>
          <p className="publication-availability">
            {availability}
            {item.title_locked && <span className="muted">{t('protectedTitle')}</span>}
          </p>
          {item.cover_file_id && (
            <a className="button primary publication-read" href={readerHref(item.cover_file_id)}>
              <Icon icon={IconBook} size={18} />
              {t('startReading')}
            </a>
          )}
        </div>
        <div className="publication-controls">
          <div className={`publication-monitoring${monitored > 0 ? ' on' : ''}`}>
            <Menu
              label={monitoringLabel}
              items={monitoringItems}
              renderTrigger={
                <>
                  <Icon icon={monitored > 0 ? IconBell : IconBellOff} size={18} />
                  {monitoringLabel}
                  <Icon icon={IconChevronDown} size={16} />
                </>
              }
            />
            <HelpTip label={t('aboutMonitoring')}>{t('monitoringHelp')}</HelpTip>
          </div>
          {!(singleEdition && visibleEditions.length === 1) && (
            <div className="edition-bar">
              <div className="edition-group">
              {editions.loading && !retainedEdition ? (
                <Skeleton variant="line" short />
              ) : editions.error ? (
                <ErrorNotice error={editions.error} retry={editions.reload} />
              ) : visibleEditions.length === 0 ? (
                <p className="empty-inline">{t('noEditions')}</p>
              ) : visibleEditions.length === 1 ? null : visibleEditions.length <= 6 ? (
                <SegmentedControl
                  label={t('edition')}
                  value={activeEdition?.id ?? ''}
                  options={visibleEditions.map((edition, index) => ({ value: edition.id, label: labels[index] }))}
                  onChange={chooseEdition}
                />
              ) : (
                <label className="edition-select">
                  <span>{t('edition')}</span>
                  <select value={activeEdition?.id} onChange={(event) => chooseEdition(event.target.value)}>
                    {visibleEditions.map((edition, index) => (
                      <option key={edition.id} value={edition.id}>
                        {labels[index]}
                        {edition.publisher && !labels[index].includes(edition.publisher) ? `, ${edition.publisher}` : ''}
                      </option>
                    ))}
                  </select>
                </label>
              )}
              {editionItems.length > 0 && multiEdition && (
                <Menu
                  label={t('editionActions')}
                  items={editionItems}
                  renderTrigger={
                    <>
                      {t('editionActions')}
                      <Icon icon={IconChevronDown} size={16} />
                    </>
                  }
                />
              )}
              {visibleEditions.length > 1 && <HelpTip label={t('aboutEditions')}>{t('editionHelp')}</HelpTip>}
              </div>
              {editions.next && (
                <Button
                  size="sm"
                  disabled={editions.loadingMore}
                  aria-busy={editions.loadingMore}
                  onClick={editions.loadMore}
                >
                  {t(editions.loadingMore ? 'loading' : 'loadMore')}
                </Button>
              )}
            </div>
          )}
        </div>
      </header>
      {panel === 'monitor' && canManage && (
        <BulkMonitor
          publication={item}
          candidates={wanted.items.filter(
            (unit) => unitStatus(unit).kind === 'missing' && unit.monitor_count === 0,
          )}
          loading={wanted.loading}
          complete={wanted.complete}
          sources={matchingSources}
          sourcesError={sources.error}
          close={(changed) => {
            closePanel(changed);
          }}
        />
      )}
      {panel === 'stop' && canManage && (
        <StopMonitoring
          publicationId={id}
          close={(changed) => {
            closePanel(changed);
          }}
        />
      )}
      {saved && (
        <p className="notice" role="status">
          {t('publicationSaved')}
        </p>
      )}
      {editing && canManage && (
        <SaveForm
          cancel={() => setEditing(false)}
          submit={async (data) => {
            const input: Schema['PublicationUpdate'] = {};
            const title = value(data, 'title');
            const sortTitle = value(data, 'sort_title');
            const runLabel = optional(data, 'run_label');
            const count = value(data, 'known_unit_count')
              ? Number(value(data, 'known_unit_count'))
              : null;
            if (title !== item.title) input.title = title;
            if (sortTitle !== item.sort_title) input.sort_title = sortTitle;
            if (runLabel !== item.run_label) input.run_label = runLabel;
            if (count !== item.known_unit_count) input.known_unit_count = count;
            if (Object.keys(input).length) {
              await request<Schema['Publication']>(`/publications/${encodedId}`, {
                method: 'PATCH',
                body: JSON.stringify(input),
              });
              publication.reload();
            }
            setEditing(false);
            setSaved(true);
          }}
        >
          <Field
            label={t('title')}
            name="title"
            defaultValue={item.title}
            required
            maxLength={1000}
            autoFocus
          />
          <Field
            label={t('sortTitle')}
            name="sort_title"
            defaultValue={item.sort_title}
            required
            maxLength={1000}
          />
          <Field
            label={t('runLabel')}
            name="run_label"
            defaultValue={item.run_label ?? ''}
            maxLength={1000}
          />
          <Field
            label={t('knownCount')}
            name="known_unit_count"
            defaultValue={item.known_unit_count ?? ''}
            type="number"
            min="0"
            step="1"
            max={Number.MAX_SAFE_INTEGER}
            hint={t('countHint')}
          />
          <p className="muted">{t('editPublicationHint')}</p>
        </SaveForm>
      )}
      {editions.moreError !== undefined && <ErrorNotice error={editions.moreError} retry={editions.loadMore} />}
      {editionSaved && (
        <p className="notice" role="status">
          {t('editionSaved')}
        </p>
      )}
      {adding && canManage && (
        <SaveForm
          label={t('addEdition')}
          cancel={() => setAdding(false)}
          submit={async (data) => {
            const input: Schema['NewEdition'] = {
              publication_id: id,
              language: value(data, 'language'),
              region: optional(data, 'region'),
              publisher: optional(data, 'publisher'),
            };
            const created = await post<Schema['Edition']>('/editions', input);
            setAdding(false);
            setSelected(created.id);
            setRetainedEdition(created);
            setEditionRefreshStarted(false);
            editions.reload();
          }}
        >
          <Field
            name="language"
            label={t('language')}
            hint={t('languageHint')}
            required
            autoFocus
            maxLength={100}
          />
          <Field name="region" label={t('region')} maxLength={100} />
          <Field name="publisher" label={t('publisher')} maxLength={1000} />
        </SaveForm>
      )}
      {editingEdition && activeEdition && canManage && (
        <SaveForm
          key={activeEdition.id}
          label={t('editEdition')}
          cancel={() => setEditingEdition(false)}
          submit={async (data) => {
            const input: Schema['EditionUpdate'] = {};
            const language = value(data, 'language');
            const region = optional(data, 'region');
            const publisher = optional(data, 'publisher');
            if (language !== activeEdition.language) input.language = language;
            if (region !== activeEdition.region) input.region = region;
            if (publisher !== activeEdition.publisher) input.publisher = publisher;
            if (Object.keys(input).length) {
              const updated = await request<Schema['Edition']>(
                `/editions/${encodeURIComponent(activeEdition.id)}`,
                { method: 'PATCH', body: JSON.stringify(input) },
              );
              setSelected(updated.id);
              setRetainedEdition(updated);
              setEditionRefreshStarted(false);
              editions.reload();
            }
            setEditingEdition(false);
            setEditionSaved(true);
          }}
        >
          <Field
            name="language"
            label={t('language')}
            hint={t('languageHint')}
            defaultValue={activeEdition.language}
            required
            autoFocus
            maxLength={100}
          />
          <Field name="region" label={t('region')} defaultValue={activeEdition.region ?? ''} maxLength={100} />
          <Field
            name="publisher"
            label={t('publisher')}
            defaultValue={activeEdition.publisher ?? ''}
            maxLength={1000}
          />
          <p className="muted">{t('editEditionHint')}</p>
        </SaveForm>
      )}
      {activeEdition && (
        <Units
          key={activeEdition.id}
          edition={activeEdition}
          publicationId={id}
          contentType={item.content_type}
          canManage={canManage}
          wanted={wanted}
          onViewKind={setUnitView}
        />
      )}
    </>
  );
}

type Group = 'volume' | 'chapter' | 'all';
const kinds = ['issue', 'chapter', 'volume', 'special', 'combined'] as const;
function Units({
  publicationId,
  edition,
  contentType,
  canManage,
  wanted,
  onViewKind,
}: {
  edition: Schema['Edition'];
  publicationId: string;
  contentType: ContentType;
  canManage: boolean;
  wanted: ReturnType<typeof useWantedUnits>;
  onViewKind: (kind: 'volume' | 'chapter' | undefined) => void;
}) {
  const { t, locale } = useI18n();
  const [adding, setAdding] = useState(false);
  const [editingUnit, setEditingUnit] = useState<string>();
  const [unitSaved, setUnitSaved] = useState(false);
  const [retainedUnit, setRetainedUnit] = useState<Schema['Unit']>();
  const [unitRefreshStarted, setUnitRefreshStarted] = useState(false);
  const [openFiles, setOpenFiles] = useState<ReadonlySet<string>>(new Set());
  const [chosenGroup, setGroup] = useState<Group>();
  const [q, setQuery] = useState('');
  /* Manga editions mix volumes and chapters: when both kinds exist, the server filters by the chosen group. */
  const unitsPath = `/editions/${encodeURIComponent(edition.id)}/units`;
  const manga = contentType === 'manga';
  const addLabel = t(manga ? 'addVolumeOrChapter' : 'addIssue');
  const volumeProbe = useResource<Schema['UnitPage']>(manga ? `${unitsPath}?limit=1&kind=volume` : null);
  const chapterProbe = useResource<Schema['UnitPage']>(manga ? `${unitsPath}?limit=1&kind=chapter` : null);
  const probing = volumeProbe.loading || chapterProbe.loading;
  const grouped = manga && !!volumeProbe.data?.items.length && !!chapterProbe.data?.items.length;
  /* Until the reader picks a group, show the kind that has files (volumes when both or neither do). */
  const filesIn = (kind: 'volume' | 'chapter') =>
    wanted.items.some(
      (unit) =>
        unit.context.edition.id === edition.id &&
        unit.context.unit.kind === kind &&
        unit.association_status === 'associated',
    );
  const group: Group = chosenGroup ?? (!filesIn('volume') && filesIn('chapter') ? 'chapter' : 'volume');
  const waitingForDefault = grouped && !chosenGroup && wanted.loading && !wanted.items.length;
  const kindFilter = grouped && group !== 'all' ? group : undefined;
  const params = new URLSearchParams({ limit: '100' });
  if (q) params.set('q', q);
  if (kindFilter) params.set('kind', kindFilter);
  const result = usePagedList<Schema['Unit']>(probing || waitingForDefault ? null : `${unitsPath}?${params}`);
  const rowsRef = useRef<HTMLUListElement>(null);
  useRowKeys(rowsRef, ['f']);
  const listed = retainedUnit
    ? [retainedUnit, ...result.items.filter((unit) => unit.id !== retainedUnit.id)]
    : result.items;
  const shownUnits = kindFilter ? listed.filter((unit) => unit.kind === kindFilter) : listed;
  useEffect(() => onViewKind(kindFilter), [kindFilter, onViewKind]);
  useEffect(() => {
    if (retainedUnit && result.loading) {
      setUnitRefreshStarted(true);
    } else if (
      retainedUnit &&
      unitRefreshStarted &&
      !result.loading &&
      result.items.some(
        (unit) =>
          unit.id === retainedUnit.id &&
          unit.label === retainedUnit.label &&
          unit.kind === retainedUnit.kind &&
          unit.sort_key === retainedUnit.sort_key &&
          unit.date === retainedUnit.date &&
          unit.date_precision === retainedUnit.date_precision,
      )
    ) {
      setRetainedUnit(undefined);
    }
  }, [result.items, result.loading, retainedUnit, unitRefreshStarted]);
  function refresh() {
    setUnitRefreshStarted(false);
    volumeProbe.reload();
    chapterProbe.reload();
    result.reload();
  }
  function toggleFiles(unitId: string, open?: boolean) {
    setOpenFiles((current) => {
      const next = new Set(current);
      if (open ?? !next.has(unitId)) next.add(unitId);
      else next.delete(unitId);
      return next;
    });
  }
  function resetPaging() {
    setEditingUnit(undefined);
    setRetainedUnit(undefined);
    setUnitRefreshStarted(false);
  }
  return (
    <section className="units">
      <header className="section-header">
        <h2>{t(manga ? 'unitsVolumesChapters' : 'unitsIssues')}</h2>
        {canManage && !adding && !editingUnit && (
          <Button size="sm" icon={IconPlus} onClick={() => setAdding(true)}>
            {addLabel}
          </Button>
        )}
      </header>
      <div className="units-toolbar">
        <SearchField
          label={t('searchUnits')}
          value={q}
          onChange={(search) => {
            setQuery(search.trim());
            resetPaging();
            setUnitSaved(false);
          }}
        />
        {grouped && (
          <SegmentedControl<Group>
            label={t('unitGroup')}
            value={group}
            options={[
              { value: 'volume', label: t('volumes') },
              { value: 'chapter', label: t('chapters') },
              { value: 'all', label: t('all') },
            ]}
            onChange={(next) => {
              setGroup(next);
              resetPaging();
            }}
          />
        )}
      </div>
      {adding && (
        <SaveForm
          label={addLabel}
          cancel={() => setAdding(false)}
          submit={async (data) => {
            const input: Schema['NewUnit'] = {
              edition_id: edition.id,
              label: value(data, 'label'),
              kind: kinds.find((kind) => kind === value(data, 'kind')) ?? 'issue',
              sort_key: optional(data, 'sort_key'),
              date: optional(data, 'date'),
            };
            const created = await post<Schema['Unit']>('/units', input);
            setQuery('');
            setAdding(false);
            setRetainedUnit(created);
            refresh();
            wanted.reload();
          }}
        >
          <Field name="label" label={t('unitLabel')} required autoFocus maxLength={1000} />
          <label className="field">
            <span>{t('unitKind')} *</span>
            <select name="kind" defaultValue={contentType === 'manga' ? 'chapter' : 'issue'}>
              {kinds.map((kind) => (
                <option key={kind} value={kind}>
                  {t(kind)}
                </option>
              ))}
            </select>
          </label>
          <Field
            name="date"
            label={t('date')}
            hint={t('dateHint')}
            pattern="[0-9]{4}(-[0-9]{2}(-[0-9]{2})?)?"
          />
          <Field name="sort_key" label={t('sortKey')} maxLength={1000} />
        </SaveForm>
      )}
      {wanted.error !== undefined && (
        <ErrorNotice error={wanted.error} retry={wanted.reload} context={t('unitStatusUnavailable')} />
      )}
      {result.loading || probing || waitingForDefault ? (
        <Loading />
      ) : result.error ? (
        <ErrorNotice error={result.error} retry={refresh} />
      ) : (
        <>
          {shownUnits.length ? (
            <ul className="unit-rows" ref={rowsRef}>
              {shownUnits.map((unit) => {
                const wantedUnit = wanted.byUnit.get(unit.id);
                const state = wantedUnit && unitStatus(wantedUnit);
                const findHref = `#/search?unit=${encodeURIComponent(unit.id)}`;
                const monitored = (wanted.byUnit.get(unit.id)?.enabled_monitor_count ?? 0) > 0;
                const meta = [
                  formatUnitDate(unit, locale),
                  unit.kind === 'special' || unit.kind === 'combined' ? t(unit.kind) : null,
                  monitored ? t('statusMonitored') : null,
                ]
                  .filter(Boolean)
                  .join(', ');
                const filesOpen = openFiles.has(unit.id);
                const menuItems: MenuItem[] = [
                  ...(canManage && editingUnit !== unit.id
                    ? [
                        {
                          label: t('editUnit'),
                          icon: IconPencil,
                          onSelect: () => {
                            setEditingUnit(unit.id);
                            setUnitSaved(false);
                          },
                        },
                      ]
                    : []),
                  {
                    label: filesOpen ? t('hideFiles') : t('files'),
                    icon: IconFiles,
                    onSelect: () => toggleFiles(unit.id),
                  },
                  ...(canManage
                    ? [
                        {
                          label: t('monitorUnit'),
                          icon: IconBell,
                          href: `#/monitors?publication_id=${encodeURIComponent(publicationId)}&unit_id=${encodeURIComponent(unit.id)}`,
                        },
                      ]
                    : []),
                ];
                return (
                  <li
                    key={unit.id}
                    className="unit-row"
                    data-row
                    data-row-f={canManage ? findHref : undefined}
                    tabIndex={-1}
                  >
                    <div className="unit-main">
                      <strong>{unit.label}</strong>
                      {meta && <span className="unit-meta">{meta}</span>}
                    </div>
                    <div className="unit-status">
                      {state ? (
                        <StatusBadge kind={state.kind} label={state.label && t(state.label)} />
                      ) : (
                        wanted.loading && <Skeleton variant="line" short />
                      )}
                    </div>
                    <div className="unit-actions">
                      {state?.readable ? (
                        <ReadButton
                          unitId={unit.id}
                          showFiles={() => toggleFiles(unit.id, true)}
                        />
                      ) : (
                        canManage &&
                        !(wanted.loading && !state) && (
                          <a className="button ghost sm" href={findHref}>
                            <Icon icon={IconSearch} size={16} />
                            {t('findReleases')}
                          </a>
                        )
                      )}
                      <Menu label={t('unitActions').replace('{label}', unit.label)} items={menuItems} />
                    </div>
                    {editingUnit === unit.id && canManage && (
                      <div className="unit-extra">
                        <SaveForm
                          key={unit.id}
                          label={t('editUnit')}
                          cancel={() => setEditingUnit(undefined)}
                          submit={async (data) => {
                            const input: Schema['UnitUpdate'] = {};
                            const label = value(data, 'label');
                            const kind = kinds.find((kind) => kind === value(data, 'kind')) ?? unit.kind;
                            const date = optional(data, 'date');
                            const sortKey = optional(data, 'sort_key');
                            if (label !== unit.label) input.label = label;
                            if (kind !== unit.kind) input.kind = kind;
                            if (date !== unit.date) input.date = date;
                            if (sortKey !== unit.sort_key) input.sort_key = sortKey;
                            if (Object.keys(input).length) {
                              const updated = await request<Schema['Unit']>(
                                `/units/${encodeURIComponent(unit.id)}`,
                                { method: 'PATCH', body: JSON.stringify(input) },
                              );
                              setQuery('');
                              setRetainedUnit(updated);
                              refresh();
                            }
                            setEditingUnit(undefined);
                            setUnitSaved(true);
                          }}
                        >
                          <Field
                            name="label"
                            label={t('unitLabel')}
                            defaultValue={unit.label}
                            required
                            autoFocus
                            maxLength={1000}
                          />
                          <label className="field">
                            <span>{t('unitKind')} *</span>
                            <select name="kind" defaultValue={unit.kind}>
                              {kinds.map((kind) => (
                                <option key={kind} value={kind}>
                                  {t(kind)}
                                </option>
                              ))}
                            </select>
                          </label>
                          <Field
                            name="date"
                            label={t('date')}
                            hint={t('dateHint')}
                            defaultValue={unit.date ?? ''}
                            pattern="[0-9]{4}(-[0-9]{2}(-[0-9]{2})?)?"
                          />
                          <Field
                            name="sort_key"
                            label={t('sortKey')}
                            defaultValue={unit.sort_key ?? ''}
                            maxLength={1000}
                          />
                          <p className="muted">{t('editUnitHint')}</p>
                        </SaveForm>
                      </div>
                    )}
                    {filesOpen && (
                      <div className="unit-extra unit-files">
                        <FileList unitId={unit.id} />
                      </div>
                    )}
                  </li>
                );
              })}
            </ul>
          ) : (
            <p className="empty-inline">{t(q ? 'noSearchResults' : 'noUnits')}</p>
          )}
          {result.moreError !== undefined && <ErrorNotice error={result.moreError} retry={result.loadMore} />}
          {result.next && (
            <div className="actions">
              <Button
                size="sm"
                disabled={result.loadingMore}
                aria-busy={result.loadingMore}
                onClick={result.loadMore}
              >
                {t(result.loadingMore ? 'loading' : 'loadMore')}
              </Button>
            </div>
          )}
        </>
      )}
      {unitSaved && (
        <p className="notice" role="status">
          {t('unitSaved')}
        </p>
      )}
    </section>
  );
}
