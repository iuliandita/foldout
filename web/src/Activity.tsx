import { useCallback, useEffect, useRef, useState, type ReactNode } from 'react';
import {
  IconChevronLeft,
  IconChevronRight,
  IconDownload,
  IconFileImport,
  IconRotateClockwise,
  IconScan,
  IconSettingsAutomation,
  IconX,
  type TablerIcon,
} from '@tabler/icons-react';
import { type Schema, request } from './lib/api/client';
import { useI18n, type MessageKey } from './i18n';
import {
  Button,
  EmptyState,
  ErrorNotice,
  Icon,
  Loading,
  domId,
  PageHeader,
  SegmentedControl,
  StatusBadge,
  type StatusKind,
} from './ui';
import './styles/activity.css';

type JobView = Schema['Job'];
type Acquisition = Schema['Acquisition'];
type DirectAcquisition = Schema['DirectAcquisition'];
type AnyState = JobView['state'] | Acquisition['state'] | DirectAcquisition['state'];

const pollInterval = 10_000;
const pageSize = 25;

/* Calls the latest `tick` every 10 s while the tab is visible and once when it becomes visible again. */
export function useVisiblePolling(tick: () => void, enabled = true) {
  const latest = useRef(tick);
  useEffect(() => {
    latest.current = tick;
  });
  useEffect(() => {
    if (!enabled) return;
    let timer: number | undefined;
    const start = () => {
      window.clearInterval(timer);
      timer = window.setInterval(() => latest.current(), pollInterval);
    };
    const onVisibility = () => {
      if (document.visibilityState === 'visible') {
        latest.current();
        start();
      } else window.clearInterval(timer);
    };
    if (document.visibilityState === 'visible') start();
    document.addEventListener('visibilitychange', onVisibility);
    return () => {
      window.clearInterval(timer);
      document.removeEventListener('visibilitychange', onVisibility);
    };
  }, [enabled]);
}

/* Like useResource, but keeps the last data while refreshing so polling never flashes a skeleton. */
export function usePolled<T>(path: string | null, poll = true) {
  const [state, setState] = useState<{ path: string | null; data?: T; error?: unknown }>({ path });
  const inflight = useRef<AbortController | undefined>(undefined);
  const load = useCallback(
    (force = false) => {
      if (!path) return;
      if (inflight.current) {
        if (!force) return;
        inflight.current.abort();
      }
      const controller = new AbortController();
      inflight.current = controller;
      request<T>(path, { signal: controller.signal })
        .then((data) => {
          if (!controller.signal.aborted) setState({ path, data });
        })
        .catch((error) => {
          if (!controller.signal.aborted)
            setState((previous) => ({
              path,
              data: previous.path === path ? previous.data : undefined,
              error,
            }));
        })
        .finally(() => {
          if (inflight.current === controller) inflight.current = undefined;
        });
    },
    [path],
  );
  useEffect(() => {
    load(true);
    return () => {
      inflight.current?.abort();
      inflight.current = undefined;
    };
  }, [load]);
  useVisiblePolling(load, poll && !!path);
  const current = state.path === path ? state : { path, data: undefined, error: undefined };
  return {
    data: current.data,
    error: current.error,
    loading: !!path && current.data === undefined && !current.error,
    reload: () => load(true),
  };
}

export type ActivityGroup = 'active' | 'attention' | 'done';
/* `attention` marks active states that still wait on the user (an indexer download that needs its file). */
export function activityStatus(state: AnyState, attention = false): { group: ActivityGroup; kind: StatusKind } {
  if (state === 'completed') return { group: 'done', kind: 'done' };
  if (state === 'canceled') return { group: 'done', kind: 'unchecked' };
  if (state === 'failed') return { group: 'attention', kind: 'error' };
  if (state === 'needs_review' || attention) return { group: 'attention', kind: 'warning' };
  return { group: 'active', kind: 'running' };
}
const stateLabels: Record<AnyState, MessageKey> = {
  queued: 'queued',
  running: 'running',
  retry_wait: 'retry_wait',
  needs_review: 'needs_review',
  completed: 'completed',
  failed: 'failed',
  cancel_requested: 'cancel_requested',
  canceled: 'canceled',
  downloading: 'downloading',
  downloaded: 'activityDownloaded',
  importing: 'importing',
};
export function ActivityStatus({ state, attention }: { state: AnyState; attention?: boolean }) {
  const { t } = useI18n();
  return <StatusBadge kind={activityStatus(state, attention).kind} label={t(stateLabels[state])} />;
}

