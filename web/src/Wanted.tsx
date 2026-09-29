import { useEffect, useRef, useState, type ReactNode } from 'react';
import {
  IconAdjustmentsHorizontal,
  IconBell,
  IconChevronDown,
  IconChevronUp,
  IconPlugConnected,
  IconSearch,
  IconX,
} from '@tabler/icons-react';
import { ApiError, type Schema, post, request } from './lib/api/client';
import { Cover } from './Cover';
import { editionLabel, editionName, formatUnitDate } from './format';
import { useI18n, type Locale, type MessageKey } from './i18n';
import {
  Button,
  EmptyState,
  domId,
  ErrorNotice,
  Icon,
  IconButton,
  Loading,
  Menu,
  PageHeader,
  SearchField,
  SegmentedControl,
  StatusBadge,
  useRowKeys,
} from './ui';
import { usePolled } from './Activity';
import { unitStatus } from './status';
import './styles/wanted.css';

type WantedUnit = Schema['WantedUnit'];
type Totals = Schema['WantedPublicationTotals'];
const contentTypes = ['all', 'comic', 'manga', 'magazine'] as const;
const monitoringOptions = ['all', 'monitored', 'unmonitored'] as const;
const availabilityOptions = ['attention', 'all'] as const;
type Kind = (typeof contentTypes)[number];
type Monitoring = (typeof monitoringOptions)[number];
type Availability = (typeof availabilityOptions)[number];
type Filters = {
  kind: Kind;
  publicationId: string;
  monitoring: Monitoring;
  availability: Availability;
  q: string;
  cursor?: string | null;
};
const monitoringLabels: Record<Monitoring, MessageKey> = {
  all: 'all',
  monitored: 'wantedMonitoringMonitored',
  unmonitored: 'wantedMonitoringUnmonitored',
};
const availabilityLabels: Record<Availability, MessageKey> = {
  attention: 'wantedNeedsAttentionAfterScan',
  all: 'all',
};

function wantedHref(filters: Filters) {
  const target = new URLSearchParams();
  if (filters.kind !== 'all') target.set('kind', filters.kind);
  if (filters.publicationId) target.set('publication_id', filters.publicationId);
  target.set('monitoring', filters.monitoring);
  target.set('availability', filters.availability);
  if (filters.q) target.set('q', filters.q);
  if (filters.cursor) target.set('cursor', filters.cursor);
  return `#/wanted?${target}`;
}
function replaceHash(hash: string) {
  history.replaceState(history.state, '', hash);
  window.dispatchEvent(new HashChangeEvent('hashchange'));
}


/* Kind and date only when the label does not already say them ("Volume 1", "September 2026"). */
function unitMeta(unit: Schema['Unit'], kind: string, locale: Locale, edition?: Schema['Edition']) {
  const label = unit.label.toLowerCase();
  const date = formatUnitDate(unit, locale);
  return [
    label.includes(kind.toLowerCase()) ? undefined : kind,
    date && !label.includes(date.toLowerCase()) ? date : undefined,
    edition ? editionLabel(edition, locale) : undefined,
  ]
    .filter(Boolean)
    .join(', ');
}

const typeNames = { comic: 'typeComic', manga: 'typeManga', magazine: 'typeMagazine' } as const;

/* Manga lists volumes before chapters; everything else lists issues first. */
const kindOrder: Record<Schema['Publication']['content_type'], readonly Schema['Unit']['kind'][]> = {
  manga: ['volume', 'chapter', 'issue', 'combined', 'special'],
  comic: ['issue', 'volume', 'chapter', 'combined', 'special'],
  magazine: ['issue', 'volume', 'chapter', 'combined', 'special'],
};
const natural = new Intl.Collator(undefined, { numeric: true, sensitivity: 'base' });
function sortUnits(items: WantedUnit[], contentType: Schema['Publication']['content_type']) {
  const order = kindOrder[contentType];
  const editions = new Map<string, number>();
  for (const item of items) if (!editions.has(item.context.edition.id)) editions.set(item.context.edition.id, editions.size);
  return [...items].sort((a, b) => {
    const x = a.context.unit;
    const y = b.context.unit;
    return (
      (editions.get(a.context.edition.id) ?? 0) - (editions.get(b.context.edition.id) ?? 0) ||
      order.indexOf(x.kind) - order.indexOf(y.kind) ||
      (x.sort_key && y.sort_key ? natural.compare(x.sort_key, y.sort_key) : 0) ||
      natural.compare(x.label, y.label)
    );
  });
}

