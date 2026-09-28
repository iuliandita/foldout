import { useEffect, useRef, useState } from 'react';
import { IconArchive, IconDownload, IconListSearch, IconRefresh, IconSearch } from '@tabler/icons-react';
import { useI18n, type Locale, type MessageKey } from './i18n';
import { ApiError, post, request, type Schema } from './lib/api/client';
import {
  Button,
  EmptyState,
  ErrorNotice,
  Loading,
  formatSize,
  PageHeader,
  SaveForm,
  StatusBadge,
  Tabs,
  useResource,
  value,
  type StatusKind,
  type TabItem,
} from './ui';
import { BackLink } from './Activity';
import { UnitPicker, type UnitSelection } from './Storage';
import { Monitors } from './Monitors';
import { DestinationFields, readDestination } from './Acquisitions';
import { DirectSearch, routeUnitId } from './Direct';
import { ArchiveSearch } from './ArchiveSearch';
import { editionLabel, formatUnitDate } from './format';
import './styles/releases.css';

type IntegrationChoice = Schema['IntegrationChoice'];
type SelectionRelease = Pick<Schema['Release'], 'release_handle' | 'title' | 'content_type' | 'size_bytes' | 'protocol'>;
type SelectedRelease = { release: SelectionRelease };
type AcquisitionAttempt = {
  decisionKey: string;
  acquisitionKey: string;
  decisionInput: Schema['ReleaseDecisionRequest'];
  decision?: Schema['ReleaseDecision'];
  input: Schema['AcquisitionRequest'];
  client: string;
};
type RejectionAttempt = { key: string; input: Schema['ReleaseDecisionRequest'] };
type RevocationAttempt = { key: string; id: string };
type PendingSelection = {
  version: 1;
  userId: string;
  unit: UnitSelection;
  release: SelectionRelease;
  assessment: Schema['ReleaseAssessment'];
  attempt?: AcquisitionAttempt;
  rejectionAttempt?: RejectionAttempt;
  revocationAttempt?: RevocationAttempt;
};
const pendingSelectionKey = (userId: string) => `library:pending-release-selection:${userId}`;
const isRecord = (value: unknown): value is Record<string, unknown> => Boolean(value) && typeof value === 'object';
const isString = (value: unknown): value is string => typeof value === 'string' && value.length > 0;
function isPendingSelection(value: unknown, userId: string, unitId: string): value is PendingSelection {
  if (!isRecord(value) || value.version !== 1 || value.userId !== userId || !isRecord(value.unit) || value.unit.id !== unitId ||
    !isRecord(value.release) || !isString(value.release.release_handle) || !isString(value.release.title) ||
    !['comic', 'manga', 'magazine'].includes(String(value.release.content_type)) || typeof value.release.size_bytes !== 'number' ||
    !isString(value.release.protocol) || !isRecord(value.assessment) || !isString(value.assessment.assessment_id) ||
    typeof value.assessment.assessment_expires_at !== 'number' || !isRecord(value.assessment.evaluation) ||
    !['eligible', 'unknown', 'ineligible'].includes(String(value.assessment.evaluation.eligibility)) ||
    !Array.isArray(value.assessment.evaluation.reasons) || !isRecord(value.assessment.target) ||
    !isRecord(value.assessment.target.unit) || value.assessment.target.unit.id !== unitId ||
    !isRecord(value.assessment.target.publication) || !isRecord(value.assessment.target.edition)) return false;
  const attempt = value.attempt;
  const rejection = value.rejectionAttempt;
  const revocation = value.revocationAttempt;
  if (!attempt && !rejection && !revocation) return false;
  if (attempt && (!isRecord(attempt) || 'decision' in attempt || !isString(attempt.decisionKey) || !isString(attempt.acquisitionKey) || !isString(attempt.client) || !isRecord(attempt.decisionInput) || attempt.decisionInput.assessment_id !== value.assessment.assessment_id || attempt.decisionInput.action !== 'selected' || !isRecord(attempt.input) || attempt.input.release_handle !== value.release.release_handle || !isString(attempt.input.client_id) || attempt.input.unit_id !== unitId)) return false;
  const restoredAttempt = attempt as AcquisitionAttempt | undefined;
  if (restoredAttempt && restoredAttempt.decisionInput.acknowledged_assessment_id !== null && restoredAttempt.decisionInput.acknowledged_assessment_id !== value.assessment.assessment_id) return false;
  if (rejection && (!isRecord(rejection) || !isString(rejection.key) || !isRecord(rejection.input) ||
    rejection.input.assessment_id !== value.assessment.assessment_id || rejection.input.action !== 'rejected')) return false;
  return !revocation || (isRecord(revocation) && isString(revocation.key) && isString(revocation.id));
}
function pendingOperationKey(value: Record<string, unknown>): string | undefined {
  if (isRecord(value.attempt) && isString(value.attempt.acquisitionKey)) return value.attempt.acquisitionKey;
  if (isRecord(value.rejectionAttempt) && isString(value.rejectionAttempt.key)) return value.rejectionAttempt.key;
  if (isRecord(value.revocationAttempt) && isString(value.revocationAttempt.key)) return value.revocationAttempt.key;
  return undefined;
}
function readPendingSelection(userId: string, unitId: string) {
  try {
    const raw = sessionStorage.getItem(pendingSelectionKey(userId));
    if (!raw) return undefined;
    const pending: unknown = JSON.parse(raw);
    return isPendingSelection(pending, userId, unitId) ? pending : undefined;
  } catch { return undefined; }
}
const assessmentFields = {
  title: 'assessmentFieldTitle',
  content_type: 'assessmentFieldContentType',
  language: 'assessmentFieldLanguage',
  run: 'assessmentFieldRun',
  region: 'assessmentFieldRegion',
  publisher: 'assessmentFieldPublisher',
  unit: 'assessmentFieldUnit',
  date: 'assessmentFieldDate',
  format: 'assessmentFieldFormat',
} as const;
const assessmentReasons = {
  missing: 'assessmentReasonMissing',
  ambiguous: 'assessmentReasonAmbiguous',
  conflict: 'assessmentReasonConflict',
  insufficient: 'assessmentReasonInsufficient',
  case_variant: 'assessmentReasonCaseVariant',
} as const;
/* Display label only. The locale falls back to the document language, which I18nProvider keeps current. */
function toUnitSelection(
  context: Schema['UnitContext'],
  t: (key: MessageKey) => string,
  locale: string = document.documentElement.lang || 'en',
): UnitSelection {
  const { publication, edition, unit } = context;
  return {
    id: unit.id,
    contentType: publication.content_type,
    title: publication.title,
    language: edition.language,
    label: [
      [publication.title, publication.run_label, unit.label].filter(Boolean).join(' '),
      t(unit.kind),
      editionLabel(edition, locale),
      formatUnitDate(unit, locale),
    ]
      .filter(Boolean)
      .join(', '),
  };
}
/* A 2xx body that fails these checks is treated like any other unexpected response: uncertain. */
const invalidResponse = () => new ApiError(0, 'invalid_response', '');
function isDecision(
  value: unknown,
  action: 'selected' | 'rejected',
  assessmentId: string,
): value is Schema['ReleaseDecision'] {
  return isRecord(value) && isString(value.id) && value.action === action && value.assessment_id === assessmentId;
}
function isAssessment(value: unknown): value is Schema['ReleaseAssessment'] {
  return (
    isRecord(value) &&
    isString(value.assessment_id) &&
    typeof value.assessment_expires_at === 'number' &&
    isRecord(value.evaluation) &&
    Array.isArray(value.evaluation.reasons) &&
    isRecord(value.target) &&
    isRecord(value.target.unit)
  );
}
type PendingSummary = { raw: string; unitId?: string; label: string; restorable: boolean };
/* Display-only view of whatever snapshot this tab holds, including ones for other units. */
function peekPendingSelection(userId: string): PendingSummary | undefined {
  try {
    const raw = sessionStorage.getItem(pendingSelectionKey(userId));
    if (!raw) return undefined;
    let stored: unknown;
    try {
      stored = JSON.parse(raw);
    } catch {
      stored = undefined;
    }
    const unit = isRecord(stored) && isRecord(stored.unit) ? stored.unit : undefined;
    const unitId = unit && isString(unit.id) ? unit.id : undefined;
    return {
      raw,
      unitId,
      label: unit && isString(unit.label) ? unit.label : '',
      restorable: Boolean(unitId && readPendingSelection(userId, unitId)),
    };
  } catch {
    return undefined;
  }
}

