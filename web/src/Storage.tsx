import { useEffect, useId, useRef, useState } from 'react';
import { IconChevronRight, IconCopy, IconLink, IconRefresh } from '@tabler/icons-react';
import { request, post, type Page, type Schema } from './lib/api/client';
import { type MessageKey, useI18n } from './i18n';
import {
  Button,
  ErrorNotice,
  Field,
  Icon,
  Loading,
  formatSize,
  HelpTip,
  IconButton,
  Menu,
  PathText,
  SaveForm,
  SearchField,
  SectionHeader,
  SegmentedControl,
  Skeleton,
  StatusBadge,
  Tabs,
  type StatusKind,
  domId,
  useResource,
  value,
} from './ui';
import { Cover } from './Cover';
import { editionLabel, formatUnitDate } from './format';
type Root = Schema['LibraryRoot'];
type Entry = Schema['InventoryEntry'];
type Preview = Schema['ImportPreview'];
type FileView = Schema['LibraryFile'];
export function Storage() {
  const { t } = useI18n();
  const roots = useResource<Root[]>('/library/roots');
  const [adding, setAdding] = useState(false);
  const [selected, setSelected] = useState<string>();
  const root = roots.data?.find((root) => root.id === selected) ?? roots.data?.[0];
  return (
    <section>
      <SectionHeader
        title={t('storage')}
        description={t('rootHint')}
        help={
          <>
            <p>{t('helpRoot')}</p>
            <p>{t('helpLink')}</p>
          </>
        }
        action={{ label: t('addRoot'), creates: true, disabled: adding, onClick: () => setAdding(true) }}
      />
      {adding && (
        <SaveForm
          label={t('addRoot')}
          cancel={() => setAdding(false)}
          submit={async (data) => {
            const created = await post<Root>('/library/roots', {
              label: value(data, 'label'),
              path: value(data, 'path'),
            } satisfies Schema['RootInput']);
            setSelected(created.id);
            setAdding(false);
            roots.reload();
          }}
        >
          <Field label={t('rootLabel')} name="label" required autoFocus maxLength={128} />
          <Field label={t('rootPath')} name="path" required maxLength={4096} hint={t('rootPathHint')} />
        </SaveForm>
      )}
      {roots.loading ? (
        <Loading />
      ) : roots.error ? (
        <ErrorNotice error={roots.error} retry={roots.reload} />
      ) : roots.data?.length && root ? (
        roots.data.length > 1 ? (
          <Tabs
            label={t('roots')}
            items={roots.data.map((item) => ({ id: item.id, label: item.label }))}
            selected={root.id}
            onSelect={setSelected}
          >
            {(id) => {
              const current = roots.data?.find((item) => item.id === id);
              return current && <Inventory key={current.id} root={current} />;
            }}
          </Tabs>
        ) : (
          <Inventory key={root.id} root={root} />
        )
      ) : (
        <p className="empty-inline">{t('noRoots')}</p>
      )}
    </section>
  );
}
/* Scan states and reasons from the scanner (src/library/scan.rs); unknown values fall back to plain text, never the raw code. */
const entryStates: Record<string, { kind: StatusKind; label: MessageKey }> = {
  pending_association: { kind: 'info', label: 'storageNotLinked' },
  skipped: { kind: 'unchecked', label: 'storageSkipped' },
  missing: { kind: 'missing', label: 'statusMissing' },
  error: { kind: 'error', label: 'storageUnreadable' },
};
const entryReasons: Record<string, MessageKey> = {
  metadata_unchanged: 'storageReasonUnchanged',
  unmatched: 'storageReasonNew',
  not_seen_in_successful_scan: 'storageReasonNotSeen',
  symlink_skipped: 'storageReasonSymlink',
  symlink_replaced: 'storageReasonSymlink',
  escaping_path: 'storageReasonOutside',
  source_escaped_root: 'storageReasonOutside',
  non_utf8_path: 'storageReasonName',
  directory_unreadable: 'storageReasonFolder',
  directory_entry_unreadable: 'storageReasonFolder',
  metadata_unreadable: 'storageReasonDetails',
  source_unreadable: 'storageReasonUnreadable',
  source_changed: 'storageReasonChanged',
  not_regular_file: 'storageReasonNotFile',
};
function EntryStatus({ entry }: { entry: Entry }) {
  const { t, locale } = useI18n();
  if (entry.associated_unit_count > 0) {
    const units = entry.associated_units ?? [];
    const more = entry.associated_unit_count - units.length;
    return (
      <>
        <StatusBadge
          kind="file"
          label={
            entry.associated_unit_count === 1
              ? t('storageLinkedOne')
              : t('storageLinkedOther').replace(
                  '{count}',
                  new Intl.NumberFormat(locale).format(entry.associated_unit_count),
                )
          }
        />
        {units.length > 0 && (
          <span className="inventory-reason">
            {units.map((unit, index) => (
              <span key={unit.unit_id}>
                {index > 0 && ', '}
                <a href={`#/publication/${encodeURIComponent(unit.publication_id)}`}>
                  {`${unit.publication_title} ${unit.unit_label}`}
                </a>
              </span>
            ))}
            {more > 0 && ` ${t('storageLinkedMore').replace('{count}', new Intl.NumberFormat(locale).format(more))}`}
          </span>
        )}
      </>
    );
  }
  const state = entryStates[entry.state];
  const reason = entry.reason ? entryReasons[entry.reason] : undefined;
  return (
    <>
      <StatusBadge kind={state?.kind ?? 'info'} label={t(state?.label ?? 'storageStateOther')} />
      {reason && <span className="inventory-reason">{t(reason)}</span>}
    </>
  );
}
const activeJobStates: ReadonlySet<Schema['Job']['state']> = new Set([
  'queued',
  'running',
  'retry_wait',
  'cancel_requested',
]);
/* While a scan of this root is queued or running, polls the job and the visible page every 5 s
   (only while the tab is visible) and returns the latest page without clearing the list. */