/* "#1-6, Annual": runs of three or more consecutive numbers collapse; dated issues are listed as dates,
   never ranges; long lists end in "and N more". `unlisted` counts matching units not loaded yet. */
const datedLabel = /^\d{4}-\d{2}(-\d{2})?$/;
type Segment = { text: string; count: number };
function labelSegments(labels: string[]) {
  const segments: Segment[] = [];
  let run: { prefix: string; end: number; labels: string[] } | undefined;
  const flush = () => {
    if (!run) return;
    if (run.labels.length >= 3)
      segments.push({ text: `${run.prefix}${run.end - run.labels.length + 1}-${run.end}`, count: run.labels.length });
    else for (const label of run.labels) segments.push({ text: label, count: 1 });
    run = undefined;
  };
  for (const label of labels) {
    const match = datedLabel.test(label.trim()) ? null : /^(.*?)(\d+)$/.exec(label.trim());
    const number = match ? Number(match[2]) : NaN;
    if (match && run && run.prefix === match[1] && number === run.end + 1) {
      run.end = number;
      run.labels.push(label);
      continue;
    }
    flush();
    if (match) run = { prefix: match[1], end: number, labels: [label] };
    else segments.push({ text: label, count: 1 });
  }
  flush();
  return segments;
}
/* With several editions each one's labels get its name ("English (US): Volume 1-5; Japanese (JP): 1-2");
   the limit and "and N more" apply across the combined list. */
function summarizeEditions(
  editions: { name?: string; labels: string[] }[],
  editionUnits: string,
  andMore: string,
  unlisted = 0,
  limit = 5,
) {
  let left = limit;
  let rest = unlisted;
  const parts: string[] = [];
  for (const edition of editions) {
    const segments = labelSegments(edition.labels);
    const shown = segments.slice(0, left);
    left -= shown.length;
    rest += segments.slice(shown.length).reduce((sum, segment) => sum + segment.count, 0);
    if (!shown.length) continue;
    const text = shown.map((segment) => segment.text).join(', ');
    parts.push(edition.name ? editionUnits.replace('{edition}', edition.name).replace('{units}', text) : text);
  }
  const list = parts.join('; ');
  return rest ? andMore.replace('{list}', list).replace('{count}', String(rest)) : list;
}

const needsFile = (item: WantedUnit) => item.availability !== 'confirmed_present';

function PublicationScope({ id, clear }: { id: string; clear: () => void }) {
  const { t } = useI18n();
  const publication = usePolled<Schema['Publication']>(`/publications/${encodeURIComponent(id)}`, false);
  return (
    <div className="wanted-scope">
      <span>
        {t('wantedOnly').replace('{title}', publication.data?.title ?? '...')}
      </span>
      <IconButton icon={IconX} size="sm" aria-label={t('wantedClearPublication')} onClick={clear} />
      <ErrorNotice error={publication.error} retry={publication.reload} />
    </div>
  );
}

