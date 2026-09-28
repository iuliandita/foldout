import { useEffect, useRef, useState } from 'react';
import {
  IconPencil,
  IconPlayerPause,
  IconPlayerPlay,
  IconPlus,
  IconSearch,
  IconTrash,
} from '@tabler/icons-react';
import { useI18n, type Locale, type MessageKey } from './i18n';
import { post, request, type Schema } from './lib/api/client';
import {
  Button,
  EmptyState,
  ErrorNotice,
  Field,
  Icon,
  Loading,
  formatSize,
  Menu,
  PageHeader,
  SaveForm,
  StatusBadge,
  useResource,
  value,
  type StatusKind,
} from './ui';
import { RelativeTime, usePolled } from './Activity';
import type { UnitSelection } from './Storage';
import './styles/wanted.css';

function formatInterval(seconds: number, locale: Locale) {
  const [amount, unit] =
    seconds % 86400 === 0
      ? [seconds / 86400, 'day']
      : seconds % 3600 === 0
        ? [seconds / 3600, 'hour']
        : [Math.round(seconds / 60), 'minute'];
  return new Intl.NumberFormat(locale, { style: 'unit', unit, unitDisplay: 'long' }).format(amount);
}
const monitorStates: Record<Schema['MonitorView']['last_state'], StatusKind> = {
  scheduled: 'info',
  disabled: 'unchecked',
  awaiting_release: 'unchecked',
  needs_review: 'warning',
  source_error: 'error',
};
/* Explains that monitors need a Prowlarr source, with a way to add one. */
function NeedsSource() {
  const { t } = useI18n();
  return (
    <EmptyState
      title={t('monitorsNeedSourceTitle')}
      action={
        <a className="button" href="#/settings?section=sources">
          {t('monitorsOpenSources')}
        </a>
      }
    >
      {t('monitorsNeedSourceText')}
    </EmptyState>
  );
}

