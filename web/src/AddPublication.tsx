import { useEffect, useRef, useState, type ChangeEvent, type FormEvent, type KeyboardEvent, type ReactNode } from 'react';
import {
  IconBook,
  IconBook2,
  IconChevronLeft,
  IconNews,
  IconPencil,
  IconRefresh,
  IconSearch,
} from '@tabler/icons-react';
import { ApiError, post, request, type Schema } from './lib/api/client';
import { useI18n, type MessageKey } from './i18n';
import { Picker } from './Storage';
import { Button, ErrorNotice, Field, Icon, Loading, PageHeader, useResource } from './ui';
import { clearDraft, commonLanguages, emptyDetails, readDraft, saveDraft, validId, type AddDraft, type Details, type Kind, type Origin, type Recovery, type Step } from './addDraft';
import './styles/add.css';

type Candidate = Schema['MetadataCandidate'];
type Choice = Schema['IntegrationChoice'];
const kinds: readonly Kind[] = ['comic', 'manga', 'magazine'];
const kindIcons = { comic: IconBook, manga: IconBook2, magazine: IconNews } as const;
const providerNames: Record<Candidate['provider'], string> = {
  comic_vine: 'Comic Vine',
  manga_updates: 'MangaUpdates',
  manga_dex: 'MangaDex',
  local_manual: 'Manual',
  issn: 'ISSN',
};
const editionHints: Record<Kind, MessageKey> = {
  comic: 'addEditionHintComic',
  manga: 'addEditionHintManga',
  magazine: 'addEditionHintMagazine',
};

/* Content type, source and query live in the hash so a reload keeps the search. */
function syncUrl(params: Record<string, string | undefined>) {
  const next = new URLSearchParams();
  for (const [key, item] of Object.entries(params)) if (item) next.set(key, item);
  const search = next.toString();
  history.replaceState(history.state, '', `#/new${search ? `?${search}` : ''}`);
}
const isUncertain = (error: unknown) =>
  !(error instanceof ApiError) || error.status === 0 || error.status >= 500 || error.code === 'invalid_response';