export function Wanted({ query, canManage }: { query: URLSearchParams; canManage: boolean }) {
  const { t } = useI18n();
  const kind = contentTypes.find((value) => value === query.get('kind')) ?? 'all';
  const publicationId = query.get('publication_id')?.trim() ?? '';
  const requestedMonitoring = monitoringOptions.find((value) => value === query.get('monitoring'));
  const availability =
    availabilityOptions.find((value) => value === query.get('availability')) ?? 'attention';
  const q = query.get('q')?.trim() ?? '';
  const cursor = query.get('cursor');
  /* With no monitors at all, "Monitored" would always be empty, so the default becomes "All". */
  const probe = usePolled<Schema['MonitorPage']>(requestedMonitoring ? null : '/monitors?limit=1', false);
  const defaultMonitoring: Monitoring | undefined = probe.loading
    ? undefined
    : probe.data && probe.data.items.length === 0
      ? 'all'
      : 'monitored';
  const monitoring = requestedMonitoring ?? defaultMonitoring;
  const params = new URLSearchParams({ limit: '50', availability });
  if (monitoring) params.set('monitoring', monitoring);
  if (kind !== 'all') params.set('kind', kind);
  if (publicationId) params.set('publication_id', publicationId);
  if (q) params.set('q', q);
  if (cursor) params.set('cursor', cursor);
  const path = monitoring ? `/wanted?${params}` : null;
  const result = usePolled<Schema['WantedPage']>(path, false);
  /* Indexer (Prowlarr), direct, and archive sources all serve Find releases; monitors need an indexer. */
  const sources = usePolled<Schema['IntegrationChoiceList']>(canManage ? '/search/integrations' : null, false);
  const hasIndexer = sources.data?.items.some((item) => item.kind === 'prowlarr' && item.release_search);
  const noSource =
    !!sources.data &&
    !sources.data.items.some(
      (item) => item.release_search || item.kind === 'getcomics' || item.kind === 'internetarchive',
    );
  /* "Load more" appends pages for the current filters; a filter change (new path) starts over. */
  const [more, setMore] = useState<{
    path: string | null;
    items: WantedUnit[];
    totals: Totals[];
    pages: number;
    next: string | null;
    loading: boolean;
    error?: unknown;
  }>({ path: null, items: [], totals: [], pages: 0, next: null, loading: false });
  const extra = more.path === path ? more : undefined;
  const [expanded, setExpanded] = useState<ReadonlySet<string>>(() => new Set());
  const [filtersOpen, setFiltersOpen] = useState(false);
  /* Selection is per filter set: changing filters starts with nothing selected. */
  const [selection, setSelection] = useState<{ path: string | null; ids: ReadonlySet<string> }>({
    path: null,
    ids: new Set(),
  });
  const groupsRef = useRef<HTMLUListElement>(null);
  useRowKeys(groupsRef, ['f']);
  const nextCursor = extra?.pages ? extra.next : (result.data?.next_cursor ?? null);
  async function loadMore() {
    if (!path || !nextCursor || extra?.loading) return;
    const base = extra ?? { path, items: [], totals: [], pages: 0, next: nextCursor, loading: false };
    setMore({ ...base, next: nextCursor, loading: true, error: undefined });
    const pageParams = new URLSearchParams(params);
    pageParams.set('cursor', nextCursor);
    try {
      const page = await request<Schema['WantedPage']>(`/wanted?${pageParams}`);
      setMore((current) =>
        current.path === path
          ? {
              path,
              items: [...current.items, ...page.items],
              totals: [...current.totals, ...(page.publication_totals ?? [])],
              pages: current.pages + 1,
              next: page.next_cursor,
              loading: false,
            }
          : current,
      );
    } catch (error) {
      setMore((current) => (current.path === path ? { ...current, loading: false, error } : current));
    }
  }
  const seen = new Set<string>();
  const allItems = [...(result.data?.items ?? []), ...(extra?.items ?? [])].filter((item) => {
    if (seen.has(item.context.unit.id)) return false;
    seen.add(item.context.unit.id);
    return true;
  });
  /* Totals cover every matching unit of a publication, independent of pagination. */
  const totals = new Map<string, Totals>();
  for (const total of [...(result.data?.publication_totals ?? []), ...(extra?.totals ?? [])])
    totals.set(total.publication_id, total);
  const selected = new Set(selection.path === path ? [...selection.ids].filter((id) => seen.has(id)) : []);
  const setSelected = (ids: string[], checked: boolean) =>
    setSelection((current) => {
      const next = new Set(current.path === path ? current.ids : []);
      for (const id of ids) {
        if (checked) next.add(id);
        else next.delete(id);
      }
      return { path, ids: next };
    });
  const filters: Filters = {
    kind,
    publicationId,
    monitoring: monitoring ?? 'monitored',
    availability,
    q,
  };
  const update = (change: Partial<Filters>) => replaceHash(wantedHref({ ...filters, ...change, cursor: null }));

  /* Grouped by publication across loaded pages, in first-seen order. */
  const groupMap = new Map<string, { publication: Schema['Publication']; items: WantedUnit[] }>();
  for (const item of allItems) {
    const group = groupMap.get(item.context.publication.id);
    if (group) group.items.push(item);
    else groupMap.set(item.context.publication.id, { publication: item.context.publication, items: [item] });
  }
  const groups = [...groupMap.values()].map((group) => ({
    ...group,
    items: sortUnits(group.items, group.publication.content_type),
  }));
  const atDefaults =
    kind === 'all' && !publicationId && !q && availability === 'attention' && monitoring === defaultMonitoring;
  /* Monitoring and availability sit behind the Filters disclosure; content type stays visible. */
  const activeFilters = [
    !!monitoring && !!defaultMonitoring && monitoring !== defaultMonitoring,
    availability !== 'attention',
  ].filter(Boolean).length;
  const filterSummary = [
    monitoring && `${t('wantedMonitoring')}: ${monitoring === 'all' ? t('all') : t(monitoringLabels[monitoring])}`,
    `${t('wantedAvailability')}: ${availability === 'all' ? t('all') : t(availabilityLabels[availability])}`,
  ]
    .filter(Boolean)
    .join('; ');
  const toggle = (id: string) =>
    setExpanded((current) => {
      const next = new Set(current);
      if (next.has(id)) next.delete(id);
      else next.add(id);
      return next;
    });
  const runnable = (item: WantedUnit) => needsFile(item) && item.enabled_monitor_count > 0;
  const runTargets = groups.flatMap((group) => group.items.filter(runnable));
  const selectedTargets = allItems.filter((item) => selected.has(item.context.unit.id) && runnable(item));
  const filtersId = domId('wanted', 'filters');
  const filterSummaryId = domId('wanted', 'filters', 'summary');
  const setupId = domId('wanted', 'setup');

  return (
    <>
      <PageHeader
        title={t('wanted')}
        meta={t('wantedIntro')}
        actions={
          canManage && result.data && groups.length > 0 ? (
            <RunMonitored
              targets={runTargets}
              hasSource={hasIndexer}
              sourceHint={!noSource}
              label={t('wantedRunMonitored')}
              confirm="wantedRunConfirm"
              none="wantedRunNoneShown"
              reasonId={noSource ? setupId : undefined}
            />
          ) : undefined
        }
      />
      {canManage && noSource && groups.length > 0 && (
        <div className="notice info wanted-setup" role="note">
          <Icon icon={IconPlugConnected} size={20} />
          <p id={setupId}>{t('wantedSetupTitle')}</p>
          <a className="button sm" href="#/settings?section=sources">
            {t('openSourceSettings')}
          </a>
        </div>
      )}
      <ErrorNotice error={sources.error} retry={sources.reload} />
      <div className="wanted-toolbar">
        <SearchField label={t('wantedSearch')} value={q} onChange={(value) => update({ q: value.trim() })} />
        <div className="wanted-filters-bar">
          <FilterGroup label={t('contentType')}>
            <SegmentedControl
              label={t('contentType')}
              value={kind}
              options={contentTypes.map((value) => ({ value, label: t(value) }))}
              onChange={(value) => update({ kind: value })}
            />
          </FilterGroup>
          <div className="wanted-filters-more">
            <button
              type="button"
              className="wanted-filters-toggle"
              aria-expanded={filtersOpen}
              aria-controls={filtersId}
              aria-describedby={filtersOpen ? undefined : filterSummaryId}
              onClick={() => setFiltersOpen((open) => !open)}
            >
              <Icon icon={IconAdjustmentsHorizontal} size={18} />
              {activeFilters
                ? t('libraryFiltersActive').replace('{count}', String(activeFilters))
                : t('libraryFilters')}
              <Icon icon={IconChevronDown} size={16} />
            </button>
            {!filtersOpen && (
              <span className="wanted-filters-summary" id={filterSummaryId}>
                {filterSummary}
              </span>
            )}
          </div>
        </div>
        <div className="wanted-filters-row" id={filtersId} hidden={!filtersOpen}>
          <FilterGroup label={t('wantedMonitoring')}>
            <SegmentedControl
              label={t('wantedMonitoring')}
              value={monitoring ?? 'monitored'}
              disabled={!monitoring}
              options={monitoringOptions.map((value) => ({ value, label: t(monitoringLabels[value]) }))}
              onChange={(value) => update({ monitoring: value })}
            />
          </FilterGroup>
          <FilterGroup label={t('wantedAvailability')}>
            <SegmentedControl
              label={t('wantedAvailability')}
              value={availability}
              options={availabilityOptions.map((value) => ({ value, label: t(availabilityLabels[value]) }))}
              onChange={(value) => update({ availability: value })}
            />
          </FilterGroup>
        </div>
      </div>
      {groups.length > 0 && monitoring === 'all' && !requestedMonitoring && defaultMonitoring === 'all' && (
        <p className="wanted-default-note">{t('wantedNothingMonitoredNote')}</p>
      )}
      {publicationId && <PublicationScope id={publicationId} clear={() => update({ publicationId: '' })} />}
      {result.loading || !monitoring ? (
        <Loading />
      ) : !result.data ? (
        <ErrorNotice error={result.error} retry={result.reload} />
      ) : groups.length ? (
        <>
          <ul className="wanted-groups" ref={groupsRef}>
            {groups.map(({ publication, items }) => (
              <WantedGroup
                key={publication.id}
                publication={publication}
                items={items}
                totals={totals.get(publication.id)}
                canManage={canManage}
                selected={selected}
                setSelected={setSelected}
                expanded={expanded.has(publication.id)}
                toggle={() => toggle(publication.id)}
              />
            ))}
          </ul>
          <ErrorNotice error={extra?.error} retry={() => void loadMore()} />
          {nextCursor && (
            <div className="pagination actions">
              <Button disabled={extra?.loading} aria-busy={extra?.loading} onClick={() => void loadMore()}>
                {t('loadMore')}
              </Button>
            </div>
          )}
          <p className="sr-only" role="status">
            {extra?.pages ? t('wantedShowingCount').replace('{count}', String(allItems.length)) : ''}
          </p>
          {canManage && (
            <>
              <p className="sr-only" role="status">
                {selected.size ? t('wantedSelectedCount').replace('{count}', String(selected.size)) : ''}
              </p>
              {selected.size > 0 && (
                <div className="wanted-bulk-bar" role="group" aria-label={t('wantedBulkActions')}>
                  <span className="wanted-bulk-count" aria-hidden="true">
                    {t('wantedSelectedCount').replace('{count}', String(selected.size))}
                  </span>
                  <RunMonitored
                    targets={selectedTargets}
                    hasSource={hasIndexer}
                    sourceHint={!noSource}
                    label={t('wantedRunSelected')}
                    confirm="wantedRunConfirmSelected"
                    none="wantedRunNoneSelected"
                    reasonId={noSource ? setupId : undefined}
                  />
                  <Button
                    variant="ghost"
                    icon={IconX}
                    onClick={() => setSelection({ path, ids: new Set() })}
                  >
                    {t('wantedClearSelection')}
                  </Button>
                </div>
              )}
            </>
          )}
        </>
      ) : atDefaults && monitoring === 'monitored' ? (
        <EmptyState
          title={t('wantedEmptyMonitoredTitle')}
          action={<Button onClick={() => update({ monitoring: 'all' })}>{t('wantedShowAll')}</Button>}
        >
          {t('wantedEmptyMonitoredText')}
        </EmptyState>
      ) : atDefaults ? (
        <EmptyState
          title={t('wantedEmptyTitle')}
          action={canManage ? <a className="button" href="#/">{t('wantedOpenLibrary')}</a> : undefined}
        >
          {t('wantedEmptyText')}
        </EmptyState>
      ) : (
        <EmptyState
          title={t('wantedNoMatchesTitle')}
          action={<Button onClick={() => replaceHash('#/wanted')}>{t('wantedClearFilters')}</Button>}
        >
          {t('wantedNoMatchesText')}
        </EmptyState>
      )}
    </>
  );
}