export function Search({ query, userId }: { query: URLSearchParams; userId: string }) {
  const unitId = query.get('unit')?.trim() || undefined;
  return unitId ? (
    <FindReleases key={unitId} unitId={unitId} userId={userId} />
  ) : (
    <UnitChooser userId={userId} />
  );
}

function UnitChooser({ userId }: { userId: string }) {
  const { t } = useI18n();
  return (
    <div className="find-releases">
      <PageHeader title={t('findReleases')} meta={t('searchHint')} />
      <PendingElsewhere userId={userId} />
      <div className="release-unit-picker">
        <UnitPicker
          select={(selected) => {
            location.hash = `#/search?unit=${encodeURIComponent(selected.id)}`;
          }}
        />
      </div>
    </div>
  );
}

type SourceTab = 'releases' | 'direct' | 'archive';
function FindReleases({ unitId, userId }: { unitId: string; userId: string }) {
  const { t, locale } = useI18n();
  const context = useResource<Schema['UnitContext']>(`/units/${encodeURIComponent(unitId)}`);
  const choices = useResource<Schema['IntegrationChoiceList']>('/search/integrations');
  if (context.loading) return <Loading />;
  if (!context.data)
    return (
      <>
        <BackLink href="#/" label={t('back')} />
        <ErrorNotice error={context.error} retry={context.reload} />
      </>
    );
  const { publication, edition, unit } = context.data;
  const selection = toUnitSelection(context.data, t, locale);
  const items = choices.data?.items;
  const tabs: TabItem<SourceTab>[] = [
    { id: 'releases', label: t('sourceIndexers'), icon: IconListSearch },
    publication.content_type === 'magazine'
      ? { id: 'archive', label: t('sourceArchive'), icon: IconArchive }
      : { id: 'direct', label: t('directDownloads'), icon: IconDownload },
  ];
  const meta = [t(unit.kind), editionLabel(edition, locale), formatUnitDate(unit, locale)]
    .filter(Boolean)
    .join(', ');
  return (
    <div className="find-releases">
      <BackLink
        href={`#/publication/${encodeURIComponent(publication.id)}`}
        label={t('backToPublication').replace('{title}', publication.title)}
      />
      <PageHeader title={`${publication.title} ${unit.label}`.trim()} meta={meta} />
      <PendingElsewhere userId={userId} unitId={context.data.unit.id} />
      {choices.loading ? (
        <Loading />
      ) : !items ? (
        <ErrorNotice error={choices.error} retry={choices.reload} />
      ) : (
        <Tabs label={t('releaseSources')} items={tabs} urlParam="source">
          {(tab) =>
            tab === 'direct' ? (
              <DirectSearch key={selection.id} unit={selection} choices={items} />
            ) : tab === 'archive' ? (
              <ArchiveSearch choices={items} defaultQuery={publication.title} />
            ) : (
              <ReleaseSearchForm
                key={context.data!.unit.id}
                choices={items}
                initialUnit={selection}
                userId={userId}
              />
            )
          }
        </Tabs>
      )}
    </div>
  );
}