export function AddPublication({ query, isAdmin, userId }: { query: URLSearchParams; isAdmin: boolean; userId: string }) {
  const { t } = useI18n();
  const choices = useResource<Schema['IntegrationChoiceList']>('/search/integrations');
  const [kind, setKind] = useState<Kind | undefined>(() => kinds.find((item) => item === query.get('kind')));
  const [manualPath, setManualPath] = useState(query.get('manual') === '1');
  const [step, setStep] = useState<Step>(kind ? (manualPath ? 3 : 2) : 1);
  const [search, setSearch] = useState({ q: query.get('q') ?? '', source: query.get('source') ?? '' });
  const [origin, setOrigin] = useState<Origin | undefined>(kind && manualPath ? { manual: true } : undefined);
  const [details, setDetails] = useState<Details>(emptyDetails);
  const [recovery, setRecovery] = useState<Recovery>({ editionDone: false });
  const [autoLink, setAutoLink] = useState(false);
  const [stored] = useState(() => {
    try { return { draft: readDraft(userId, sessionStorage), failed: false }; }
    catch { return { draft: undefined, failed: true }; }
  });
  const [pendingDraft, setPendingDraft] = useState(stored.draft);
  const [invalidDraft, setInvalidDraft] = useState(stored.failed);
  const [storageFailed, setStorageFailed] = useState(stored.failed);
  const [discarding, setDiscarding] = useState(false);
  const snapshot = useRef<AddDraft>({ version: 2, userId, kind, manualPath, step, search, origin, details, recovery });
  snapshot.current = { version: 2, userId, kind, manualPath, step, search, origin, details, recovery };
  const heading = useRef<HTMLDivElement>(null);
  const moved = useRef(false);

  useEffect(() => {
    if (pendingDraft || invalidDraft) return;
    syncUrl({ kind, source: search.source, q: search.q, manual: manualPath ? '1' : undefined });
  }, [kind, search, manualPath, pendingDraft, invalidDraft]);
  useEffect(() => {
    const next = snapshot.current;
    if (pendingDraft || invalidDraft || next.recovery.complete) return;
    if (next.kind || next.search.q || next.recovery.attempted || next.recovery.publicationId) persist(next);
  }, [kind, manualPath, step, search, origin, details, recovery, pendingDraft, invalidDraft]);
  useEffect(() => {
    if (!moved.current) {
      moved.current = true;
      return;
    }
    heading.current?.querySelector<HTMLElement>('[aria-current="step"] h2')?.focus();
  }, [step]);

  const items = choices.data?.items ?? [];
  const sources = kind
    ? items.filter((choice) => choice.metadata_lookup && choice.content_types.includes(kind))
    : [];
  const hasProwlarr = items.some((choice) => choice.kind === 'prowlarr' && choice.release_search);
  const locked = Boolean(recovery.publicationId || recovery.attempted);

  function persist(next: AddDraft) {
    snapshot.current = next;
    try {
      saveDraft(next, sessionStorage);
      setStorageFailed(false);
      return true;
    } catch {
      setStorageFailed(true);
      return false;
    }
  }
  function rememberDetails(next: Details) {
    persist({ ...snapshot.current, details: next });
    setDetails(next);
  }
  function rememberRecovery(next: Recovery) {
    const saved = persist({ ...snapshot.current, step: 4, recovery: next });
    setRecovery(next);
    return saved;
  }
  function linkExisting(publication: Schema['Publication']) {
    if (snapshot.current.recovery.publicationId || snapshot.current.recovery.attempted) return true;
    const next: Recovery = { publicationId: publication.id, editionDone: false, existingLink: true, existingTitle: publication.title };
    const previous = snapshot.current;
    if (!persist({ ...previous, step: 4, recovery: next })) {
      snapshot.current = previous;
      return false;
    }
    setRecovery(next);
    setAutoLink(true);
    setStep(4);
    return true;
  }
  function discard() {
    try {
      clearDraft(userId, sessionStorage);
      setPendingDraft(undefined);
      setInvalidDraft(false);
      setStorageFailed(false);
      setDiscarding(false);
      setKind(undefined);
      setManualPath(false);
      setOrigin(undefined);
      setDetails(emptyDetails);
      setSearch({ q: '', source: '' });
      setRecovery({ editionDone: false });
      setAutoLink(false);
      setStep(1);
    } catch { setStorageFailed(true); }
  }
  function resume(draft: AddDraft) {
    setAutoLink(false);
    setKind(draft.kind);
    setManualPath(draft.manualPath);
    setSearch(draft.search);
    setOrigin(draft.origin);
    setDetails(draft.details);
    setRecovery(draft.recovery);
    setStep(draft.step);
    setPendingDraft(undefined);
  }

  function chooseKind(next: Kind) {
    if (next !== kind) {
      setKind(next);
      setSearch({ q: search.q, source: '' });
      setOrigin(manualPath ? { manual: true } : undefined);
      setDetails(emptyDetails);
    }
    setStep(manualPath ? 3 : 2);
  }
  function chooseOrigin(next: Origin) {
    setManualPath(next.manual);
    setOrigin(next);
    setDetails(
      next.manual
        ? { ...emptyDetails, title: search.q }
        : { ...emptyDetails, title: next.candidate.title, run_label: next.candidate.date ?? '' },
    );
    setStep(3);
  }

  const editionText = [details.language, details.region, details.publisher].filter(Boolean).join(', ');
  const originText = origin
    ? origin.manual
      ? t('addManualEntry')
      : `${providerNames[origin.candidate.provider]} #${origin.candidate.external_id}`
    : '';
  const totalSteps = manualPath ? 3 : 4;
  const shownStep = (index: Step) => manualPath && index > 2 ? index - 1 : index;

  return (
    <>
      <a className="back" href="#/">
        <Icon icon={IconChevronLeft} size={18} />
        {t('back')}
      </a>
      <PageHeader
        title={t('addPublication')}
        meta={!pendingDraft && !invalidDraft ? t('addStepProgress').replace('{n}', String(shownStep(step))).replace('{total}', String(totalSteps)) : undefined}
      />
      {storageFailed && <p className="notice error" role="alert">{t('addDraftStorageFailed')}</p>}
      {pendingDraft || invalidDraft ? (
        <section className="add-draft" aria-labelledby="add-draft-title">
          <h2 id="add-draft-title">{t('addDraftFound')}</h2>
          {pendingDraft?.details.title && <p><strong>{pendingDraft.details.title}</strong></p>}
          <p>{t('addDraftBody')}</p>
          {Boolean(pendingDraft?.recovery.publicationId || pendingDraft?.recovery.attempted) && <p>{t('addDraftDiscardRecovery')}</p>}
          <div className="actions">
            {pendingDraft && <Button variant="primary" onClick={() => resume(pendingDraft)}>{t('addDraftResume')}</Button>}
            <Button onClick={() => {
              if (invalidDraft || pendingDraft?.recovery.publicationId || pendingDraft?.recovery.attempted) setDiscarding(true);
              else discard();
            }}>{t('addDraftDiscard')}</Button>
          </div>
          {discarding && <div className="notice">
            <p>{t('addDraftDiscardRecovery')}</p>
            <div className="actions">
              <Button variant="danger" onClick={discard}>{t('addDraftDiscard')}</Button>
              <Button onClick={() => setDiscarding(false)}>{t('cancel')}</Button>
            </div>
          </div>}
        </section>
      ) : <div ref={heading}>
        <ol className="add-steps">
          <StepSection
            index={1}
            number={shownStep(1)}
            total={totalSteps}
            step={step}
            title={t('addStepType')}
            done={kind ? t(kind) : undefined}
            change={locked ? undefined : () => setStep(1)}
          >
            <KindPicker value={kind} choose={chooseKind} />
          </StepSection>

          {!manualPath && <StepSection
            index={2}
            number={shownStep(2)}
            total={totalSteps}
            step={step}
            title={t('addStepFind')}
            done={originText || undefined}
            change={locked ? undefined : () => setStep(2)}
          >
            {kind &&
              (choices.loading ? (
                <Loading />
              ) : choices.error ? (
                <ErrorNotice error={choices.error} retry={choices.reload} />
              ) : (
                <ProviderSearch
                  kind={kind}
                  sources={sources}
                  isAdmin={isAdmin}
                  initial={search}
                  remember={setSearch}
                  pick={(candidate) => chooseOrigin({ manual: false, candidate })}
                  manual={() => chooseOrigin({ manual: true })}
                />
              ))}
            <div className="actions">
              <Button onClick={() => setStep(1)}>{t('addBack')}</Button>
            </div>
          </StepSection>}

          <StepSection
            index={3}
            number={shownStep(3)}
            total={totalSteps}
            step={step}
            title={t('addStepEdition')}
            done={details.title ? [details.title, details.run_label, editionText].filter(Boolean).join(' / ') : undefined}
            change={locked ? undefined : () => setStep(3)}
          >
            {manualPath && (
              <Button variant="ghost" icon={IconSearch} onClick={() => {
                setManualPath(false);
                setOrigin(undefined);
                setStep(2);
              }}>
                {t('addSearchInstead')}
              </Button>
            )}
            {kind && origin && (
              <EditionForm
                kind={kind}
                origin={origin}
                details={details}
                remember={rememberDetails}
                linkExisting={linkExisting}
                back={() => setStep(manualPath ? 1 : 2)}
                save={(next) => {
                  setDetails(next);
                  setStep(4);
                }}
              />
            )}
          </StepSection>

          <StepSection index={4} number={shownStep(4)} total={totalSteps} step={step} title={t(recovery.existingLink ? 'addLinkExisting' : 'addStepSummary')}>
            {kind && origin && (
              <Summary
                kind={kind}
                origin={origin}
                details={details}
                originText={originText}
                editionText={editionText}
                recovery={recovery}
                autoLink={autoLink}
                rememberRecovery={rememberRecovery}
                complete={() => {
                  try { clearDraft(userId, sessionStorage); return true; }
                  catch { setStorageFailed(true); return false; }
                }}
                back={() => setStep(3)}
                monitorHint={!choices.loading && !hasProwlarr}
                isAdmin={isAdmin}
              />
            )}
          </StepSection>
        </ol>
      </div>}
    </>
  );
}