/* Counts per status. From totals when the server sends them (every matching unit), otherwise from what is loaded. */
function groupCounts(items: WantedUnit[], totals: Totals | undefined) {
  if (totals)
    return {
      missing: totals.missing,
      unchecked: totals.unverified,
      present: totals.present,
      changed: totals.changed,
      scanProblem: totals.scan_problem,
      monitored: totals.monitored,
      exact: true,
    };
  const kinds = items.map((item) => unitStatus(item).kind);
  const count = (kind: string) => kinds.filter((value) => value === kind).length;
  return {
    missing: count('missing'),
    unchecked: count('unchecked'),
    present: count('file'),
    changed: count('warning'),
    scanProblem: count('error'),
    monitored: items.filter((item) => item.enabled_monitor_count > 0).length,
    exact: false,
  };
}

function Checkbox({
  label,
  checked,
  indeterminate = false,
  onChange,
}: {
  label: string;
  checked: boolean;
  indeterminate?: boolean;
  onChange: (checked: boolean) => void;
}) {
  const ref = useRef<HTMLInputElement>(null);
  useEffect(() => {
    if (ref.current) ref.current.indeterminate = indeterminate;
  }, [indeterminate]);
  return (
    <label className="wanted-check">
      <input
        ref={ref}
        type="checkbox"
        checked={checked}
        aria-label={label}
        onChange={(event) => onChange(event.target.checked)}
      />
    </label>
  );
}