/* Tells the user which unit holds this tab's pending request, since it blocks new selections. */
function PendingElsewhere({ userId, unitId }: { userId: string; unitId?: string }) {
  const { t } = useI18n();
  const [pending, setPending] = useState(() => peekPendingSelection(userId));
  const [failure, setFailure] = useState<unknown>();
  if (!pending || (pending.restorable && pending.unitId === unitId)) return null;
  const message = t('pendingElsewhere').replace('{unit}', pending.label || t('pendingUnknownUnit'));
  return (
    <div className="notice release-pending-elsewhere">
      <p>{message}</p>
      {pending.restorable && pending.unitId ? (
        <a
          className="button sm"
          href={`#/search?unit=${encodeURIComponent(pending.unitId)}&source=releases`}
        >
          {t('pendingResume')}
        </a>
      ) : (
        <>
          <ErrorNotice error={failure} />
          <DiscardPending
            discard={() => {
              /* Unreadable snapshot: remove it only if it is still the exact one shown. */
              try {
                const key = pendingSelectionKey(userId);
                if (sessionStorage.getItem(key) === pending.raw) sessionStorage.removeItem(key);
                if (sessionStorage.getItem(key) === pending.raw) throw new Error('Retry snapshot remains');
                setPending(undefined);
              } catch {
                setFailure(new ApiError(0, 'retry_storage_clear_failed', t('retryStorageClearFailed')));
              }
            }}
          />
        </>
      )}
    </div>
  );
}

function DiscardPending({ disabled, discard }: { disabled?: boolean; discard: () => void }) {
  const { t } = useI18n();
  const [confirming, setConfirming] = useState(false);
  return confirming ? (
    <div className="release-discard" role="group" aria-label={t('pendingDiscard')}>
      <p>{t('pendingDiscardHint')}</p>
      <div className="actions">
        <Button
          variant="danger"
          disabled={disabled}
          onClick={() => {
            setConfirming(false);
            discard();
          }}
        >
          {t('pendingDiscardConfirm')}
        </Button>
        <Button variant="ghost" disabled={disabled} onClick={() => setConfirming(false)}>
          {t('pendingDiscardKeep')}
        </Button>
      </div>
    </div>
  ) : (
    <div className="actions release-discard-trigger">
      <Button variant="danger" size="sm" disabled={disabled} onClick={() => setConfirming(true)}>
        {t('pendingDiscard')}
      </Button>
    </div>
  );
}