/* Radio group with no default: arrow keys move focus (starting from the first option), and click, Enter,
   or Space commits the choice and advances. Only a committed choice is checked. */
function KindPicker({ value, choose }: { value?: Kind; choose: (kind: Kind) => void }) {
  const { t } = useI18n();
  const [focused, setFocused] = useState<Kind>(value ?? kinds[0]);
  const refs = useRef<(HTMLButtonElement | null)[]>([]);
  function onKeyDown(event: KeyboardEvent<HTMLButtonElement>, index: number) {
    const last = kinds.length - 1;
    const next =
      event.key === 'ArrowLeft' || event.key === 'ArrowUp'
        ? index === 0 ? last : index - 1
        : event.key === 'ArrowRight' || event.key === 'ArrowDown'
          ? index === last ? 0 : index + 1
          : event.key === 'Home'
            ? 0
            : event.key === 'End'
              ? last
              : undefined;
    if (next === undefined) return;
    event.preventDefault();
    setFocused(kinds[next]);
    refs.current[next]?.focus();
  }
  return (
    <div className="add-kinds" role="radiogroup" aria-label={t('contentType')}>
      {kinds.map((item, index) => (
        <button
          key={item}
          ref={(node) => {
            refs.current[index] = node;
          }}
          type="button"
          role="radio"
          aria-checked={item === value}
          tabIndex={item === focused ? 0 : -1}
          onClick={() => {
            setFocused(item);
            choose(item);
          }}
          onKeyDown={(event) => onKeyDown(event, index)}
        >
          <Icon icon={kindIcons[item]} size={22} />
          {t(item)}
        </button>
      ))}
    </div>
  );
}