function WantedGroup({
  publication,
  items,
  totals,
  canManage,
  selected,
  setSelected,
  expanded,
  toggle,
}: {
  publication: Schema['Publication'];
  items: WantedUnit[];
  totals: Totals | undefined;
  canManage: boolean;
  selected: ReadonlySet<string>;
  setSelected: (ids: string[], checked: boolean) => void;
  expanded: boolean;
  toggle: () => void;
}) {
  const { t, locale } = useI18n();
  const titleId = domId('wanted', publication.id, 'title');
  const metaId = domId('wanted', publication.id, 'meta');
  const summaryId = domId('wanted', publication.id, 'summary');
  const unitsId = domId('wanted', publication.id, 'units');
  const editions = new Set(items.map((item) => item.context.edition.id));
  const counts = groupCounts(items, totals);
  const missingByEdition = new Map<string, { edition: Schema['Edition']; labels: string[] }>();
  for (const item of items) {
    if (unitStatus(item).kind !== 'missing') continue;
    const { edition, unit } = item.context;
    const label = datedLabel.test(unit.label.trim()) ? (formatUnitDate(unit, locale) ?? unit.label) : unit.label;
    const entry = missingByEdition.get(edition.id);
    if (entry) entry.labels.push(label);
    else missingByEdition.set(edition.id, { edition, labels: [label] });
  }
  const missingCount = [...missingByEdition.values()].reduce((sum, entry) => sum + entry.labels.length, 0);
  const missingEditions = [...missingByEdition.values()].map(({ edition, labels }) => ({
    name: missingByEdition.size > 1 ? editionName(edition, locale) : undefined,
    labels,
  }));
  const count = (key: MessageKey, value: number) => t(key).replace('{count}', String(value));
  const summary = [
    counts.missing
      ? missingCount
        ? count('wantedGroupMissing', counts.missing).replace(
            '{units}',
            summarizeEditions(
              missingEditions,
              t('wantedEditionUnits'),
              t('wantedAndMore'),
              Math.max(0, counts.missing - missingCount),
            ),
          )
        : count('wantedGroupMissingCount', counts.missing)
      : undefined,
    counts.unchecked ? count('wantedGroupUnverified', counts.unchecked) : undefined,
    counts.present ? count('wantedGroupPresent', counts.present) : undefined,
    counts.exact ? undefined : count('wantedGroupShown', items.length),
  ]
    .filter(Boolean)
    .join('. ');
  const first = items.find(needsFile) ?? items[0];
  const findHref = `#/search?unit=${encodeURIComponent(first.context.unit.id)}`;
  const ids = items.map((item) => item.context.unit.id);
  const selectedCount = ids.filter((id) => selected.has(id)).length;
  return (
    <li className="wanted-group">
      <div className="wanted-group-header">
        {canManage && (
          <Checkbox
            label={t('wantedSelectGroup').replace('{title}', publication.title)}
            checked={selectedCount === ids.length}
            indeterminate={selectedCount > 0 && selectedCount < ids.length}
            onChange={(checked) => setSelected(ids, checked)}
          />
        )}
        <Cover
          fileId={items[0].publication_cover_file_id}
          title={publication.title}
          contentType={publication.content_type}
        />
        <div className="wanted-group-main">
          <h2 id={titleId}>
            <a
              href={`#/publication/${encodeURIComponent(publication.id)}`}
              aria-describedby={`${metaId} ${summaryId}`}
              data-row
              data-row-f={canManage ? findHref : undefined}
              title={publication.title}
            >
              {publication.title}
            </a>
          </h2>
          <p className="wanted-meta" id={metaId}>
            {[t(typeNames[publication.content_type]), publication.run_label].filter(Boolean).join(', ')}
          </p>
          <p className="wanted-group-summary" id={summaryId}>
            {summary}
          </p>
          {(counts.monitored > 0 || counts.changed > 0 || counts.scanProblem > 0) && (
            <div className="wanted-unit-status">
              {counts.monitored > 0 && (
                <StatusBadge kind="monitored" label={count('wantedGroupMonitored', counts.monitored)} />
              )}
              {counts.changed > 0 && <StatusBadge kind="warning" label={count('wantedGroupChanged', counts.changed)} />}
              {counts.scanProblem > 0 && (
                <StatusBadge kind="error" label={count('wantedGroupScanProblem', counts.scanProblem)} />
              )}
            </div>
          )}
        </div>
        <div className="wanted-group-actions">
          {canManage && (
            <a className="button ghost sm" href={findHref} aria-describedby={titleId}>
              <Icon icon={IconSearch} size={16} />
              {t('wantedFindReleases')}
            </a>
          )}
          <Button
            size="sm"
            variant="ghost"
            icon={expanded ? IconChevronUp : IconChevronDown}
            aria-expanded={expanded}
            aria-controls={unitsId}
            aria-label={t('wantedDetailsFor').replace('{title}', publication.title)}
            onClick={toggle}
          >
            {t('wantedDetails')}
          </Button>
        </div>
      </div>
      <ul className={`wanted-units${canManage ? ' selectable' : ''}`} id={unitsId} hidden={!expanded}>
        {expanded &&
          items.map((item) => {
            const { edition, unit } = item.context;
            const status = unitStatus(item);
            const name = `${publication.title} ${unit.label}`.trim();
            return (
              <li key={unit.id} className="wanted-unit">
                {canManage && (
                  <Checkbox
                    label={t('wantedSelectUnit').replace('{title}', name)}
                    checked={selected.has(unit.id)}
                    onChange={(checked) => setSelected([unit.id], checked)}
                  />
                )}
                <div className="wanted-unit-main">
                  <strong>{unit.label}</strong>
                  <span className="wanted-meta">
                    {unitMeta(unit, t(unit.kind), locale, editions.size > 1 ? edition : undefined)}
                  </span>
                </div>
                <div className="wanted-unit-status">
                  <StatusBadge kind={status.kind} label={status.label && t(status.label)} />
                  {item.enabled_monitor_count > 0 && <StatusBadge kind="monitored" />}
                </div>
                <div className="wanted-unit-actions">
                  {canManage && (
                    <a className="button ghost sm" href={`#/search?unit=${encodeURIComponent(unit.id)}`}>
                      <Icon icon={IconSearch} size={16} />
                      {t('wantedFindReleases')}
                    </a>
                  )}
                  <Menu
                    label={t('wantedActionsFor').replace('{title}', name)}
                    items={[
                      {
                        label: t('wantedMonitor'),
                        icon: IconBell,
                        href: `#/monitors?unit_id=${encodeURIComponent(unit.id)}`,
                      },
                    ]}
                  />
                </div>
              </li>
            );
          })}
      </ul>
    </li>
  );
}