function ReleaseSearchForm({
  choices,
  initialUnit,
  userId,
}: {
  choices: IntegrationChoice[];
  initialUnit: UnitSelection;
  userId: string;
}) {
  const { t, locale } = useI18n();
  const pending = readPendingSelection(userId, initialUnit.id);
  const [unit, setUnit] = useState<UnitSelection>(initialUnit);
  const [search, setSearch] = useState<Schema['ReleaseSearch']>();
  const [results, setResults] = useState<Schema['Releases']>();
  const [release, setRelease] = useState<SelectedRelease | undefined>(pending && { release: pending.release });
  const [error, setError] = useState<unknown>();
  const [busy, setBusy] = useState(false);
  const requestVersion = useRef(0);
  const currentUnitId = useRef(unit.id);
  currentUnitId.current = unit.id;
  const sources = choices.filter(
    (choice) => choice.release_search && choice.content_types.includes(unit.contentType),
  );
  async function run(input: Schema['ReleaseSearch']) {
    const version = ++requestVersion.current;
    setBusy(true);
    setError(undefined);
    setResults(undefined);
    setSearch(input);
    try {
      const page = await post<Schema['Releases']>('/search/releases', input);
      if (version === requestVersion.current && currentUnitId.current === input.unit_id) {
        if (!isRecord(page) || !Array.isArray(page.releases)) {
          setError(invalidResponse());
          return;
        }
        if (!page.target || page.target.unit.id !== input.unit_id) {
          setError(new ApiError(0, 'request_failed', t('assessmentTargetMismatch')));
          return;
        }
        setUnit(toUnitSelection(page.target, t, locale));
        setResults(page);
      }
    } catch (failure) {
      if (version === requestVersion.current && currentUnitId.current === input.unit_id)
        setError(failure);
    } finally {
      if (version === requestVersion.current) setBusy(false);
    }
  }
  function resetForUnit() {
    requestVersion.current += 1;
    setBusy(false);
    setSearch(undefined);
    setResults(undefined);
    setRelease(undefined);
    setError(undefined);
  }
  if (release)
    return (
      <AcquireSelection
        key={release.release.release_handle}
        unit={unit}
        release={release}
        pending={pending}
        userId={userId}
        clients={choices.filter(
          (choice) => choice.download_client && choice.protocol === release.release.protocol,
        )}
        cancel={() => setRelease(undefined)}
      />
    );
  const sourceLabel = (id: string) => choices.find((choice) => choice.id === id)?.label;
  const page = search ? Math.floor(search.offset / search.limit) + 1 : 1;
  return (
    <div className="releases">
      {sources.length ? (
        <>
          <p className="muted releases-hint">{t('releaseSearchHint')}</p>
          <form
            className="release-search-form"
            aria-busy={busy}
            onSubmit={(event) => {
              event.preventDefault();
              const data = new FormData(event.currentTarget);
              void run({
                integration_id: value(data, 'integration_id'),
                query: value(data, 'query'),
                content_type: unit.contentType,
                unit_id: unit.id,
                offset: 0,
                limit: 20,
              });
            }}
          >
            <label className="field">
              <span>{t('source')}</span>
              <select
                name="integration_id"
                required
                defaultValue={sources.length === 1 ? sources[0].id : ''}
              >
                <option value="" disabled>
                  {t('chooseSource')}
                </option>
                {sources.map((choice) => (
                  <option value={choice.id} key={choice.id}>
                    {choice.label}
                  </option>
                ))}
              </select>
            </label>
            <label className="field release-query">
              <span>{t('searchQuery')}</span>
              <input name="query" required maxLength={512} defaultValue={unit.title} />
            </label>
            <Button type="submit" variant="primary" icon={IconSearch} disabled={busy}>
              {t('search')}
            </Button>
          </form>
        </>
      ) : (
        <EmptyState
          title={t('indexerNotConfigured')}
          action={
            <a className="button" href="#/settings?section=sources">
              {t('openSourceSettings')}
            </a>
          }
        >
          {t('indexerNotConfiguredHint')}
        </EmptyState>
      )}
      <ErrorNotice error={error} />
      {busy && <Loading />}
      {results &&
        (results.releases.length ? (
          <div className="release-table-wrap">
            <table className="release-table">
              <caption className="sr-only">{t('releaseResults')}</caption>
              <thead>
                <tr>
                  <th scope="col">{t('releaseColumnRelease')}</th>
                  <th scope="col">{t('source')}</th>
                  <th scope="col">{t('language')}</th>
                  <th scope="col">{t('assessmentFieldFormat')}</th>
                  <th scope="col" className="numeric">
                    {t('size')}
                  </th>
                  <th scope="col">{t('releaseColumnMatch')}</th>
                  <th scope="col">
                    <span className="sr-only">{t('releaseColumnAction')}</span>
                  </th>
                </tr>
              </thead>
              <tbody>
                {results.releases.map((item) => (
                  <ReleaseRow
                    key={item.release_handle}
                    item={item}
                    source={sourceLabel(item.integration_id)}
                    select={() => setRelease({ release: item })}
                  />
                ))}
              </tbody>
            </table>
          </div>
        ) : (
          <EmptyState title={t('noReleasesFound')}>{t('noReleasesFoundHint')}</EmptyState>
        ))}
      {results && search && (search.offset > 0 || results.next_offset !== null) && (
        <div className="pagination">
          <Button
            size="sm"
            disabled={busy || search.offset === 0}
            onClick={() => void run({ ...search, offset: Math.max(0, search.offset - search.limit) })}
          >
            {t('previous')}
          </Button>
          <span>
            {t('page')} {page}
          </span>
          <Button
            size="sm"
            disabled={busy || results.next_offset === null}
            onClick={() =>
              results.next_offset !== null && void run({ ...search, offset: results.next_offset })
            }
          >
            {t('next')}
          </Button>
        </div>
      )}
      {sources.length > 0 && (
        <Monitors
          key={unit.id}
          unit={unit}
          sources={sources}
          select={(candidate, monitor) => {
            resetForUnit();
            setUnit(toUnitSelection(monitor.target, t, locale));
            setRelease({ release: candidate });
          }}
        />
      )}
    </div>
  );
}