function StepSection({
  index,
  number,
  total,
  step,
  title,
  done,
  change,
  children,
}: {
  index: Step;
  number: number;
  total: number;
  step: Step;
  title: string;
  done?: string;
  change?: () => void;
  children: ReactNode;
}) {
  const { t } = useI18n();
  const state = index === step ? 'current' : index < step ? 'done' : 'upcoming';
  return (
    <li className={`add-step ${state}`} aria-current={state === 'current' ? 'step' : undefined}>
      <div className="add-step-head">
        <span className="add-step-number" aria-hidden="true">
          {number}
        </span>
        <div className="add-step-title">
          <h2 tabIndex={-1}>
            <span className="sr-only">{t('addStepOf').replace('{n}', String(number)).replace('{total}', String(total))} </span>
            {title}
          </h2>
          {state === 'done' && done && <p className="add-step-done">{done}</p>}
          {state === 'upcoming' && index === step + 1 && <p className="add-step-next">{t('addStepNext')}</p>}
        </div>
        {state === 'done' && change && (
          <Button size="sm" variant="ghost" icon={IconPencil} onClick={change}>
            {t('addChange')}
          </Button>
        )}
      </div>
      {state === 'current' && <div className="add-step-body">{children}</div>}
    </li>
  );
}

type SearchRun = { integration_id: string; query: string; page: number; limit: number };
function ProviderSearch({
  kind,
  sources,
  isAdmin,
  initial,
  remember,
  pick,
  manual,
}: {
  kind: Kind;
  sources: Choice[];
  isAdmin: boolean;
  initial: { q: string; source: string };
  remember: (value: { q: string; source: string }) => void;
  pick: (candidate: Candidate) => void;
  manual: () => void;
}) {
  const { t } = useI18n();
  const [text, setText] = useState(initial.q);
  const [sourceId, setSourceId] = useState(
    sources.find((source) => source.id === initial.source)?.id ?? sources[0]?.id ?? '',
  );
  const [run, setRun] = useState<SearchRun>();
  const [result, setResult] = useState<Schema['MetadataPage']>();
  const [error, setError] = useState<unknown>();
  const [busy, setBusy] = useState(false);
  const active = useRef<AbortController | undefined>(undefined);
  const source = sources.find((item) => item.id === (run?.integration_id ?? sourceId));
  const sourceLabel = source?.label ?? '';

  async function execute(input: SearchRun) {
    active.current?.abort();
    const controller = new AbortController();
    active.current = controller;
    setBusy(true);
    setError(undefined);
    setResult(undefined);
    setRun(input);
    remember({ q: input.query, source: input.integration_id });
    try {
      const params = new URLSearchParams({
        integration_id: input.integration_id,
        query: input.query,
        page: String(input.page),
        limit: String(input.limit),
      });
      const page = await request<Schema['MetadataPage']>(`/search/metadata?${params}`, {
        signal: controller.signal,
      });
      if (!controller.signal.aborted) setResult(page);
    } catch (failure) {
      if (!controller.signal.aborted) setError(failure);
    } finally {
      if (active.current === controller) {
        active.current = undefined;
        setBusy(false);
      }
    }
  }
  useEffect(() => {
    if (initial.q.trim() && sourceId) void execute({ integration_id: sourceId, query: initial.q.trim(), page: 1, limit: 20 });
    return () => active.current?.abort();
    // Once on mount: a reload repeats the search kept in the URL.
  }, []);

  function onSubmit(event: FormEvent<HTMLFormElement>) {
    event.preventDefault();
    const q = text.trim();
    if (!q || !sourceId) return;
    void execute({ integration_id: sourceId, query: q, page: 1, limit: 20 });
  }

  const manualButton = (
    <Button icon={IconPencil} onClick={manual}>
      {t('addCreateManually')}
    </Button>
  );

  if (!sources.length)
    return (
      <div className="notice">
        <p>{t('addNoSource').replace('{type}', t(kind))}</p>
        <div className="actions">
          {manualButton}
          {isAdmin ? (
            <a className="button" href="#/settings?section=sources">
              {t('addSetUpSource')}
            </a>
          ) : <span>{t('addAskAdminSource')}</span>}
        </div>
      </div>
    );

  return (
    <>
      <p className="muted">{t('addFindHint')}</p>
      <form className="add-search" role="search" onSubmit={onSubmit}>
        {sources.length > 1 && (
          <label className="field">
            <span>{t('source')}</span>
            <select value={sourceId} onChange={(event) => setSourceId(event.target.value)}>
              {sources.map((item) => (
                <option key={item.id} value={item.id}>
                  {item.label}
                </option>
              ))}
            </select>
          </label>
        )}
        <label className="field add-query">
          <span>{sources.length > 1 ? t('addSearchLabel') : t('addSearchOn').replace('{source}', sources[0].label)}</span>
          <input
            type="search"
            name="query"
            value={text}
            maxLength={512}
            required
            autoComplete="off"
            spellCheck={false}
            placeholder={t('addSearchPlaceholder')}
            onChange={(event) => setText(event.target.value)}
          />
        </label>
        <Button type="submit" variant="primary" icon={IconSearch} disabled={busy}>
          {t('search')}
        </Button>
      </form>
      {busy && <Loading />}
      {error !== undefined && !busy && (
        <div className="notice error" role="alert">
          <p>
            <strong>{t('addSearchFailed').replace('{source}', sourceLabel)}</strong>
          </p>
          <p>{failureReason(error, sourceLabel, t)}</p>
          {error instanceof ApiError && error.trace && (
            <small>
              {t('trace')}: {error.trace}
            </small>
          )}
          <div className="actions">
            <Button icon={IconRefresh} onClick={() => run && void execute(run)}>
              {t('retry')}
            </Button>
            {manualButton}
          </div>
        </div>
      )}
      {result && !busy && (
        <>
          <p className="muted" role="status">
            {result.candidates.length
              ? t('addMatches').replace('{count}', String(result.total)).replace('{source}', sourceLabel)
              : t('addNoMatches').replace('{source}', sourceLabel)}
          </p>
          {result.candidates.length > 0 && (
            <ul className="add-candidates">
              {result.candidates.map((item) => (
                <li key={`${item.provider}/${item.external_id}`}>
                  <div>
                    <strong>{item.title}</strong>
                    <span className="muted">
                      {[t(item.content_type), item.date ?? t('addYearUnknown'), providerNames[item.provider]].join(' / ')}
                    </span>
                  </div>
                  <Button size="sm" onClick={() => pick(item)}>
                    {t('addUseThis')}
                  </Button>
                </li>
              ))}
            </ul>
          )}
          {(run?.page ?? 1) > 1 || result.next_page !== null ? (
            <div className="actions">
              <Button
                disabled={!run || run.page === 1}
                onClick={() => run && void execute({ ...run, page: run.page - 1, limit: result.page_size || run.limit })}
              >
                {t('previous')}
              </Button>
              <Button
                disabled={result.next_page === null}
                onClick={() =>
                  run &&
                  result.next_page !== null &&
                  void execute({ ...run, page: result.next_page, limit: result.page_size || run.limit })
                }
              >
                {t('next')}
              </Button>
            </div>
          ) : null}
          <p className="add-manual-line">
            {t('addNotListed')} {manualButton}
          </p>
        </>
      )}
      {!result && error === undefined && !busy && <div className="actions">{manualButton}</div>}
    </>
  );
}