function relative(seconds: number, now: number, locale: string) {
  const diff = Math.round(seconds - now / 1000);
  const abs = Math.abs(diff);
  const format = new Intl.RelativeTimeFormat(locale, { numeric: 'auto' });
  if (abs < 45) return format.format(0, 'second');
  if (abs < 3600) return format.format(Math.round(diff / 60), 'minute');
  if (abs < 86400) return format.format(Math.round(diff / 3600), 'hour');
  if (abs < 86400 * 7) return format.format(Math.round(diff / 86400), 'day');
  return new Date(seconds * 1000).toLocaleDateString(locale, { dateStyle: 'medium' });
}
function duration(seconds: number, locale: string) {
  const [amount, unit] =
    seconds < 60
      ? [Math.max(1, Math.round(seconds)), 'second']
      : seconds < 3600
        ? [Math.round(seconds / 60), 'minute']
        : [Math.round(seconds / 360) / 10, 'hour'];
  return new Intl.NumberFormat(locale, { style: 'unit', unit, unitDisplay: 'long' }).format(amount);
}

export function RelativeTime({ seconds, now }: { seconds: number; now?: number }) {
  const { locale } = useI18n();
  const date = new Date(seconds * 1000);
  return (
    <time dateTime={date.toISOString()} title={date.toLocaleString(locale)}>
      {relative(seconds, now ?? Date.now(), locale)}
    </time>
  );
}

/* One relative time frame per row ("Finished 5 minutes ago, took 2 minutes"); the absolute time is in the tooltip. */
const finishedStates: ReadonlySet<AnyState> = new Set(['completed', 'failed', 'canceled']);
function EntryTime({ entry, now }: { entry: Entry; now: number }) {
  const { t, locale } = useI18n();
  const finished = finishedStates.has(entry.state);
  const seconds = !finished && entry.started !== undefined ? entry.started : entry.updated;
  const template = finished
    ? entry.started !== undefined
      ? t('activityFinishedTook').replace(
          '{duration}',
          duration(Math.max(0, entry.updated - entry.started), locale),
        )
      : t('activityFinishedAt')
    : entry.started !== undefined
      ? t('activityStartedAt')
      : undefined;
  if (!template) return <RelativeTime seconds={seconds} now={now} />;
  const [before, after = ''] = template.split('{time}');
  return (
    <>
      {before}
      <RelativeTime seconds={seconds} now={now} />
      {after}
    </>
  );
}

export function TechnicalDetails({ rows }: { rows: [string, ReactNode][] }) {
  const { t } = useI18n();
  return (
    <details className="technical-details">
      <summary>{t('technicalDetails')}</summary>
      <dl className="job-details">
        {rows.map(([label, content]) => (
          <div key={label} className="technical-row">
            <dt>{label}</dt>
            <dd>{content}</dd>
          </div>
        ))}
      </dl>
    </details>
  );
}

export function BackLink({ href = '#/activity', label }: { href?: string; label?: string }) {
  const { t } = useI18n();
  return (
    <a className="back" href={href}>
      <Icon icon={IconChevronLeft} size={18} />
      {label ?? t('backActivity')}
    </a>
  );
}

export const unitName = (context: Schema['UnitContext']) =>
  `${context.publication.title} ${context.unit.label}`.trim();

/* Resolves unit ids to "Title label" once per id; failures fall back to the generic label. */
function useUnitNames(ids: string[]) {
  const [names, setNames] = useState<Record<string, string | null>>({});
  const requested = useRef(new Set<string>());
  const wanted = ids.filter((id) => !requested.current.has(id));
  const key = wanted.join(',');
  useEffect(() => {
    if (!key) return;
    for (const id of key.split(',')) {
      requested.current.add(id);
      request<Schema['UnitContext']>(`/units/${encodeURIComponent(id)}`)
        .then((context) => setNames((previous) => ({ ...previous, [id]: unitName(context) })))
        .catch(() => setNames((previous) => ({ ...previous, [id]: null })));
    }
  }, [key]);
  return names;
}
/* Same lookup for one unit on a detail page. */
export function useUnitContext(id: string | undefined) {
  return usePolled<Schema['UnitContext']>(id ? `/units/${encodeURIComponent(id)}` : null, false);
}