type Selection = (candidate: Schema['MonitorCandidate'], monitor: Schema['MonitorView']) => void;
export function Monitors({
  unit,
  sources,
  select,
}: {
  unit: UnitSelection;
  sources: Schema['IntegrationChoice'][];
  select: Selection;
}) {
  const { t } = useI18n();
  const [cursor, setCursor] = useState<string>();
  const [creating, setCreating] = useState(false);
  const list = useResource<Schema['MonitorPage']>(
    `/monitors?limit=20&unit_id=${encodeURIComponent(unit.id)}${cursor ? `&cursor=${encodeURIComponent(cursor)}` : ''}`,
  );
  return (
    <section>
      <header className="section-header">
        <div>
          <h2>{t('monitors')}</h2>
          <p className="wanted-meta">{t('monitorsIntro')}</p>
        </div>
        {sources.length > 0 && (
          <Button icon={IconPlus} disabled={creating} onClick={() => setCreating(true)}>
            {t('createMonitor')}
          </Button>
        )}
      </header>
      {creating && (
        <SaveForm
          key={unit.id}
          label={t('createMonitor')}
          cancel={() => setCreating(false)}
          submit={async (data) => {
            const input: Schema['CreateMonitor'] = {
              unit_id: unit.id,
              integration_id: value(data, 'monitor_source'),
              query: value(data, 'monitor_query'),
              interval_seconds: Number(value(data, 'interval')),
              enabled: value(data, 'monitor_enabled') === 'true',
              selection_policy: 'review_only',
            };
            await post<Schema['MonitorView']>('/monitors', input);
            setCreating(false);
            setCursor(undefined);
            list.reload();
          }}
        >
          <p>
            <strong>{unit.label}</strong> / {t(unit.contentType)}
          </p>
          <label className="field">
            <span>{t('source')} *</span>
            <select required name="monitor_source" defaultValue="">
              <option value="" disabled>
                {t('chooseSource')}
              </option>
              {sources.map((source) => (
                <option key={source.id} value={source.id}>
                  {source.label}
                </option>
              ))}
            </select>
          </label>
          <MonitorFields query={unit.title} interval={3600} enabled={false} />
        </SaveForm>
      )}
      {list.loading ? (
        <Loading />
      ) : list.error ? (
        <ErrorNotice error={list.error} retry={list.reload} />
      ) : (
        list.data && (
          <>
            {list.data.items.length === 0 && !cursor && !sources.length && <NeedsSource />}
            <ul className="monitor-list">
              {list.data.items.map((item) => (
                <MonitorRow
                  key={`${item.id}:${item.revision}`}
                  item={item}
                  reload={list.reload}
                  select={select}
                  canManage
                />
              ))}
            </ul>
            {list.data.items.length === 0 && sources.length > 0 && (
              <p className="wanted-meta">{t('monitorsUnitNone')}</p>
            )}
            <div className="pagination actions">
              {cursor && <button onClick={() => setCursor(undefined)}>{t('firstPage')}</button>}
              {list.data.next_cursor && (
                <button onClick={() => setCursor(list.data?.next_cursor ?? undefined)}>
                  {t('next')}
                </button>
              )}
            </div>
          </>
        )
      )}
    </section>
  );
}
function PublicationMonitorScope({ id }: { id: string }) {
  const publication = usePolled<Schema['Publication']>(`/publications/${encodeURIComponent(id)}`, false);
  return publication.data ? (
    <>{[publication.data.title, publication.data.run_label].filter(Boolean).join(', ')}</>
  ) : null;
}
function UnitMonitorScope({ id }: { id: string }) {
  const context = usePolled<Schema['UnitContext']>(`/units/${encodeURIComponent(id)}`, false);
  if (!context.data) return null;
  const { publication, unit } = context.data;
  return <>{`${publication.title} ${unit.label}`}</>;
}
export function AllMonitors({ query, canManage }: { query: URLSearchParams; canManage: boolean }) {
  const { t } = useI18n();
  const cursor = query.get('cursor');
  const publicationId = query.get('publication_id')?.trim();
  const unitId = query.get('unit_id')?.trim();
  const params = new URLSearchParams({ limit: '20' });
  if (publicationId) params.set('publication_id', publicationId);
  if (unitId) params.set('unit_id', unitId);
  if (cursor) params.set('cursor', cursor);
  const list = usePolled<Schema['MonitorPage']>(`/monitors?${params}`);
  const empty = list.data?.items.length === 0 && !cursor;
  const choices = usePolled<Schema['IntegrationChoiceList']>(
    empty && canManage ? '/search/integrations' : null,
    false,
  );
  const hasSource = choices.data?.items.some((choice) => choice.kind === 'prowlarr');
  const href = (nextCursor?: string | null) => {
    const target = new URLSearchParams();
    if (publicationId) target.set('publication_id', publicationId);
    if (unitId) target.set('unit_id', unitId);
    if (nextCursor) target.set('cursor', nextCursor);
    const search = target.toString();
    return `#/monitors${search ? `?${search}` : ''}`;
  };
  return (
    <>
      <PageHeader
        title={t('monitors')}
        meta={
          unitId ? (
            <UnitMonitorScope key={unitId} id={unitId} />
          ) : publicationId ? (
            <PublicationMonitorScope key={publicationId} id={publicationId} />
          ) : (
            t('monitorsIntro')
          )
        }
        actions={
          <>
            {(publicationId || unitId) && (
              <a className="button ghost" href="#/monitors">
                {t('clearMonitorScope')}
              </a>
            )}
            {canManage && unitId && (
              <a className="button" href={`#/search?unit=${encodeURIComponent(unitId)}`}>
                <Icon icon={IconSearch} size={18} />
                {t('wantedFindReleases')}
              </a>
            )}
          </>
        }
      />
      {list.loading ? (
        <Loading />
      ) : !list.data ? (
        <ErrorNotice error={list.error} retry={list.reload} />
      ) : empty ? (
        choices.loading ? (
          <Loading />
        ) : canManage && choices.data && !hasSource ? (
          <NeedsSource />
        ) : (
          <EmptyState
            title={t('monitorsEmptyTitle')}
            action={
              canManage && unitId ? (
                <a className="button primary" href={`#/search?unit=${encodeURIComponent(unitId)}`}>
                  {t('monitorsCreate')}
                </a>
              ) : !unitId ? (
                <a className="button" href="#/wanted">
                  {t('monitorsOpenWanted')}
                </a>
              ) : undefined
            }
          >
            {t(unitId ? 'monitorsUnitEmptyText' : 'monitorsEmptyText')}
          </EmptyState>
        )
      ) : (
        <>
          <ErrorNotice error={list.error} retry={list.reload} />
          <ul className="monitor-list">
            {list.data.items.map((item) => (
              <MonitorRow
                key={`${item.id}:${item.revision}`}
                item={item}
                reload={list.reload}
                canManage={canManage}
                searchHref={`#/search?unit=${encodeURIComponent(item.target.unit.id)}`}
              />
            ))}
          </ul>
          {(cursor || list.data.next_cursor) && (
            <div className="pagination actions">
              {cursor && <a className="button" href={href()}>{t('firstPage')}</a>}
              {list.data.next_cursor && <a className="button" href={href(list.data.next_cursor)}>{t('next')}</a>}
            </div>
          )}
        </>
      )}
    </>
  );
}
function MonitorFields({
  query,
  interval,
  enabled,
}: {
  query: string;
  interval: number;
  enabled: boolean;
}) {
  const { t } = useI18n();
  return (
    <>
      <Field
        name="monitor_query"
        label={t('searchQuery')}
        required
        defaultValue={query}
        maxLength={512}
      />
      <Field
        name="interval"
        label={t('intervalSeconds')}
        hint={t('utcIntervalHint')}
        type="number"
        min={900}
        max={31536000}
        step={1}
        required
        defaultValue={interval}
      />
      <label className="field">
        <span>{t('state')}</span>
        <select name="monitor_enabled" defaultValue={String(enabled)}>
          <option value="false">{t('paused')}</option>
          <option value="true">{t('enabled')}</option>
        </select>
      </label>
      <p>{t('reviewOnly')}</p>
    </>
  );
}
function MonitorRow({
  item,
  reload,
  select,
  canManage,
  searchHref,
}: {
  item: Schema['MonitorView'];
  reload: () => void;
  select?: Selection;
  canManage: boolean;
  searchHref?: string;
}) {
  const { t, locale } = useI18n();
  const [editing, setEditing] = useState(false);
  const [confirming, setConfirming] = useState(false);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<unknown>();
  const active = useRef(false);
  const [now, setNow] = useState(Date.now());
  useEffect(() => {
    const timer = window.setInterval(() => setNow(Date.now()), 15000);
    return () => window.clearInterval(timer);
  }, []);
  async function act(action: 'run' | 'toggle' | 'delete') {
    if (active.current) return;
    active.current = true;
    setBusy(true);
    setError(undefined);
    try {
      const path = `/monitors/${item.id}`;
      if (action === 'delete')
        await request(`${path}?revision=${item.revision}`, { method: 'DELETE' });
      else if (action === 'run')
        await post<Schema['MonitorView']>(`${path}/run`, {
          revision: item.revision,
        } satisfies Schema['MonitorRevision']);
      else
        await request<Schema['MonitorView']>(path, {
          method: 'PATCH',
          body: JSON.stringify({
            revision: item.revision,
            enabled: !item.enabled,
          } satisfies Schema['UpdateMonitor']),
        });
      reload();
    } catch (failure) {
      setError(failure);
    } finally {
      active.current = false;
      setBusy(false);
    }
  }
  const { publication, unit } = item.target;
  const stateKey: MessageKey = item.running
    ? 'running'
    : item.candidates_truncated
      ? 'incompleteResults'
      : item.last_state;
  return (
    <li className="monitor-row">
      <div className="monitor-head">
        <div className="monitor-title">
          <h3>{item.query}</h3>
          <p className="wanted-meta">
            <a href={`#/publication/${encodeURIComponent(publication.id)}`}>{publication.title}</a>{' '}
            <a href={`#/monitors?unit_id=${encodeURIComponent(unit.id)}`}>{unit.label}</a>
            {`, ${item.integration_label}`}
          </p>
        </div>
        <div className="monitor-badges">
          <StatusBadge kind={item.enabled ? 'monitored' : 'not_monitored'} label={t(item.enabled ? 'enabled' : 'paused')} />
          <StatusBadge
            kind={item.running ? 'info' : item.candidates_truncated ? 'warning' : monitorStates[item.last_state]}
            label={t(stateKey)}
          />
        </div>
      </div>
      <p className="wanted-meta">
        {t('monitorEvery').replace('{interval}', formatInterval(item.interval_seconds, locale))}
        {item.enabled && (
          <>
            {'. '}
            {t('monitorNextCheck')} <RelativeTime seconds={item.next_run} now={now} />
          </>
        )}
        {item.last_run_at !== null && (
          <>
            {'. '}
            {t('monitorLastCheck')} <RelativeTime seconds={item.last_run_at} now={now} />
          </>
        )}
      </p>
      {item.reason && <p className="notice">{t(item.reason)}</p>}
      <ErrorNotice error={error} retry={reload} />
      {canManage && editing ? (
        <SaveForm
          label={t('save')}
          cancel={() => setEditing(false)}
          submit={async (data) => {
            const input: Schema['UpdateMonitor'] = {
              revision: item.revision,
              query: value(data, 'monitor_query'),
              interval_seconds: Number(value(data, 'interval')),
              enabled: value(data, 'monitor_enabled') === 'true',
              selection_policy: 'review_only',
            };
            await request<Schema['MonitorView']>(`/monitors/${item.id}`, {
              method: 'PATCH',
              body: JSON.stringify(input),
            });
            setEditing(false);
            reload();
          }}
        >
          <p className="wanted-meta">{t('monitorUpdateHint')}</p>
          <MonitorFields
            query={item.query}
            interval={item.interval_seconds}
            enabled={item.enabled}
          />
        </SaveForm>
      ) : canManage ? (
        <div className="actions">
          <Button
            size="sm"
            icon={IconPlayerPlay}
            disabled={busy || !item.enabled || item.running}
            onClick={() => void act('run')}
          >
            {t('runNow')}
          </Button>
          {searchHref && (
            <a className="button sm ghost" href={searchHref}>
              {t('reviewReleases')}
            </a>
          )}
          <Menu
            label={t('wantedActionsFor').replace('{title}', item.query)}
            items={[
              {
                label: t(item.enabled ? 'pauseMonitor' : 'enableMonitor'),
                icon: item.enabled ? IconPlayerPause : IconPlayerPlay,
                disabled: busy,
                onSelect: () => void act('toggle'),
              },
              { label: t('edit'), icon: IconPencil, disabled: busy, onSelect: () => setEditing(true) },
              {
                label: t('deleteMonitor'),
                icon: IconTrash,
                danger: true,
                disabled: busy,
                onSelect: () => setConfirming(true),
              },
            ]}
          />
        </div>
      ) : null}
      {canManage && !editing && item.candidates.length > 0 && (
        <p className="wanted-meta">{t('monitorUpdateHint')}</p>
      )}
      {confirming && (
        <div className="notice">
          <p>{t('deleteMonitorHint')}</p>
          <div className="actions">
            <Button
              variant="danger"
              disabled={busy || editing}
              onClick={() => void act('delete')}
            >
              {t('deleteMonitor')}
            </Button>
            <Button variant="ghost" disabled={busy} onClick={() => setConfirming(false)}>
              {t('cancel')}
            </Button>
          </div>
        </div>
      )}
      {item.candidates_truncated && <p className="notice">{t('incompleteResultsHint')}</p>}
      {item.candidates.length > 0 && (
        <ul className="monitor-candidates">
          {item.candidates.map((candidate) => (
            <li key={candidate.release_handle}>
              <div>
                <strong>{candidate.title}</strong>
                <span className="wanted-meta">
                  {t(candidate.protocol)}, {formatSize(candidate.size_bytes, locale)},{' '}
                  {t(candidate.expires_at * 1000 <= now ? 'candidateExpired' : 'candidateAvailable')}
                </span>
              </div>
              {canManage && select && (
                <Button
                  size="sm"
                  disabled={busy || editing || candidate.expires_at * 1000 <= now}
                  onClick={() => {
                    if (candidate.expires_at * 1000 > Date.now()) select(candidate, item);
                    else setNow(Date.now());
                  }}
                >
                  {t('selectRelease')}
                </Button>
              )}
            </li>
          ))}
        </ul>
      )}
    </li>
  );
}