function failureReason(error: unknown, source: string, t: (key: MessageKey) => string) {
  if (!(error instanceof ApiError)) return t('requestFailed');
  const key: MessageKey | undefined =
    error.code === 'provider_unavailable'
      ? 'addFailUnavailable'
      : error.code === 'provider_cooldown'
        ? 'addFailCooldown'
        : error.code === 'not_configured'
          ? 'addFailNotConfigured'
          : error.code === 'unsupported'
            ? 'addFailUnsupported'
            : error.code === 'network_error'
              ? 'networkError'
              : undefined;
  return key ? t(key).replace('{source}', source) : error.message || t('requestFailed');
}

function EditionForm({
  kind,
  origin,
  details,
  remember,
  linkExisting,
  back,
  save,
}: {
  kind: Kind;
  origin: Origin;
  details: Details;
  remember: (details: Details) => void;
  linkExisting: (publication: Schema['Publication']) => boolean;
  back: () => void;
  save: (details: Details) => void;
}) {
  const { t } = useI18n();
  const [linking, setLinking] = useState(false);
  const [linkError, setLinkError] = useState<unknown>();
  const edit = (field: keyof Details) => (event: ChangeEvent<HTMLInputElement>) =>
    remember({ ...details, [field]: event.target.value });
  function onSubmit(event: FormEvent<HTMLFormElement>) {
    event.preventDefault();
    const data = new FormData(event.currentTarget);
    const read = (key: keyof Details) => String(data.get(key) ?? details[key]).trim();
    save({
      title: read('title'),
      run_label: read('run_label'),
      sort_title: read('sort_title'),
      known_unit_count: read('known_unit_count'),
      language: read('language'),
      region: read('region'),
      publisher: read('publisher'),
    });
  }
  const runField = kind !== 'magazine' && (
    <Field
      name="run_label"
      label={kind === 'comic' ? t('runLabel') : t('addStartYear')}
      value={details.run_label}
      onChange={edit('run_label')}
      maxLength={1000}
    />
  );
  const languageField = (
    <LanguagePicker value={details.language} change={(language) => remember({ ...details, language })} />
  );
  const regionField = (
    <Field
      name="region"
      label={t('region')}
      hint={t('addRegionHint')}
      value={details.region}
      onChange={edit('region')}
      maxLength={100}
    />
  );
  return (
    <>
      <p className="muted">{t(editionHints[kind])}</p>
      {!origin.manual && (
        <p className="add-origin">
          {t('addFromSource')
            .replace('{source}', providerNames[origin.candidate.provider])
            .replace('{id}', origin.candidate.external_id)}
        </p>
      )}
      <form className="editor add-edition" onSubmit={onSubmit}>
        <Field name="title" label={t('title')} value={details.title} onChange={edit('title')} required maxLength={1000} />
        {kind === 'comic' && runField}
        {kind === 'magazine' ? (
          <>
            {regionField}
            {languageField}
          </>
        ) : (
          <>
            {languageField}
            {regionField}
          </>
        )}
        {kind === 'manga' && runField}
        <Field name="publisher" label={t('publisher')} value={details.publisher} onChange={edit('publisher')} maxLength={1000} />
        <details className="add-more" open={Boolean(details.sort_title || details.known_unit_count)}>
          <summary>{t('addMoreDetails')}</summary>
          <Field name="sort_title" label={t('sortTitle')} value={details.sort_title} onChange={edit('sort_title')} maxLength={1000} />
          <Field
            name="known_unit_count"
            label={kind === 'manga' ? t('addCountVolumes') : t('addCountIssues')}
            hint={t('addCountHint')}
            value={details.known_unit_count}
            onChange={edit('known_unit_count')}
            type="number"
            min="0"
            step="1"
            max={Number.MAX_SAFE_INTEGER}
          />
        </details>
        <div className="actions">
          <Button type="submit" variant="primary">
            {t('addContinue')}
          </Button>
          <Button onClick={back}>{t('addBack')}</Button>
        </div>
      </form>
      {!origin.manual && (
        <div className="add-link-existing">
          {linking ? (
            <>
              <h3>{t('addLinkPick')}</h3>
              <Picker<Schema['Publication']>
                path="/publications"
                label={t('choosePublication')}
                text={(item) => [item.title, item.run_label, t(item.content_type)].filter(Boolean).join(' / ')}
                select={(publication) => {
                  setLinkError(undefined);
                  if (!linkExisting(publication)) setLinkError(new Error(t('addDraftStorageFailed')));
                }}
              />
              <ErrorNotice error={linkError} />
              <Button onClick={() => setLinking(false)}>{t('cancel')}</Button>
            </>
          ) : (
            <p>
              {t('addAlreadyOwned')}{' '}
              <Button size="sm" variant="ghost" onClick={() => setLinking(true)}>
                {t('addLinkExisting')}
              </Button>
            </p>
          )}
        </div>
      )}
    </>
  );
}