type Cursor = string | number;
type SourcePage<T> = { items: T[]; next: Cursor | null };
/* One paginated source of the timeline: keeps its own cursor, refreshes its first page on poll. */
function useSource<T extends { id: string }>(
  fetchPage: (cursor: Cursor | null) => Promise<SourcePage<T>>,
  enabled: boolean,
) {
  const [state, setState] = useState<{
    items: T[];
    next: Cursor | null;
    pages: number;
    loaded: boolean;
    error?: unknown;
  }>({ items: [], next: null, pages: 0, loaded: false });
  const busy = useRef(false);
  const fetcher = useRef(fetchPage);
  useEffect(() => {
    fetcher.current = fetchPage;
  });
  const refresh = useCallback(async () => {
    if (!enabled || busy.current) return;
    busy.current = true;
    try {
      const page = await fetcher.current(null);
      setState((previous) => {
        const fresh = new Map(page.items.map((item) => [item.id, item]));
        const older = previous.items.filter((item) => !fresh.has(item.id));
        return {
          items: [...page.items, ...(previous.pages > 1 ? older : [])],
          next: previous.pages > 1 ? previous.next : page.next,
          pages: Math.max(previous.pages, 1),
          loaded: true,
        };
      });
    } catch (error) {
      setState((previous) => ({ ...previous, loaded: true, error }));
    } finally {
      busy.current = false;
    }
  }, [enabled]);
  const loadMore = useCallback(async () => {
    if (!enabled || busy.current || state.next === null) return;
    busy.current = true;
    try {
      const page = await fetcher.current(state.next);
      setState((previous) => {
        const known = new Set(previous.items.map((item) => item.id));
        return {
          items: [...previous.items, ...page.items.filter((item) => !known.has(item.id))],
          next: page.next,
          pages: previous.pages + 1,
          loaded: true,
        };
      });
    } catch (error) {
      setState((previous) => ({ ...previous, error }));
    } finally {
      busy.current = false;
    }
  }, [enabled, state.next]);
  useEffect(() => {
    void refresh();
  }, [refresh]);
  return { ...state, hasMore: enabled && state.next !== null, refresh, loadMore };
}
const offsetPage =
  <T,>(path: string) =>
  async (cursor: Cursor | null): Promise<SourcePage<T>> => {
    const offset = typeof cursor === 'number' ? cursor : 0;
    const page = await request<{ items: T[] }>(`${path}?limit=${pageSize}&offset=${offset}`);
    return { items: page.items, next: page.items.length === pageSize ? offset + pageSize : null };
  };
const jobsPage = async (cursor: Cursor | null): Promise<SourcePage<JobView>> => {
  const page = await request<Schema['JobPage']>(
    `/jobs?limit=${pageSize}${typeof cursor === 'string' ? `&cursor=${encodeURIComponent(cursor)}` : ''}`,
  );
  return { items: page.items, next: page.next_cursor };
};
const acquisitionsPage = offsetPage<Acquisition>('/acquisition');
const directPage = offsetPage<DirectAcquisition>('/direct/acquisitions');

const kindFilters = ['all', 'downloads', 'scans', 'imports'] as const;
const stateFilters = ['all', 'active', 'attention', 'done'] as const;
type KindFilter = (typeof kindFilters)[number];
type StateFilter = (typeof stateFilters)[number];
const kindLabels: Record<KindFilter, MessageKey> = {
  all: 'all',
  downloads: 'activityDownloads',
  scans: 'activityScans',
  imports: 'activityImports',
};
const stateFilterLabels: Record<StateFilter, MessageKey> = {
  all: 'all',
  active: 'activityActive',
  attention: 'activityAttention',
  done: 'activityDone',
};

type Entry = {
  key: string;
  kind: 'download' | 'scan' | 'other';
  importing: boolean;
  icon: TablerIcon;
  title: string;
  meta?: string;
  state: AnyState;
  attention: boolean;
  updated: number;
  /* Jobs only: start time, for "Started ..." and the run duration. */
  started?: number;
  href: string;
};