type RunFailure = { id: string; label: string; message: string };
/* Makes the enabled monitors of the given monitored, still-missing units due now (POST /monitors/{id}/run). */
function RunMonitored({
  targets,
  hasSource,
  sourceHint,
  label,
  confirm,
  none,
  reasonId,
}: {
  targets: WantedUnit[];
  /* undefined while sources load */
  hasSource: boolean | undefined;
  /* false when the page already shows the setup banner */
  sourceHint: boolean;
  label: string;
  confirm: MessageKey;
  none: MessageKey;
  /* id of reason text shown elsewhere on the page (the setup banner) */
  reasonId?: string;
}) {
  const { t } = useI18n();
  const [phase, setPhase] = useState<'idle' | 'confirm' | 'running' | 'done'>('idle');
  const [queued, setQueued] = useState<WantedUnit[]>([]);
  const [progress, setProgress] = useState({ done: 0, started: 0 });
  const [failures, setFailures] = useState<RunFailure[]>([]);
  const hintId = domId('wanted', 'run', label, 'hint');
  const message = (error: unknown) =>
    error instanceof ApiError && error.code !== 'network_error' && error.message ? error.message : t('requestFailed');
  async function run(list: WantedUnit[]) {
    setPhase('running');
    setFailures([]);
    let started = 0;
    const failed: RunFailure[] = [];
    for (const [index, item] of list.entries()) {
      const { publication, unit } = item.context;
      try {
        const params = new URLSearchParams({ unit_id: unit.id, enabled: 'true', limit: '100' });
        const page = await request<Schema['MonitorPage']>(`/monitors?${params}`);
        for (const monitor of page.items)
          if (monitor.enabled && !monitor.running)
            await post<Schema['MonitorView']>(`/monitors/${encodeURIComponent(monitor.id)}/run`, {
              revision: monitor.revision,
            } satisfies Schema['MonitorRevision']);
        started += 1;
      } catch (error) {
        failed.push({ id: unit.id, label: `${publication.title} ${unit.label}`.trim(), message: message(error) });
      }
      setProgress({ done: index + 1, started });
    }
    setFailures(failed);
    setPhase('done');
  }
  const running = phase === 'running';
  const loaded = hasSource !== undefined;
  const hint =
    loaded && !hasSource ? (
      sourceHint ? (
        <>
          {t('wantedRunNeedsSource')} <a href="#/settings?section=sources">{t('openSourceSettings')}</a>
        </>
      ) : undefined
    ) : loaded && !targets.length ? (
      t(none)
    ) : undefined;
  return (
    <div className="wanted-run">
      {phase === 'confirm' ? (
        <div className="wanted-run-confirm" role="group" aria-label={label}>
          <p>{t(confirm).replace('{count}', String(queued.length))}</p>
          <div className="actions">
            <Button variant="primary" icon={IconSearch} autoFocus onClick={() => void run(queued)}>
              {t('wantedRunConfirmButton').replace('{count}', String(queued.length))}
            </Button>
            <Button variant="ghost" onClick={() => setPhase('idle')}>
              {t('cancel')}
            </Button>
          </div>
        </div>
      ) : (
        <Button
          icon={IconSearch}
          disabled={!hasSource || !targets.length || running}
          aria-busy={running}
          aria-describedby={hint ? hintId : loaded && !hasSource ? reasonId : undefined}
          onClick={() => {
            setQueued(targets);
            setProgress({ done: 0, started: 0 });
            setPhase('confirm');
          }}
        >
          {label}
        </Button>
      )}
      {hint && phase !== 'confirm' && (
        <p className="wanted-meta" id={hintId}>
          {hint}
        </p>
      )}
      <p className="wanted-run-status" role="status">
        {running
          ? t('wantedRunProgress').replace('{done}', String(progress.done)).replace('{total}', String(queued.length))
          : phase === 'done' && progress.started
            ? t('wantedRunDone').replace('{count}', String(progress.started))
            : ''}
        {phase === 'done' && progress.started > 0 && (
          <>
            {' '}
            <a href="#/review">{t('wantedOpenReview')}</a>
          </>
        )}
      </p>
      {phase === 'done' && failures.length > 0 && (
        <div className="notice error wanted-run-failures" role="alert">
          <p>{t('wantedRunFailed').replace('{count}', String(failures.length))}</p>
          <ul>
            {failures.map((failure) => (
              <li key={failure.id}>
                <strong>{failure.label}</strong>: {failure.message}
              </li>
            ))}
          </ul>
        </div>
      )}
    </div>
  );
}

function FilterGroup({ label, children }: { label: string; children: ReactNode }) {
  return (
    <div className="filter-group">
      <span className="filter-label" aria-hidden="true">
        {label}
      </span>
      {children}
    </div>
  );
}