function languageName(language: string | undefined, region: string | undefined, locale: Locale) {
  if (!language) return undefined;
  if (/^[a-z]{2,3}$/i.test(language) && (!region || /^[a-z]{2}$/i.test(region))) {
    try {
      const name = new Intl.DisplayNames(locale, { type: 'language' }).of(
        region ? `${language}-${region}` : language,
      );
      if (name) return name;
    } catch {
      /* Unknown tags stay as written. */
    }
  }
  return region ? `${language} (${region})` : language;
}
const explicit = (evidence: Schema['EvidenceString']) =>
  evidence.state === 'explicit' ? evidence.value : undefined;

function ReleaseRow({
  item,
  source,
  select,
}: {
  item: Schema['Release'];
  source?: string;
  select: () => void;
}) {
  const { t, locale } = useI18n();
  const format = item.evidence.format.state === 'explicit' ? item.evidence.format.value.toUpperCase() : undefined;
  return (
    <tr>
      <th scope="row" className="release-name">
        {item.title}
      </th>
      <td data-label={t('source')}>{[source, t(item.protocol)].filter(Boolean).join(' · ')}</td>
      <td data-label={t('language')}>
        {languageName(explicit(item.evidence.language), explicit(item.evidence.region), locale)}
      </td>
      <td data-label={t('assessmentFieldFormat')}>{format}</td>
      <td data-label={t('size')} className="numeric">
        {formatSize(item.size_bytes, locale)}
      </td>
      <td data-label={t('releaseColumnMatch')}>
        <ReleaseAssessment evaluation={item.evaluation} />
      </td>
      <td className="release-action">
        <Button size="sm" onClick={select} aria-label={`${t('selectRelease')}: ${item.title}`}>
          {t('releaseSelect')}
        </Button>
      </td>
    </tr>
  );
}