function replaceActivityQuery(query: URLSearchParams, name: string, value: string) {
  const next = new URLSearchParams(query);
  if (value === 'all') next.delete(name);
  else next.set(name, value);
  const search = next.toString();
  history.replaceState(history.state, '', `#/activity${search ? `?${search}` : ''}`);
  window.dispatchEvent(new HashChangeEvent('hashchange'));
}

export function Activity({ query, canManage }: { query: URLSearchParams; canManage: boolean }) {
  const { t } = useI18n();
  const kind = kindFilters.find((value) => value === query.get('kind')) ?? 'all';
  const stateFilter = stateFilters.find((value) => value === query.get('state')) ?? 'all';
  const jobs = useSource(jobsPage, true);
  const acquisitions = useSource(acquisitionsPage, true);
  const direct = useSource(directPage, canManage);
  const [now, setNow] = useState(() => Date.now());
  const [loadingMore, setLoadingMore] = useState(false);
  useVisiblePolling(() => {
    setNow(Date.now());
    void jobs.refresh();
    void acquisitions.refresh();
    void direct.refresh();
  });
  const unitIds = [
    ...new Set([...acquisitions.items, ...direct.items].map((item) => item.unit_id)),
  ];
  const names = useUnitNames(unitIds);
  const downloadTitle = (unitId: string) => {
    const name = names[unitId];
    return name ? t('activityDownloadOf').replace('{title}', name) : t('activityDownload');
  };
  const pipelineJobs = new Set(acquisitions.items.map((item) => item.job_id));
  const entries: Entry[] = [
    ...jobs.items
      .filter((job) => !(job.kind === 'acquisition.pipeline' && pipelineJobs.has(job.id)))
      .map<Entry>((job) => ({
        key: `job:${job.id}`,
        kind:
          job.kind === 'library.scan'
            ? 'scan'
            : job.kind === 'acquisition.pipeline'
              ? 'download'
              : 'other',
        importing: false,
        icon:
          job.kind === 'library.scan'
            ? IconScan
            : job.kind === 'acquisition.pipeline'
              ? IconDownload
              : IconSettingsAutomation,
        title: jobTitle(job, t),
        meta: job.reason === 'uncertain_effect' ? t('uncertainEffect') : undefined,
        state: job.state,
        attention: false,
        updated: job.updated_at,
        started: job.created_at,
        href: `#/job/${encodeURIComponent(job.id)}`,
      })),
    ...acquisitions.items.map<Entry>((item) => ({
      key: `acquisition:${item.id}`,
      kind: 'download',
      importing: item.state === 'downloaded' || item.state === 'importing',
      icon: item.state === 'downloaded' || item.state === 'importing' ? IconFileImport : IconDownload,
      title: downloadTitle(item.unit_id),
      meta: item.reason
        ? t(item.reason)
        : item.state === 'downloaded'
          ? t('activityChooseFile')
          : t('activityDownloadClient'),
      state: item.state,
      attention: item.state === 'downloaded',
      updated: item.updated_at,
      href: `#/acquisition/${encodeURIComponent(item.id)}`,
    })),
    ...direct.items.map<Entry>((item) => ({
      key: `direct:${item.id}`,
      kind: 'download',
      importing: item.state === 'downloaded' || item.state === 'importing',
      icon: item.state === 'downloaded' || item.state === 'importing' ? IconFileImport : IconDownload,
      title: downloadTitle(item.unit_id),
      meta: item.reason ? t(item.reason) : t('activityDirect'),
      state: item.state,
      attention: false,
      updated: item.updated_at,
      href: `#/direct-acquisition/${encodeURIComponent(item.id)}`,
    })),
  ].sort((a, b) => b.updated - a.updated);
  const visible = entries.filter(
    (entry) =>
      (kind === 'all' ||
        (kind === 'downloads' && entry.kind === 'download') ||
        (kind === 'scans' && entry.kind === 'scan') ||
        (kind === 'imports' && entry.importing)) &&
      (stateFilter === 'all' || activityStatus(entry.state, entry.attention).group === stateFilter),
  );
  const sources = canManage ? [jobs, acquisitions, direct] : [jobs, acquisitions];
  const loaded = sources.every((source) => source.loaded);
  const hasMore = sources.some((source) => source.hasMore);
  const filtered = kind !== 'all' || stateFilter !== 'all';
  async function loadMore() {
    if (loadingMore) return;
    setLoadingMore(true);
    try {
      await Promise.all(sources.filter((source) => source.hasMore).map((source) => source.loadMore()));
    } finally {
      setLoadingMore(false);
    }
  }
  return (
    <>
      <PageHeader title={t('activity')} meta={t('activityIntro')} />
      <div className="activity-filters">
        <div className="filter-group">
          <span className="filter-label" aria-hidden="true">
            {t('activityKindFilter')}
          </span>
          <SegmentedControl
            label={t('activityKindFilter')}
            value={kind}
            options={kindFilters.map((value) => ({ value, label: t(kindLabels[value]) }))}
            onChange={(value) => replaceActivityQuery(query, 'kind', value)}
          />
        </div>
        <div className="filter-group">
          <span className="filter-label" aria-hidden="true">
            {t('activityStateFilter')}
          </span>
          <SegmentedControl
            label={t('activityStateFilter')}
            value={stateFilter}
            options={stateFilters.map((value) => ({ value, label: t(stateFilterLabels[value]) }))}
            onChange={(value) => replaceActivityQuery(query, 'state', value)}
          />
        </div>
      </div>
      <ErrorNotice error={jobs.error} context={t('activityTasksUnavailable')} retry={() => void jobs.refresh()} />
      <ErrorNotice
        error={acquisitions.error}
        context={t('activityDownloadsUnavailable')}
        retry={() => void acquisitions.refresh()}
      />
      {canManage && (
        <ErrorNotice
          error={direct.error}
          context={t('activityDirectUnavailable')}
          retry={() => void direct.refresh()}
        />
      )}
      {!loaded && !entries.length ? (
        <Loading />
      ) : visible.length ? (
        <ul className="activity-list">
          {visible.map((entry) => (
            <li key={entry.key}>
              <a
                className="activity-row"
                href={entry.href}
                aria-labelledby={domId('activity', entry.key, 'title')}
                aria-describedby={[
                  entry.meta && domId('activity', entry.key, 'meta'),
                  domId('activity', entry.key, 'state'),
                  domId('activity', entry.key, 'time'),
                ]
                  .filter(Boolean)
                  .join(' ')}
              >
                <span className="activity-icon">
                  <Icon icon={entry.icon} />
                </span>
                <span className="activity-main">
                  <strong id={domId('activity', entry.key, 'title')}>{entry.title}</strong>
                  {entry.meta && (
                    <span className="activity-meta" id={domId('activity', entry.key, 'meta')}>
                      {entry.meta}
                    </span>
                  )}
                </span>
                <span className="activity-state" id={domId('activity', entry.key, 'state')}>
                  <ActivityStatus state={entry.state} attention={entry.attention} />
                </span>
                <span
                  className={`activity-time${finishedStates.has(entry.state) ? ' finished' : ''}`}
                  id={domId('activity', entry.key, 'time')}
                >
                  <EntryTime entry={entry} now={now} />
                </span>
                <span className="activity-chevron">
                  <Icon icon={IconChevronRight} size={18} />
                </span>
              </a>
            </li>
          ))}
        </ul>
      ) : filtered ? (
        <EmptyState
          title={t('activityNoMatchesTitle')}
          action={
            <Button
              onClick={() => {
                history.replaceState(history.state, '', '#/activity');
                window.dispatchEvent(new HashChangeEvent('hashchange'));
              }}
            >
              {t('activityShowAll')}
            </Button>
          }
        >
          {t(hasMore ? 'activityNoMatchesMore' : 'activityNoMatchesText')}
        </EmptyState>
      ) : (
        <EmptyState title={t('activityEmptyTitle')}>{t('activityEmptyText')}</EmptyState>
      )}
      {hasMore && (
        <div className="activity-more">
          <Button disabled={loadingMore} aria-busy={loadingMore} onClick={() => void loadMore()}>
            {t(loadingMore ? 'loading' : 'loadMore')}
          </Button>
        </div>
      )}
    </>
  );
}