function useScanRefresh(root: Root, jobId: string | undefined, path: string) {
  const [running, setRunning] = useState<string>();
  const [fresh, setFresh] = useState<{ path: string; page: Schema['InventoryPage'] }>();
  useEffect(() => {
    if (jobId) setRunning(jobId);
  }, [jobId]);
  /* A scan started elsewhere (another tab, a schedule) also counts. */
  useEffect(() => {
    const controller = new AbortController();
    request<Schema['JobPage']>('/jobs?limit=50', { signal: controller.signal })
      .then((page) => {
        const job = page.items.find(
          (item) => item.subject?.root_id === root.id && activeJobStates.has(item.state),
        );
        if (job && !controller.signal.aborted) setRunning((current) => current ?? job.id);
      })
      .catch(() => undefined);
    return () => controller.abort();
  }, [root.id]);
  useEffect(() => {
    if (!running) return;
    const controller = new AbortController();
    let timer: number | undefined;
    async function tick() {
      if (document.visibilityState !== 'visible') return;
      try {
        const [job, page] = await Promise.all([
          request<Schema['Job']>(`/jobs/${encodeURIComponent(running!)}`, { signal: controller.signal }),
          request<Schema['InventoryPage']>(path, { signal: controller.signal }),
        ]);
        if (controller.signal.aborted) return;
        setFresh({ path, page });
        if (!activeJobStates.has(job.state)) setRunning(undefined);
      } catch {
        if (!controller.signal.aborted) setRunning(undefined);
      }
    }
    timer = window.setInterval(() => void tick(), 5000);
    const onVisible = () => {
      if (document.visibilityState === 'visible') void tick();
    };
    document.addEventListener('visibilitychange', onVisible);
    return () => {
      controller.abort();
      window.clearInterval(timer);
      document.removeEventListener('visibilitychange', onVisible);
    };
  }, [running, path]);
  return { polling: !!running, page: fresh?.path === path ? fresh.page : undefined, clear: () => setFresh(undefined) };
}
function CopyPath({ path }: { path: string }) {
  const { t } = useI18n();
  const [result, setResult] = useState<'done' | 'failed'>();
  useEffect(() => {
    if (result !== 'done') return;
    const timer = window.setTimeout(() => setResult(undefined), 4000);
    return () => window.clearTimeout(timer);
  }, [result]);
  return (
    <>
      <IconButton
        icon={IconCopy}
        aria-label={t('copyPath')}
        onClick={() => {
          if (!navigator.clipboard) {
            setResult('failed');
            return;
          }
          navigator.clipboard.writeText(path).then(
            () => setResult('done'),
            () => setResult('failed'),
          );
        }}
      />
      <span className="copy-status" role="status">
        {result && t(result === 'done' ? 'copyPathDone' : 'copyPathFailed')}
      </span>
    </>
  );
}
function Inventory({ root }: { root: Root }) {
  const { t, locale } = useI18n();
  const [cursor, setCursor] = useState<string>();
  const entriesPath = `/library/roots/${root.id}/entries?limit=50${cursor ? `&cursor=${encodeURIComponent(cursor)}` : ''}`;
  const entries = useResource<Schema['InventoryPage']>(entriesPath);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<unknown>();
  const [jobId, setJobId] = useState<string>();
  const [entry, setEntry] = useState<Entry>();
  const scan = useScanRefresh(root, jobId, entriesPath);
  const page = scan.page ?? entries.data;
  function reload() {
    scan.clear();
    entries.reload();
  }
  return (
    <section className="inventory">
      <header className="section-header">
        <div className="inventory-heading">
          <h3>{root.label}</h3>
          <p className="root-path">
            <PathText path={root.path} />
            <CopyPath path={root.path} />
          </p>
        </div>
        <div className="actions">
          <Button
            disabled={busy}
            onClick={async () => {
              setBusy(true);
              setError(undefined);
              try {
                const job = await request<Schema['Job']>(`/library/roots/${root.id}/scan`, {
                  method: 'POST',
                });
                setJobId(job.id);
              } catch (failure) {
                setError(failure);
              } finally {
                setBusy(false);
              }
            }}
          >
            {t(busy ? 'saving' : 'scan')}
          </Button>
        </div>
        <p className="inventory-hint muted">{t('scanRefreshHint')}</p>
      </header>
      {(jobId || scan.polling) && (
        <p className="notice" role="status">
          {scan.polling && <span>{t('storageAutoRefresh')} </span>}
          {jobId && (
            <a className="back" href={`#/job/${jobId}`}>
              {t('scanJob')}
            </a>
          )}
        </p>
      )}
      <ErrorNotice error={error} />
      {!entry && (
        <div className="inventory-list-head">
          <h4>{t('files')}</h4>
          <IconButton icon={IconRefresh} aria-label={t('storageRefreshList')} onClick={reload} />
        </div>
      )}
      {entry ? (
        <Association
          entry={entry}
          close={() => {
            setEntry(undefined);
            reload();
          }}
        />
      ) : entries.loading && !page ? (
        <Loading />
      ) : entries.error && !page ? (
        <ErrorNotice error={entries.error} retry={reload} />
      ) : (
        page && (
          <>
            {page.items.length ? (
              <ul className="inventory-list">
                {page.items.map((entry) => (
                  <li
                    key={entry.id}
                    className={
                      entry.state === 'pending_association' && entry.associated_unit_count > 0
                        ? 'has-menu'
                        : undefined
                    }
                  >
                    <div>
                      <strong className="inventory-path">
                        <PathText path={entry.relative_path} />
                      </strong>
                      <p className="inventory-meta">
                        {[
                          entry.format?.toUpperCase(),
                          entry.size_bytes === null ? null : formatSize(entry.size_bytes, locale),
                        ]
                          .filter(Boolean)
                          .join(', ')}
                      </p>
                      <p className="inventory-status">
                        <EntryStatus entry={entry} />
                      </p>
                    </div>
                    {entry.state === 'pending_association' &&
                      (entry.associated_unit_count > 0 ? (
                        <Menu
                          label={t('storageFileActions').replace('{name}', entry.relative_path)}
                          items={[{ label: t('addAssociation'), icon: IconLink, onSelect: () => setEntry(entry) }]}
                        />
                      ) : (
                        <Button variant="primary" onClick={() => setEntry(entry)}>
                          {t('associate')}
                        </Button>
                      ))}
                  </li>
                ))}
              </ul>
            ) : (
              <p className="empty-inline">{t('noEntries')}</p>
            )}
            <div className="actions">
              {cursor && (
                <button onClick={() => setCursor(undefined)}>{t('firstPage')}</button>
              )}
              {page.next_cursor && (
                <button onClick={() => setCursor(page.next_cursor ?? undefined)}>
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
function Association({ entry, close }: { entry: Entry; close: () => void }) {
  const { t, locale } = useI18n();
  const [unit, setUnit] = useState<{ id: string; label: string }>();
  const [preview, setPreview] = useState<Preview>();
  const [file, setFile] = useState<FileView>();
  const [error, setError] = useState<unknown>();
  const [busy, setBusy] = useState(false);
  return (
    <div className="association">
      <h3>{t('associate')}</h3>
      <p className="path">
        <PathText path={entry.relative_path} />
      </p>
      <HelpTip>
        <p>{t('helpLink')}</p>
        <p>{t('helpEdition')}</p>
      </HelpTip>
      {file ? (
        <div className="notice" role="status">
          <h3>{t('associated')}</h3>
          <p>{unit?.label}</p>
          <p>
            {file.format.toUpperCase()}, {formatSize(file.size_bytes, locale)}
          </p>
          <button onClick={close}>{t('inventory')}</button>
        </div>
      ) : (
        <>
          {!unit ? (
            <UnitPicker select={setUnit} />
          ) : (
            <>
              <p>
                <strong>{unit.label}</strong>
              </p>
              {preview ? (
                <div className="notice">
                  <h3>{t('preview')}</h3>
                  <p>{t('previewHint')}</p>
                  <p>
                    {t('size')}: {formatSize(preview.source_size, locale)}
                  </p>
                  <p className="path">
                    {t('signature')}: {preview.signature}
                  </p>
                  <button
                    className="primary"
                    disabled={busy}
                    onClick={async () => {
                      setBusy(true);
                      setError(undefined);
                      try {
                        setFile(
                          await request<FileView>(`/library/previews/${preview.id}/accept`, {
                            method: 'POST',
                          }),
                        );
                      } catch (failure) {
                        setError(failure);
                      } finally {
                        setBusy(false);
                      }
                    }}
                  >
                    {t(busy ? 'saving' : 'acceptPreview')}
                  </button>
                </div>
              ) : (
                <button
                  disabled={busy}
                  onClick={async () => {
                    setBusy(true);
                    setError(undefined);
                    try {
                      setPreview(
                        await post<Preview>('/library/previews', {
                          entry_id: entry.id,
                          unit_id: unit.id,
                        } satisfies Schema['PreviewInput']),
                      );
                    } catch (failure) {
                      setError(failure);
                    } finally {
                      setBusy(false);
                    }
                  }}
                >
                  {t(busy ? 'saving' : 'preview')}
                </button>
              )}
            </>
          )}
          <ErrorNotice error={error} />
          <div className="actions">
            <button disabled={busy} onClick={close}>
              {t('cancel')}
            </button>
          </div>
        </>
      )}
    </div>
  );
}
export type UnitSelection = {
  id: string;
  label: string;
  contentType: Schema['Publication']['content_type'];
  title: string;
  language?: string;
};

/* First page on load, more pages appended on request; a new path starts over. */
function usePagedList<T>(path: string) {
  const [version, setVersion] = useState(0);
  const [state, setState] = useState<{
    path: string;
    items: T[];
    next: string | null;
    loading: boolean;
    error?: unknown;
  }>({ path, items: [], next: null, loading: true });
  useEffect(() => {
    const controller = new AbortController();
    setState({ path, items: [], next: null, loading: true });
    request<Page<T>>(path, { signal: controller.signal })
      .then((page) => {
        if (!controller.signal.aborted)
          setState({ path, items: page.items, next: page.next_cursor, loading: false });
      })
      .catch((error) => {
        if (!controller.signal.aborted) setState({ path, items: [], next: null, loading: false, error });
      });
    return () => controller.abort();
  }, [path, version]);
  const current = state.path === path ? state : { path, items: [] as T[], next: null, loading: true };
  async function loadMore() {
    const cursor = current.next;
    if (!cursor || current.loading) return;
    setState((value) => ({ ...value, loading: true, error: undefined }));
    try {
      const page = await request<Page<T>>(`${path}&cursor=${encodeURIComponent(cursor)}`);
      setState((value) =>
        value.path === path
          ? { path, items: [...value.items, ...page.items], next: page.next_cursor, loading: false }
          : value,
      );
    } catch (error) {
      setState((value) => (value.path === path ? { ...value, loading: false, error } : value));
    }
  }
  return {
    ...current,
    firstLoad: current.loading && current.items.length === 0,
    loadMore: () => void loadMore(),
    reload: () => setVersion((value) => value + 1),
  };
}

function ListFooter({ list }: { list: ReturnType<typeof usePagedList<unknown>> }) {
  const { t } = useI18n();
  return (
    <>
      {list.error !== undefined && list.items.length > 0 && <ErrorNotice error={list.error} retry={list.loadMore} />}
      {list.next && (
        <div className="actions choice-more">
          <Button disabled={list.loading} aria-busy={list.loading} onClick={list.loadMore}>
            {t('loadMore')}
          </Button>
        </div>
      )}
    </>
  );
}

const typeNames = { comic: 'typeComic', manga: 'typeManga', magazine: 'typeMagazine' } as const;
function ChoiceRow({
  id,
  title,
  meta,
  cover,
  onChoose,
}: {
  id: string;
  title: string;
  meta?: string;
  cover?: Schema['PublicationSummary'];
  onChoose: () => void;
}) {
  return (
    <li>
      <button
        type="button"
        className="choice-row"
        aria-labelledby={domId('choice', id, 'title')}
        aria-describedby={meta ? domId('choice', id, 'meta') : undefined}
        onClick={onChoose}
      >
        {cover && <Cover fileId={cover.cover_file_id} title={cover.title} contentType={cover.content_type} />}
        <span className="choice-text">
          <strong id={domId('choice', id, 'title')} title={title}>{title}</strong>
          {meta && (
            <span className="choice-meta" id={domId('choice', id, 'meta')}>
              {meta}
            </span>
          )}
        </span>
        <Icon icon={IconChevronRight} size={18} />
      </button>
    </li>
  );
}

/** Publication, then edition, then issue/chapter/volume. The select callback receives the same UnitSelection as before. */
export function UnitPicker({ select }: { select: (unit: UnitSelection) => void }) {
  const { t } = useI18n();
  const [publication, setPublication] = useState<Schema['PublicationSummary']>();
  return (
    <div className="unit-picker">
      <h3>{t(publication ? 'chooseUnit' : 'choosePublication')}</h3>
      {publication ? (
        <EditionUnitChoice
          key={publication.id}
          publication={publication}
          change={() => setPublication(undefined)}
          select={select}
        />
      ) : (
        <PublicationChoice select={setPublication} />
      )}
    </div>
  );
}

function PublicationChoice({ select }: { select: (publication: Schema['PublicationSummary']) => void }) {
  const { t } = useI18n();
  const [q, setQuery] = useState('');
  const params = new URLSearchParams({ limit: '25' });
  if (q) params.set('q', q);
  const list = usePagedList<Schema['PublicationSummary']>(`/publications?${params}`);
  return (
    <>
      <SearchField label={t('searchPublications')} value={q} onChange={(search) => setQuery(search.trim())} autoFocus />
      {list.firstLoad ? (
        <Loading />
      ) : list.error !== undefined && !list.items.length ? (
        <ErrorNotice error={list.error} retry={list.reload} />
      ) : list.items.length ? (
        <ul className="choice-list">
          {list.items.map((item) => (
            <ChoiceRow
              key={item.id}
              id={item.id}
              title={item.title}
              meta={[t(typeNames[item.content_type]), item.run_label].filter(Boolean).join(', ')}
              cover={item}
              onChoose={() => select(item)}
            />
          ))}
        </ul>
      ) : (
        <p className="empty-inline">{t(q ? 'noSearchResults' : 'noChoices')}</p>
      )}
      <ListFooter list={list} />
    </>
  );
}

function EditionUnitChoice({
  publication,
  change,
  select,
}: {
  publication: Schema['PublicationSummary'];
  change: () => void;
  select: (unit: UnitSelection) => void;
}) {
  const { t, locale } = useI18n();
  const editions = usePagedList<Schema['Edition']>(
    `/publications/${encodeURIComponent(publication.id)}/editions?limit=50`,
  );
  const [chosen, setChosen] = useState<string>();
  const edition = editions.items.find((item) => item.id === chosen) ?? editions.items[0];
  const names = editions.items.map((item) => editionLabel(item, locale));
  return (
    <>
      <div className="choice-selected">
        <Cover fileId={publication.cover_file_id} title={publication.title} contentType={publication.content_type} />
        <span className="choice-text">
          <strong>{publication.title}</strong>
          <span className="choice-meta">
            {[t(typeNames[publication.content_type]), publication.run_label].filter(Boolean).join(', ')}
          </span>
        </span>
        <Button onClick={change}>{t('pickerChangePublication')}</Button>
      </div>
      {editions.firstLoad ? (
        <Skeleton variant="line" short />
      ) : editions.error !== undefined && !editions.items.length ? (
        <ErrorNotice error={editions.error} retry={editions.reload} />
      ) : !edition ? (
        <p className="empty-inline">{t('noEditions')}</p>
      ) : (
        <>
          <div className="choice-editions">
            {editions.items.length === 1 ? (
              <p className="choice-meta">
                {t('edition')}: {names[0]}
              </p>
            ) : editions.items.length <= 6 && !editions.next ? (
              <SegmentedControl
                label={t('chooseEdition')}
                value={edition.id}
                options={editions.items.map((item, index) => ({ value: item.id, label: names[index] }))}
                onChange={setChosen}
              />
            ) : (
              <label className="edition-select">
                <span>{t('edition')}</span>
                <select value={edition.id} onChange={(event) => setChosen(event.target.value)}>
                  {editions.items.map((item, index) => (
                    <option key={item.id} value={item.id}>
                      {names[index]}
                    </option>
                  ))}
                </select>
              </label>
            )}
            {editions.items.length > 6 && <ListFooter list={editions} />}
          </div>
          <UnitChoice
            key={edition.id}
            edition={edition}
            choose={(unit) =>
              select({
                id: unit.id,
                contentType: publication.content_type,
                title: publication.title,
                language: edition.language,
                label: `${publication.title} ${unit.label}`.trim(),
              })
            }
          />
        </>
      )}
    </>
  );
}

function UnitChoice({ edition, choose }: { edition: Schema['Edition']; choose: (unit: Schema['Unit']) => void }) {
  const { t, locale } = useI18n();
  const [q, setQuery] = useState('');
  const params = new URLSearchParams({ limit: '50' });
  if (q) params.set('q', q);
  const list = usePagedList<Schema['Unit']>(`/editions/${encodeURIComponent(edition.id)}/units?${params}`);
  return (
    <>
      <SearchField label={t('searchUnits')} value={q} onChange={(search) => setQuery(search.trim())} />
      {list.firstLoad ? (
        <Loading />
      ) : list.error !== undefined && !list.items.length ? (
        <ErrorNotice error={list.error} retry={list.reload} />
      ) : list.items.length ? (
        <ul className="choice-list">
          {list.items.map((unit) => {
            const kind = t(unit.kind);
            const date = formatUnitDate(unit, locale);
            const label = unit.label.toLowerCase();
            return (
              <ChoiceRow
                key={unit.id}
                id={unit.id}
                title={unit.label}
                meta={[
                  label.includes(kind.toLowerCase()) ? undefined : kind,
                  date && !label.includes(date.toLowerCase()) ? date : undefined,
                ]
                  .filter(Boolean)
                  .join(', ')}
                onChoose={() => choose(unit)}
              />
            );
          })}
        </ul>
      ) : (
        <p className="empty-inline">{t(q ? 'noSearchResults' : 'noUnits')}</p>
      )}
      <ListFooter list={list} />
    </>
  );
}
export function LocalSearch({
  label,
  query,
  submit,
}: {
  label: string;
  query: string;
  submit: (query: string) => void;
}) {
  const { t } = useI18n();
  const id = useId();
  const input = useRef<HTMLInputElement>(null);
  const [draft, setDraft] = useState(query);
  useEffect(() => {
    setDraft(query);
    input.current?.setCustomValidity('');
  }, [query]);
  return (
    <form
      role="search"
      aria-label={label}
      className="actions"
      onSubmit={(event) => {
        event.preventDefault();
        const search = draft.trim();
        const valid = new TextEncoder().encode(search).length <= 256 && !/\p{Cc}/u.test(search);
        input.current?.setCustomValidity(valid ? '' : t('invalidLocalSearch'));
        if (!valid) {
          input.current?.reportValidity();
          return;
        }
        submit(search);
      }}
    >
      <label className="field" htmlFor={id}>
        <span>{label}</span>
        <input
          ref={input}
          id={id}
          type="search"
          maxLength={256}
          value={draft}
          onChange={(event) => {
            event.target.setCustomValidity('');
            setDraft(event.target.value);
          }}
        />
      </label>
      <button type="submit">{t('search')}</button>
      {(query || draft) && (
        <button
          type="button"
          onClick={() => {
            input.current?.setCustomValidity('');
            setDraft('');
            submit('');
          }}
        >
          {t('clearSearch')}
        </button>
      )}
    </form>
  );
}
export function Picker<T extends { id: string }>({
  path,
  label,
  text,
  select,
}: {
  path: string;
  label: string;
  text: (item: T) => string;
  select: (item: T) => void;
}) {
  const { t } = useI18n();
  const [cursor, setCursor] = useState<string>();
  const [q, setQuery] = useState('');
  const searchable =
    path === '/publications' ||
    /^\/publications\/[^/]+\/editions$/.test(path) ||
    /^\/editions\/[^/]+\/units$/.test(path);
  const params = new URLSearchParams({ limit: '50' });
  if (cursor) params.set('cursor', cursor);
  if (searchable && q) params.set('q', q);
  const result = useResource<Page<T>>(`${path}?${params}`);
  return (
    <div aria-label={label}>
      {searchable && (
        <LocalSearch
          label={label}
          query={q}
          submit={(search) => {
            setQuery(search);
            setCursor(undefined);
          }}
        />
      )}
      {result.loading ? (
        <Loading />
      ) : result.error ? (
        <ErrorNotice error={result.error} retry={result.reload} />
      ) : (
        result.data && (
          <>
            <ul className="picker-list">
              {result.data.items.map((item) => (
                <li key={item.id}>
                  <button onClick={() => select(item)}>{text(item)}</button>
                </li>
              ))}
            </ul>
            {result.data.items.length === 0 && (
              <p className="muted">{t(q ? 'noSearchResults' : 'noChoices')}</p>
            )}
            <div className="actions">
              {cursor && (
                <button onClick={() => setCursor(undefined)}>{t('firstPage')}</button>
              )}
              {result.data.next_cursor && (
                <button onClick={() => setCursor(result.data?.next_cursor ?? undefined)}>
                  {t('next')}
                </button>
              )}
            </div>
          </>
        )
      )}
    </div>
  );
}