function ReleaseAssessment({
  evaluation,
}: {
  evaluation: Schema['Release']['evaluation'];
}) {
  const { t } = useI18n();
  if (!evaluation) return <StatusBadge kind="info" label={t('releaseNotAssessed')} />;
  const [kind, label]: [StatusKind, MessageKey] =
    evaluation.eligibility === 'eligible'
      ? ['file', 'assessmentMatched']
      : evaluation.eligibility === 'unknown'
        ? ['warning', 'assessmentReview']
        : ['error', 'assessmentDoesNotMatch'];
  /* Conflicts decide a mismatch, so they come first. */
  const reasons = [...evaluation.reasons].sort(
    (a, b) => Number(b.code === 'conflict') - Number(a.code === 'conflict'),
  );
  return (
    <div className="release-match">
      <StatusBadge kind={kind} label={t(label)} />
      {reasons.length > 0 && (
        <ul className="release-reasons">
          {reasons.map((reason) => (
            <li key={`${reason.field}/${reason.code}`}>
              {t(assessmentFields[reason.field])}: {t(assessmentReasons[reason.code])}
            </li>
          ))}
        </ul>
      )}
    </div>
  );
}
const decisionFailures: Record<string, MessageKey> = {
  release_rejected: 'decisionReleaseRejected',
  selection_superseded: 'decisionSelectionSuperseded',
};
/* Definite 409 outcomes the user can act on get a localized message instead of the server text. */
function localizeFailure(failure: unknown, t: (key: MessageKey) => string) {
  if (!(failure instanceof ApiError) || failure.status !== 409 || !Object.hasOwn(decisionFailures, failure.code))
    return failure;
  return new ApiError(failure.status, failure.code, t(decisionFailures[failure.code]), failure.trace);
}
function isUncertainFailure(failure: unknown) {
  return !(failure instanceof ApiError) || failure.status === 0 ||
    failure.code === 'uncertain_result' || failure.code === 'invalid_response' ||
    failure.status >= 500 || failure.status === 401 || failure.status === 403 || failure.status === 429;
}
function AcquireSelection({
  unit,
  release,
  pending,
  userId,
  clients,
  cancel,
}: {
  unit: UnitSelection;
  release: SelectedRelease;
  pending?: PendingSelection;
  userId: string;
  clients: IntegrationChoice[];
  cancel: () => void;
}) {
  const { t, locale } = useI18n();
  const [assessment, setAssessment] = useState<Schema['ReleaseAssessment'] | undefined>(pending?.assessment);
  const [assessmentError, setAssessmentError] = useState<unknown>();
  const [assessmentLoading, setAssessmentLoading] = useState(!pending);
  const [acknowledged, setAcknowledged] = useState(false);
  const [attempt, setAttempt] = useState<AcquisitionAttempt | undefined>(pending?.attempt);
  const [rejectionAttempt, setRejectionAttempt] = useState<RejectionAttempt | undefined>(pending?.rejectionAttempt);
  const [revocationAttempt, setRevocationAttempt] = useState<RevocationAttempt | undefined>(pending?.revocationAttempt);
  const [restoredPending, setRestoredPending] = useState(Boolean(pending));
  const active = useRef(false);
  const assessmentRequest = useRef<{ version: number; controller?: AbortController }>({ version: 0 });
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<unknown>();
  const [recovery, setRecovery] = useState(false);
  /* A late success after leaving this unit must not move the user somewhere else. */
  const mounted = useRef(false);
  useEffect(() => {
    mounted.current = true;
    return () => {
      mounted.current = false;
    };
  }, []);
  function clearPending(expectedOperationKey: string) {
    try {
      const raw = sessionStorage.getItem(pendingSelectionKey(userId));
      if (!raw) return;
      let stored: unknown;
      try { stored = JSON.parse(raw); } catch { return; }
      if (!isRecord(stored) || pendingOperationKey(stored) !== expectedOperationKey) return;
      sessionStorage.removeItem(pendingSelectionKey(userId));
      if (sessionStorage.getItem(pendingSelectionKey(userId)) !== null)
        throw new Error('Retry snapshot remains');
    } catch { throw new ApiError(0, 'retry_storage_clear_failed', t('retryStorageClearFailed')); }
  }
  function savePending(next: Omit<PendingSelection, 'version' | 'userId' | 'unit' | 'release' | 'assessment'>) {
    if (!assessment) throw new ApiError(0, 'retry_storage_failed', t('retryStorageUnavailable'));
    const snapshot: PendingSelection = { version: 1, userId, unit, release: release.release, assessment, ...next };
    try {
      const existing = sessionStorage.getItem(pendingSelectionKey(userId));
      if (existing) {
        const value: unknown = JSON.parse(existing);
        if (isRecord(value) && value.userId === userId) {
          if (!isRecord(value.unit) || value.unit.id !== unit.id || !isRecord(value.release) || value.release.release_handle !== release.release.release_handle)
            throw new ApiError(0, 'retry_storage_occupied', t('pendingSelectionExists'));
          const storedKey = pendingOperationKey(value);
          const newKey = next.attempt?.acquisitionKey ?? next.rejectionAttempt?.key ?? next.revocationAttempt?.key;
          if (storedKey !== undefined && storedKey !== newKey)
            throw new ApiError(0, 'retry_storage_occupied', t('pendingSelectionExists'));
        }
      }
      sessionStorage.setItem(pendingSelectionKey(userId), JSON.stringify(snapshot));
    }
    catch (failure) {
      if (failure instanceof ApiError) throw failure;
      throw new ApiError(0, 'retry_storage_failed', t('retryStorageUnavailable'));
    }
  }
  async function refreshAssessment() {
    assessmentRequest.current.controller?.abort();
    const version = assessmentRequest.current.version + 1;
    const controller = new AbortController();
    assessmentRequest.current = { version, controller };
    setAssessmentLoading(true);
    setAssessmentError(undefined);
    setAssessment(undefined);
    setAcknowledged(false);
    try {
      const next = await request<Schema['ReleaseAssessment']>('/search/release-assessments', {
        method: 'POST', signal: controller.signal, body: JSON.stringify({
          release_handle: release.release.release_handle,
          unit_id: unit.id,
        } satisfies Schema['ReleaseAssessmentRequest']),
      });
      if (assessmentRequest.current.version !== version) return;
      if (!isAssessment(next)) throw invalidResponse();
      if (next.target.unit.id !== unit.id)
        throw new ApiError(0, 'request_failed', t('assessmentTargetMismatch'));
      setAssessment(next);
    } catch (failure) {
      if (assessmentRequest.current.version !== version) return;
      setAssessment(undefined);
      setAssessmentError(failure);
    } finally {
      if (assessmentRequest.current.version === version) setAssessmentLoading(false);
    }
  }
  useEffect(() => {
    if (restoredPending) return;
    void refreshAssessment();
    return () => {
      assessmentRequest.current.controller?.abort();
      assessmentRequest.current.version += 1;
    };
  }, [release.release.release_handle, unit.id, restoredPending]);
  async function submit(selected: NonNullable<typeof attempt>) {
    if (active.current) return;
    active.current = true;
    setBusy(true);
    setError(undefined);
    try {
      let decision = selected.decision;
      if (!decision) {
        decision = await request<Schema['ReleaseDecision']>('/search/release-decisions', {
          method: 'POST',
          headers: { 'Idempotency-Key': selected.decisionKey },
          body: JSON.stringify(selected.decisionInput),
        });
        if (!isDecision(decision, 'selected', selected.decisionInput.assessment_id)) throw invalidResponse();
        const updated = { ...selected, decision, input: { ...selected.input, selection_decision_id: decision.id } };
        setAttempt(updated);
        selected = updated;
      }
      const result = await request<Schema['Acquisition']>('/acquisition', {
        method: 'POST',
        headers: { 'Idempotency-Key': selected.acquisitionKey },
        body: JSON.stringify(selected.input),
      });
      if (!isRecord(result) || !isString(result.id)) throw invalidResponse();
      clearPending(selected.acquisitionKey);
      if (mounted.current && routeUnitId() === unit.id)
        location.hash = `/acquisition/${encodeURIComponent(result.id)}`;
    } catch (failure) {
      setError(localizeFailure(failure, t));
      if (!isUncertainFailure(failure)) {
        try { clearPending(selected.acquisitionKey); } catch (storageFailure) { setError(storageFailure); return; }
        setAttempt(undefined);
        setRestoredPending(false);
        setRecovery(true);
        void refreshAssessment();
      }
    } finally {
      active.current = false;
      setBusy(false);
    }
  }
  async function reject(selected: NonNullable<typeof rejectionAttempt>) {
    if (active.current) return;
    active.current = true;
    setBusy(true);
    setError(undefined);
    try {
      const decision = await request<Schema['ReleaseDecision']>('/search/release-decisions', {
        method: 'POST', headers: { 'Idempotency-Key': selected.key }, body: JSON.stringify(selected.input),
      });
      if (!isDecision(decision, 'rejected', selected.input.assessment_id)) throw invalidResponse();
      clearPending(selected.key);
      setAssessment((current) => current && { ...current, active_rejection: decision });
      setRejectionAttempt(undefined);
      setRestoredPending(false);
    } catch (failure) {
      setError(localizeFailure(failure, t));
      if (!isUncertainFailure(failure)) {
        try { clearPending(selected.key); } catch (storageFailure) { setError(storageFailure); return; }
        setRejectionAttempt(undefined);
        setRestoredPending(false);
        setRecovery(true);
        void refreshAssessment();
      }
    } finally { active.current = false; setBusy(false); }
  }
  async function revoke(selected: NonNullable<typeof revocationAttempt>) {
    if (active.current) return;
    active.current = true;
    setBusy(true);
    setError(undefined);
    try {
      const revocation = await request<Schema['ReleaseRejectionRevocation']>(`/search/release-decisions/${encodeURIComponent(selected.id)}/revocations`, {
        method: 'POST', headers: { 'Idempotency-Key': selected.key }, body: JSON.stringify({}),
      });
      if (!isRecord(revocation) || !isString(revocation.id)) throw invalidResponse();
      clearPending(selected.key);
      setRevocationAttempt(undefined);
      setRestoredPending(false);
      await refreshAssessment();
    } catch (failure) {
      setError(localizeFailure(failure, t));
      if (!isUncertainFailure(failure)) {
        try { clearPending(selected.key); } catch (storageFailure) { setError(storageFailure); return; }
        setRevocationAttempt(undefined);
        setRestoredPending(false);
        setRecovery(true);
        void refreshAssessment();
      }
    } finally { active.current = false; setBusy(false); }
  }
  const unresolved = Boolean(attempt || rejectionAttempt || revocationAttempt);
  const eligible = assessment?.evaluation.eligibility === 'eligible';
  const requiresAcknowledgement = assessment?.evaluation.eligibility === 'unknown';
  const canSelect = Boolean(
    assessment &&
      assessment.assessment_expires_at * 1000 > Date.now() &&
      !assessmentLoading &&
      !assessmentError &&
      !busy &&
      !unresolved &&
      !recovery &&
      (eligible || (requiresAcknowledgement && acknowledged)) &&
      !assessment.active_rejection,
  );
  const displayUnit = assessment ? toUnitSelection(assessment.target, t) : unit;
  /* Forgets the local retry for this exact operation key; the server may already have it. */
  function discardPending() {
    if (active.current) return;
    const key = attempt?.acquisitionKey ?? rejectionAttempt?.key ?? revocationAttempt?.key;
    if (!key) return;
    try { clearPending(key); } catch (storageFailure) { setError(storageFailure); return; }
    setAttempt(undefined);
    setRejectionAttempt(undefined);
    setRevocationAttempt(undefined);
    setRestoredPending(false);
    setRecovery(false);
    setError(undefined);
    void refreshAssessment();
  }
  return (
    <div className="release-confirm">
      <h2>{t('releaseConfirmTitle')}</h2>
      <p className="release-confirm-name">{release.release.title}</p>
      <p className="release-confirm-meta">
        {displayUnit.label}
        <br />
        {t(release.release.protocol)} · {formatSize(release.release.size_bytes, locale)}
      </p>
      {assessmentLoading ? <Loading /> : assessmentError ? <ErrorNotice error={assessmentError} retry={() => void refreshAssessment()} /> : assessment ? (
        <>
          <div className="release-confirm-assessment">
            <ReleaseAssessment evaluation={assessment.evaluation} />
            <div className="actions">
              <Button variant="ghost" size="sm" icon={IconRefresh} disabled={busy || assessmentLoading || unresolved} onClick={() => void refreshAssessment()}>{t('refreshAssessment')}</Button>
              {!assessment.active_rejection && !rejectionAttempt && (
                <Button variant="ghost" size="sm" className="danger" disabled={busy || unresolved} onClick={() => {
                  if (!assessment) return;
                  const selected = { key: crypto.randomUUID(), input: { assessment_id: assessment.assessment_id, action: 'rejected' as const, acknowledged_assessment_id: null, reason: null } };
                  try { savePending({ rejectionAttempt: selected }); setRejectionAttempt(selected); void reject(selected); }
                  catch (failure) { setError(failure); }
                }}>{t('rejectRelease')}</Button>
              )}
            </div>
          </div>
          {error && !unresolved && !recovery && <ErrorNotice error={error} />}
          {assessment.active_rejection ? (
            <div className="notice">
              <p>{t('releaseRejected')}</p>
              {!recovery && <ErrorNotice error={error} />}
              <Button disabled={busy || Boolean(attempt || rejectionAttempt)} onClick={() => {
                const selected = revocationAttempt ?? { key: crypto.randomUUID(), id: assessment.active_rejection!.id };
                try { savePending({ revocationAttempt: selected }); setRevocationAttempt(selected); void revoke(selected); }
                catch (failure) { setError(failure); }
              }}>{t(busy ? 'saving' : revocationAttempt ? 'retryDecision' : 'revokeRejection')}</Button>
            </div>
          ) : rejectionAttempt ? (
            <div className="notice"><p>{t('pendingDecisionHint')}</p><ErrorNotice error={error} /><Button disabled={busy} onClick={() => void reject(rejectionAttempt)}>{t(busy ? 'saving' : 'retryDecision')}</Button></div>
          ) : null}
          {requiresAcknowledgement && !assessment.active_rejection && (
            <label className="release-ack"><input type="checkbox" disabled={busy || unresolved || assessmentLoading} checked={acknowledged} onChange={(event) => setAcknowledged(event.currentTarget.checked)} /> <span>{t('acknowledgeAssessment')}</span></label>
          )}
        </>
      ) : null}
      {recovery && (
        <div className="notice">
          <ErrorNotice error={error} />
          <div className="actions">
            <Button disabled={assessmentLoading || busy || unresolved} onClick={() => void refreshAssessment()}>{t('refreshAssessment')}</Button>
            {assessment && !assessmentLoading && !assessmentError && (
              <Button disabled={busy || unresolved} onClick={() => { setRecovery(false); setError(undefined); }}>{t('useCurrentAssessment')}</Button>
            )}
            <Button variant="ghost" disabled={busy || unresolved} onClick={cancel}>{t('changeRelease')}</Button>
          </div>
        </div>
      )}
      {attempt ? (
        <div className="notice release-pending">
          <p>{t('pendingAcquisitionHint')}</p>
          <dl className="job-details">
            <dt>{t('downloadClient')}</dt>
            <dd>{attempt.client}</dd>
            {attempt.input.destination && (
              <>
                <dt>{t('destination')}</dt>
                <dd className="path">{attempt.input.destination.relative_path}</dd>
              </>
            )}
          </dl>
          <ErrorNotice error={error} />
          <div className="actions">
            <Button variant="primary" disabled={busy} onClick={() => void submit(attempt)}>
              {t(busy ? 'saving' : 'retryAcquisition')}
            </Button>
            <a href="#/activity?kind=downloads">{t('activityOpenDownloads')}</a>
          </div>
        </div>
      ) : canSelect && clients.length ? (
        <SaveForm
          label={t('submitAcquisition')}
          cancel={cancel}
          submit={async (data) => {
            const client = clients.find((client) => client.id === value(data, 'client_id'));
            if (!client) throw new ApiError(0, 'invalid_input', t('chooseClient'));
            const input: Schema['AcquisitionRequest'] = {
              release_handle: release.release.release_handle,
              client_id: client.id,
              unit_id: unit.id,
              destination: readDestination(data, t('invalidRelativePath')),
              selection_decision_id: null,
            };
            if (!assessment) throw new ApiError(0, 'invalid_input', t('assessmentSelectionBlocked'));
            const selected = {
              decisionKey: crypto.randomUUID(),
              acquisitionKey: crypto.randomUUID(),
              decisionInput: {
                assessment_id: assessment.assessment_id,
                action: 'selected' as const,
                acknowledged_assessment_id:
                  assessment.evaluation.eligibility === 'unknown' ? assessment.assessment_id : null,
                reason: null,
              },
              input,
              client: client.label,
            };
            savePending({ attempt: selected });
            setAttempt(selected);
            await submit(selected);
          }}
        >
          <label className="field">
            <span>{t('downloadClient')} *</span>
            <select name="client_id" required defaultValue="">
              <option disabled value="">
                {t('chooseClient')}
              </option>
              {clients.map((client) => (
                <option key={client.id} value={client.id}>
                  {client.label}
                </option>
              ))}
            </select>
          </label>
          <DestinationFields optional />
          <p className="muted release-field-hint">{t('destinationHint')}</p>
        </SaveForm>
      ) : clients.length ? (
        <div className="notice"><p>{t('assessmentSelectionBlocked')}</p><Button disabled={busy || unresolved} onClick={cancel}>{t('changeRelease')}</Button></div>
      ) : (
        <div className="notice">
          <p>{t('noCompatibleClients')} <a href="#/settings?section=sources">{t('openSourceSettings')}</a></p>
          <Button disabled={busy || unresolved} onClick={cancel}>{t('changeRelease')}</Button>
        </div>
      )}
      {unresolved && <DiscardPending disabled={busy} discard={discardPending} />}
    </div>
  );
}