const jobReasons: Record<string, MessageKey> = {
  uncertain_effect: 'uncertainEffect',
  uncertain_submission: 'uncertain_submission',
  scan_failed: 'jobReasonScanFailed',
  scan_has_unreadable_entries: 'jobReasonScanUnreadable',
  invalid_scan_payload: 'jobReasonScanPayload',
};
function jobTitle(job: Pick<JobView, 'kind' | 'subject'>, t: (key: MessageKey) => string) {
  const kind = job.kind;
  if (kind === 'library.scan' && job.subject?.root_label)
    return t('activityScanOf').replace('{root}', job.subject.root_label);
  return kind === 'library.scan'
    ? t('libraryScan')
    : kind === 'acquisition.pipeline'
      ? t('activityDownload')
      : t('activityTask');
}
export function JobDetail({ id, canManage }: { id: string; canManage: boolean }) {
  const { t, locale } = useI18n();
  const result = usePolled<JobView>(`/jobs/${encodeURIComponent(id)}`);
  const [reviewing, setReviewing] = useState(false);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<unknown>();
  async function act(action: 'cancel' | 'retry') {
    if (busy) return;
    setBusy(true);
    setError(undefined);
    try {
      await request<JobView>(`/jobs/${encodeURIComponent(id)}/${action}`, { method: 'POST' });
      setReviewing(false);
      result.reload();
    } catch (failure) {
      setError(failure);
    } finally {
      setBusy(false);
    }
  }
  const job = result.data;
  return (
    <>
      <BackLink />
      {result.loading ? (
        <Loading />
      ) : !job ? (
        <ErrorNotice error={result.error} retry={result.reload} />
      ) : (
        <div className="activity-detail">
          <PageHeader
            title={jobTitle(job, t)}
            meta={
              <span className="activity-detail-meta">
                <ActivityStatus state={job.state} />
                <span>
                  {t('updated')} <RelativeTime seconds={job.updated_at} />
                </span>
              </span>
            }
          />
          <ErrorNotice error={result.error} retry={result.reload} />
          {job.reason && (
            <div className="notice">
              <p>{t(Object.hasOwn(jobReasons, job.reason) ? jobReasons[job.reason] : 'jobReasonUnknown')}</p>
            </div>
          )}
          <dl className="job-details">
            <dt>{t('attempts')}</dt>
            <dd>{job.attempts}</dd>
            <dt>{t('created')}</dt>
            <dd>{new Date(job.created_at * 1000).toLocaleString(locale)}</dd>
            <dt>{t('updated')}</dt>
            <dd>{new Date(job.updated_at * 1000).toLocaleString(locale)}</dd>
            {job.retry_at !== null && (
              <>
                <dt>{t('retryAt')}</dt>
                <dd>{new Date(job.retry_at * 1000).toLocaleString(locale)}</dd>
              </>
            )}
          </dl>
          {job.kind === 'acquisition.pipeline' && (
            <p className="notice">
              {t('activityPipelineHint')}{' '}
              <a href="#/activity?kind=downloads">{t('activityOpenDownloads')}</a>
            </p>
          )}
          <ErrorNotice error={error} />
          {canManage && (
            <div className="actions">
              {['queued', 'running', 'retry_wait'].includes(job.state) && (
                <Button icon={IconX} disabled={busy} onClick={() => void act('cancel')}>
                  {t(busy ? 'saving' : 'cancelJob')}
                </Button>
              )}
              {job.kind !== 'acquisition.pipeline' &&
                ['needs_review', 'failed'].includes(job.state) &&
                (reviewing ? (
                  <div className="notice">
                    <p>{t('retryHint')}</p>
                    <div className="actions">
                      <Button variant="primary" disabled={busy} onClick={() => void act('retry')}>
                        {t(busy ? 'saving' : 'confirmRetry')}
                      </Button>
                      <Button variant="ghost" disabled={busy} onClick={() => setReviewing(false)}>
                        {t('cancel')}
                      </Button>
                    </div>
                  </div>
                ) : (
                  <Button icon={IconRotateClockwise} onClick={() => setReviewing(true)}>
                    {t('retryJob')}
                  </Button>
                ))}
            </div>
          )}
          <TechnicalDetails
            rows={[
              [t('activityTaskId'), <code>{job.id}</code>],
              [t('activityKindLabel'), <code>{job.kind}</code>],
              ...(job.reason ? [[t('activityReasonLabel'), <code>{job.reason}</code>] as [string, ReactNode]] : []),
            ]}
          />
        </div>
      )}
    </>
  );
}