function LanguagePicker({ value, change }: { value: string; change: (language: string) => void }) {
  const { t, locale } = useI18n();
  const [other, setOther] = useState(Boolean(value && !commonLanguages.some((code) => code === value)));
  const names = new Intl.DisplayNames([locale], { type: 'language' });
  return (
    <div className="add-language">
      <label className="field" htmlFor="add-language-choice">
        <span>{t('language')} *</span>
        <select
          id="add-language-choice"
          value={other ? 'other' : value}
          required
          onChange={(event) => {
            const next = event.target.value;
            setOther(next === 'other');
            change(next === 'other' ? '' : next);
          }}
        >
          <option value="">{t('addLanguageChoose')}</option>
          {commonLanguages.map((code) => <option key={code} value={code}>{names.of(code)}</option>)}
          <option value="other">{t('addLanguageOther')}</option>
        </select>
      </label>
      {other ? (
        <Field name="language" label={t('addLanguageCode')} hint={t('languageHint')} value={value}
          onChange={(event) => change(event.target.value)} required maxLength={100} />
      ) : <input type="hidden" name="language" value={value} />}
    </div>
  );
}

function Summary({
  kind,
  origin,
  details,
  originText,
  editionText,
  recovery,
  autoLink,
  rememberRecovery,
  complete,
  monitorHint,
  isAdmin,
  back,
}: {
  kind: Kind;
  origin: Origin;
  details: Details;
  originText: string;
  editionText: string;
  recovery: Recovery;
  autoLink: boolean;
  rememberRecovery: (recovery: Recovery) => boolean;
  complete: () => boolean;
  monitorHint: boolean;
  isAdmin: boolean;
  back: () => void;
}) {
  const { t } = useI18n();
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<unknown>();
  const active = useRef(false);
  const uncertain = recovery.attempted === 'publication';
  const partial = recovery.publicationId ? { id: recovery.publicationId, retry: !recovery.attempted && !recovery.complete } : undefined;
  const rows: [string, string][] = recovery.existingLink ? [
    [t('title'), recovery.existingTitle ?? ''],
    [t('addMetadata'), originText],
  ] : [
    [t('contentType'), t(kind)],
    [t('title'), details.title],
    ...(details.run_label ? ([[kind === 'comic' ? t('runLabel') : t('addStartYear'), details.run_label]] as [string, string][]) : []),
    [t('addSummaryEdition'), editionText],
    [t('addMetadata'), originText],
    ...(details.sort_title ? ([[t('sortTitle'), details.sort_title]] as [string, string][]) : []),
    ...(details.known_unit_count
      ? ([[kind === 'manga' ? t('addCountVolumes') : t('addCountIssues'), details.known_unit_count]] as [string, string][])
      : []),
    [t('addStepMonitor'), t('addMonitorLater')],
  ];
  useEffect(() => {
    if (autoLink && recovery.existingLink) void create();
    // Only an explicit picker choice starts a link; restoring a draft never submits it.
  }, []);

  async function create() {
    if (active.current || recovery.attempted || recovery.complete) return;
    active.current = true;
    setBusy(true);
    setError(undefined);
    let progress = { ...recovery };
    const checkpoint = (next: Recovery) => {
      progress = next;
      if (!rememberRecovery(next)) throw new Error(t('addDraftStorageFailed'));
    };
    async function attempt(operation: NonNullable<Recovery['attempted']>, work: () => Promise<void>) {
      const previous = progress;
      progress = { ...progress, attempted: operation };
      if (!rememberRecovery(progress)) {
        progress = previous;
        rememberRecovery(previous);
        throw new Error(t('addDraftStorageFailed'));
      }
      try { await work(); }
      catch (failure) {
        if (!isUncertain(failure)) {
          progress = { ...progress, attempted: undefined };
          rememberRecovery(progress);
        }
        throw failure;
      }
    }
    try {
      if (!progress.publicationId && !progress.existingLink) {
        const input: Schema['NewPublication'] = {
          title: details.title,
          content_type: kind,
          sort_title: details.sort_title || null,
          run_label: details.run_label || null,
          known_unit_count: details.known_unit_count ? Number(details.known_unit_count) : null,
        };
        await attempt('publication', async () => {
          const publication = await post<Schema['Publication']>('/publications', input);
          if (!publication || !validId(publication.id)) throw new ApiError(200, 'invalid_response', '');
          checkpoint({ publicationId: publication.id, editionDone: false });
        });
      }
      const id = progress.publicationId!;
      if (!progress.editionDone && !progress.existingLink) {
        await attempt('edition', async () => {
          const edition: Schema['NewEdition'] = {
            publication_id: id,
            language: details.language,
            region: details.region || null,
            publisher: details.publisher || null,
          };
          const saved = await post<Schema['Edition']>('/editions', edition);
          if (!saved || !validId(saved.id) || saved.publication_id !== id) throw new ApiError(200, 'invalid_response', '');
          checkpoint({ publicationId: id, editionDone: true });
        });
      }
      if (!origin.manual) {
        await attempt('link', async () => {
          const link: Schema['NewProviderLink'] = {
            provider: origin.candidate.provider,
            external_id: origin.candidate.external_id,
            publication_id: id,
          };
          const saved = await post<Schema['ProviderLink']>('/provider-links', link);
          if (!saved || !validId(saved.id) || saved.publication_id !== id || saved.provider !== link.provider || saved.external_id !== link.external_id)
            throw new ApiError(200, 'invalid_response', '');
          checkpoint({ ...progress, publicationId: id, attempted: undefined, complete: true });
        });
      } else {
        checkpoint({ publicationId: id, editionDone: true, complete: true });
      }
      if (complete()) location.hash = `/publication/${id}`;
    } catch (failure) {
      setError(failure);
    } finally {
      active.current = false;
      setBusy(false);
    }
  }

  return (
    <div aria-busy={busy}>
      <dl className="add-summary">
        {rows.map(([label, text]) => (
          <div key={label}>
            <dt>{label}</dt>
            <dd>{text}</dd>
          </div>
        ))}
      </dl>
      {!recovery.existingLink && <p className="muted">{t('addMonitorBody')}</p>}
      {!recovery.existingLink && monitorHint && <p className="notice info">
        {t('addMonitorNeedsProwlarr')}{' '}
        {isAdmin ? <a href="#/settings?section=sources">{t('addSetUpSource')}</a> : t('addAskAdminSource')}
      </p>}
      {busy ? <Loading /> : uncertain ? (
        <div className="notice error" role="alert">
          <p>{t('addCreateUncertain')}</p>
          <a className="button" href={`#/?${new URLSearchParams({ q: details.title })}`}>
            {t('addCheckLibrary')}
          </a>
        </div>
      ) : partial ? (
        <>
          {!recovery.complete && <p className="notice error" role="alert">{t(recovery.existingLink ? 'addExistingLinkRecovery' : 'addPartial')}</p>}
          <ErrorNotice error={error} />
          <div className="actions">
            <a className="button" href={`#/publication/${partial.id}`}>
              {t('openPublication')}
            </a>
            {partial.retry && (
              <Button icon={IconRefresh} disabled={busy} onClick={() => void create()}>
                {t('retry')}
              </Button>
            )}
          </div>
        </>
      ) : (
        <>
          <ErrorNotice error={error} />
          <div className="actions">
            <Button variant="primary" disabled={busy} onClick={() => void create()}>
              {busy ? t('saving') : t('addCreate')}
            </Button>
            <Button disabled={busy} onClick={back}>
              {t('addBack')}
            </Button>
          </div>
        </>
      )}
    </div>
  );
}
