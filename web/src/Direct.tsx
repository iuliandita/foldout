import { useEffect, useRef, useState } from 'react';
import { useI18n } from './i18n';
import { ApiError, post, request, type Schema } from './lib/api/client';
import { IconSearch, IconX } from '@tabler/icons-react';
import { Button, EmptyState, ErrorNotice, Loading, PageHeader, SaveForm, formatSize, useResource, value } from './ui';
import {
  ActivityStatus,
  BackLink,
  RelativeTime,
  TechnicalDetails,
  unitName,
  usePolled,
  useUnitContext,
} from './Activity';
import type { UnitSelection } from './Storage';
import { DestinationFields, readDestination } from './Acquisitions';
import { MangaDirect } from './MangaDirect';

/* The unit the Find releases route shows, so a late success never navigates away from another page. */
export function routeUnitId() {
  const [path, search = ''] = location.hash.replace(/^#/, '').split('?');
  return path === '/search' ? new URLSearchParams(search).get('unit')?.trim() || undefined : undefined;
}

const mirrorStates = {
  direct: 'directReady',
  needs_resolution: 'needsResolution',
  manual_action: 'manualMirror',
  unsupported: 'unsupportedMirror',
} as const;

export function DirectSearch({
  unit,
  choices,
}: {
  unit: UnitSelection;
  choices: Schema['IntegrationChoice'][];
}) {
  const { t } = useI18n();
  const [results, setResults] = useState<Schema['DirectSearchPage']>();
  const [detail, setDetail] = useState<Schema['DirectDetail']>();
  const [selected, setSelected] = useState<Schema['DirectLink']>();
  const [search, setSearch] = useState<Schema['DirectSearch']>();
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<unknown>();
  const active = useRef(false);
  const sources = choices.filter((choice) => choice.kind === 'getcomics');
  async function action(work: () => Promise<void>) {
    if (active.current) return;
    active.current = true;
    setBusy(true);
    setError(undefined);
    try {
      await work();
    } catch (failure) {
      setError(failure);
    } finally {
      active.current = false;
      setBusy(false);
    }
  }
  async function run(input: Schema['DirectSearch']) {
    await action(async () => {
      setResults(undefined);
      setDetail(undefined);
      setSearch(input);
      setResults(await post<Schema['DirectSearchPage']>('/direct/search', input));
    });
  }
  if (selected)
    return (
      <DirectSubmit
        unit={unit}
        link={selected}
        title={detail?.title ?? ''}
        cancel={() => setSelected(undefined)}
      />
    );
  if (unit.contentType === 'manga' && unit.language)
    return (
      <MangaDirect
        unit={{ ...unit, language: unit.language }}
        choices={choices}
        renderSelection={(chapter, title, cancel) => (
          <DirectSubmit
            unit={unit}
            link={{ link_handle: chapter.link_handle, host: 'MangaDex' }}
            title={title}
            cancel={cancel}
          />
        )}
      />
    );
  return (
    <div className="releases">
      {unit.contentType !== 'comic' ? (
        <EmptyState title={t('directNotAvailable')}>{t('directComicsOnly')}</EmptyState>
      ) : sources.length === 0 ? (
        <EmptyState
          title={t('directNotConfigured')}
          action={
            <a className="button" href="#/settings?section=sources">
              {t('openSourceSettings')}
            </a>
          }
        >
          {t('directNoSource')}
        </EmptyState>
      ) : (
        <>
          <form
            className="release-search-form"
            aria-busy={busy}
            onSubmit={(event) => {
              event.preventDefault();
              const data = new FormData(event.currentTarget);
              void run({
                integration_id: value(data, 'direct_source'),
                query: value(data, 'direct_query'),
                page: 1,
              });
            }}
          >
            <label className="field">
              <span>{t('source')}</span>
              <select
                name="direct_source"
                required
                defaultValue={sources.length === 1 ? sources[0].id : ''}
              >
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
            <label className="field release-query">
              <span>{t('searchQuery')}</span>
              <input name="direct_query" required maxLength={512} defaultValue={unit.title} />
            </label>
            <Button type="submit" variant="primary" icon={IconSearch} disabled={busy}>
              {t('search')}
            </Button>
          </form>
          {results && !detail && (
            <>
              {results.posts.length ? (
                <ul className="release-list">
                  {results.posts.map((item) => (
                    <li key={item.post_handle}>
                      <span className="release-name">{item.title}</span>
                      <Button
                        size="sm"
                        disabled={busy}
                        onClick={() =>
                          void action(async () =>
                            setDetail(
                              await post<Schema['DirectDetail']>('/direct/details', {
                                post_handle: item.post_handle,
                              }),
                            ),
                          )
                        }
                      >
                        {t('viewMirrors')}
                      </Button>
                    </li>
                  ))}
                </ul>
              ) : (
                <EmptyState title={t('noReleasesFound')}>{t('noReleasesFoundHint')}</EmptyState>
              )}
              {(results.page > 1 || results.next_page) && (
                <div className="pagination">
                  <Button
                    size="sm"
                    disabled={busy || results.page <= 1}
                    onClick={() => search && void run({ ...search, page: results.page - 1 })}
                  >
                    {t('previous')}
                  </Button>
                  <span>
                    {t('page')} {results.page}
                  </span>
                  <Button
                    size="sm"
                    disabled={busy || !results.next_page}
                    onClick={() =>
                      search && results.next_page && void run({ ...search, page: results.next_page })
                    }
                  >
                    {t('next')}
                  </Button>
                </div>
              )}
            </>
          )}
          {detail && (
            <div className="release-detail">
              <div className="section-header">
                <h3>{detail.title}</h3>
                <Button variant="ghost" size="sm" disabled={busy} onClick={() => setDetail(undefined)}>
                  {t('directBackResults')}
                </Button>
              </div>
              <ul className="release-list">
                {detail.links.map((link) => (
                  <li key={link.link_handle}>
                    <span>
                      <strong>{link.host}</strong>{' '}
                      <span className="muted">
                        {t(mirrorStates[link.state])}
                      </span>
                    </span>
                    {(link.state === 'direct' || link.state === 'needs_resolution') && (
                      <Button
                        size="sm"
                        disabled={busy}
                        onClick={() =>
                          void action(async () => {
                            const resolved = await post<
                              Pick<Schema['DirectLink'], 'state' | 'host'>
                            >('/direct/resolve', { link_handle: link.link_handle });
                            const updated = { ...link, ...resolved };
                            setDetail({
                              ...detail,
                              links: detail.links.map((item) =>
                                item.link_handle === link.link_handle ? updated : item,
                              ),
                            });
                            if (resolved.state === 'direct') setSelected(updated);
                          })
                        }
                      >
                        {t('releaseSelect')}
                      </Button>
                    )}
                  </li>
                ))}
              </ul>
            </div>
          )}
        </>
      )}
      {busy && <Loading />}
      <ErrorNotice error={error} />
    </div>
  );
}

function DirectSubmit({
  unit,
  link,
  title,
  cancel,
}: {
  unit: UnitSelection;
  link: Pick<Schema['DirectLink'], 'link_handle'> & { host: string };
  title: string;
  cancel: () => void;
}) {
  const { t } = useI18n();
  const roots = useResource<Schema['RootChoiceList']>('/acquisition/roots');
  const [attempt, setAttempt] = useState<{ key: string; input: Schema['DirectRequest'] }>();
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<unknown>();
  const active = useRef(false);
  const mounted = useRef(false);
  useEffect(() => {
    mounted.current = true;
    return () => {
      mounted.current = false;
    };
  }, []);
  async function submit(selected: NonNullable<typeof attempt>) {
    if (active.current) return;
    active.current = true;
    setBusy(true);
    setError(undefined);
    try {
      const created = await request<Schema['DirectAcquisition']>('/direct/acquisitions', {
        method: 'POST',
        headers: { 'Idempotency-Key': selected.key },
        body: JSON.stringify(selected.input),
      });
      if (!created || typeof created !== 'object' || typeof created.id !== 'string' || !created.id)
        throw new ApiError(0, 'invalid_response', '');
      if (mounted.current && routeUnitId() === unit.id)
        location.hash = `/direct-acquisition/${encodeURIComponent(created.id)}`;
    } catch (failure) {
      setError(failure);
    } finally {
      active.current = false;
      setBusy(false);
    }
  }
  return (
    <div className="release-confirm">
      <h2>{t('releaseConfirmTitle')}</h2>
      {title && <p className="release-confirm-name">{title}</p>}
      <p className="release-confirm-meta">
        {unit.label}
        <br />
        {link.host}
      </p>
      <p className="muted release-field-hint">{t('directConfirm')}</p>
      {attempt ? (
        <div className="notice release-pending">
          <p>{t('pendingAcquisitionHint')}</p>
          <p className="path">{attempt.input.destination.relative_path}</p>
          <ErrorNotice error={error} />
          <div className="actions">
            <Button variant="primary" disabled={busy} onClick={() => void submit(attempt)}>
              {t(busy ? 'saving' : 'retryAcquisition')}
            </Button>
            <a href="#/activity?kind=downloads">{t('activityOpenDownloads')}</a>
          </div>
        </div>
      ) : (
        <SaveForm
          label={t('submitAcquisition')}
          cancel={cancel}
          submit={async (data) => {
            const destination = readDestination(data, t('invalidRelativePath'));
            if (!destination) throw new ApiError(0, 'invalid_input', t('chooseRoot'));
            const selected = {
              key: crypto.randomUUID(),
              input: {
                link_handle: link.link_handle,
                unit_id: unit.id,
                download_root_id: value(data, 'download_root'),
                destination,
              } satisfies Schema['DirectRequest'],
            };
            setAttempt(selected);
            await submit(selected);
          }}
        >
          <label className="field">
            <span>{t('downloadRoot')} *</span>
            <select name="download_root" required defaultValue="">
              <option value="" disabled>
                {t('chooseRoot')}
              </option>
              {roots.data?.items.map((root) => (
                <option key={root.id} value={root.id}>
                  {root.label}
                </option>
              ))}
            </select>
          </label>
          {roots.loading && <Loading />}
          <ErrorNotice error={roots.error} retry={roots.reload} />
          <DestinationFields />
        </SaveForm>
      )}
    </div>
  );
}

export function DirectAcquisitionDetail({ id }: { id: string }) {
  const { t, locale } = useI18n();
  const result = usePolled<Schema['DirectAcquisition']>(
    `/direct/acquisitions/${encodeURIComponent(id)}`,
  );
  const item = result.data;
  const unit = useUnitContext(item?.unit_id);
  const [confirming, setConfirming] = useState(false);
  const [cancelError, setCancelError] = useState<unknown>();
  const [canceling, setCanceling] = useState(false);
  const active = useRef(false);
  async function cancelIntent() {
    if (active.current) return;
    active.current = true;
    setCanceling(true);
    setCancelError(undefined);
    try {
      await post<Schema['DirectAcquisition']>(
        `/direct/acquisitions/${encodeURIComponent(id)}/cancel`,
        {},
      );
      setConfirming(false);
      result.reload();
    } catch (failure) {
      setCancelError(failure);
    } finally {
      active.current = false;
      setCanceling(false);
    }
  }
  return (
    <>
      <BackLink />
      {result.loading ? (
        <Loading />
      ) : !item ? (
        <ErrorNotice error={result.error} retry={result.reload} />
      ) : (
        <div className="activity-detail">
          <PageHeader
            title={
              unit.data
                ? t('activityDownloadOf').replace('{title}', unitName(unit.data))
                : t('activityDownload')
            }
            meta={
              <span className="activity-detail-meta">
                <ActivityStatus state={item.state} />
                <span>
                  {t('updated')} <RelativeTime seconds={item.updated_at} />
                </span>
              </span>
            }
          />
          <ErrorNotice error={result.error} retry={result.reload} />
          {unit.data && (
            <p className="activity-detail-link">
              <a href={`#/publication/${encodeURIComponent(unit.data.publication.id)}`}>
                {unit.data.publication.title}
              </a>
            </p>
          )}
          {item.reason && <p className="notice">{t(item.reason)}</p>}
          <dl className="job-details">
            <dt>{t('activitySource')}</dt>
            <dd>{t('activityDirect')}</dd>
            {item.downloaded_bytes > 0 && (
              <>
                <dt>{t('activitySize')}</dt>
                <dd>{formatSize(item.downloaded_bytes, locale)}</dd>
              </>
            )}
            <dt>{t('updated')}</dt>
            <dd>{new Date(item.updated_at * 1000).toLocaleString(locale)}</dd>
            {item.canceled_at !== null && (
              <>
                <dt>{t('canceled')}</dt>
                <dd>{new Date(item.canceled_at * 1000).toLocaleString(locale)}</dd>
              </>
            )}
          </dl>
          <ErrorNotice error={cancelError} />
          {cancelError instanceof ApiError && cancelError.code === 'direct_review_required' && (
            <p className="notice">{t('directCancelReview')}</p>
          )}
          {cancelError instanceof ApiError && cancelError.code === 'direct_busy' && (
            <p className="notice">{t('directCancelBusy')}</p>
          )}
          {(item.state === 'queued' ||
            item.state === 'needs_review' ||
            item.state === 'downloaded') &&
            (confirming ? (
              <div className="notice">
                <p>{t('directCancelHint')}</p>
                <div className="actions">
                  <Button
                    variant="danger"
                    disabled={canceling}
                    onClick={() => void cancelIntent()}
                  >
                    {t(canceling ? 'saving' : 'directConfirmCancel')}
                  </Button>
                  <Button
                    variant="ghost"
                    disabled={canceling}
                    onClick={() => {
                      setConfirming(false);
                      setCancelError(undefined);
                    }}
                  >
                    {t('directKeepIntent')}
                  </Button>
                </div>
              </div>
            ) : (
              <div className="actions">
                <Button
                  icon={IconX}
                  disabled={canceling}
                  onClick={() => {
                    setConfirming(true);
                    setCancelError(undefined);
                  }}
                >
                  {t('directCancelIntent')}
                </Button>
              </div>
            ))}
          <TechnicalDetails
            rows={[
              [t('acquisitionId'), <code>{item.id}</code>],
              [t('activityUnitId'), <code>{item.unit_id}</code>],
              [t('activityImportId'), <code>{item.import_id}</code>],
              [
                t('activityTransferStarted'),
                t(item.attempted ? 'activityYes' : 'activityNo'),
              ],
              [t('activityBytes'), item.downloaded_bytes.toLocaleString(locale)],
            ]}
          />
        </div>
      )}
    </>
  );
}
