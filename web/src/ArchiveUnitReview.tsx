import { useEffect, useRef, useState } from 'react';
import { useI18n } from './i18n';
import { ApiError, request, type Page, type Schema } from './lib/api/client';
import { editionLabel } from './format';
import { Button, ErrorNotice, Loading, StatusBadge } from './ui';

type Choice = { id: string; label: string };

export function ArchiveUnitReview({ integrationId, magazine, onClose }: {
  integrationId: string;
  magazine: Schema['ArchiveMagazine'];
  onClose: () => void;
}) {
  const { t, locale } = useI18n();
  const acceptedLink = useRef<HTMLAnchorElement>(null);
  const heading = useRef<HTMLHeadingElement>(null);
  const [publications, setPublications] = useState<Schema['PublicationSummary'][]>([]);
  const [publicationCursor, setPublicationCursor] = useState<string | null>(null);
  const [publication, setPublication] = useState('');
  const [editions, setEditions] = useState<Choice[]>([]);
  const [edition, setEdition] = useState('');
  const [proposal, setProposal] = useState<Schema['ArchiveUnitProposal']>();
  const [label, setLabel] = useState('');
  const [kind, setKind] = useState<Schema['Unit']['kind']>('issue');
  const [date, setDate] = useState('');
  const [duplicate, setDuplicate] = useState<Schema['ArchiveUnitMatch']>();
  const [accepted, setAccepted] = useState<Schema['Unit']>();
  const [pending, setPending] = useState<Schema['ArchiveAcceptRequest']>();
  const [busy, setBusy] = useState(true);
  const [error, setError] = useState<unknown>();
  const active = useRef<AbortController | null>(null);

  useEffect(() => {
    heading.current?.focus();
    const controller = new AbortController();
    active.current = controller;
    void loadPublications(null, controller);
    return () => controller.abort();
  }, []);
  useEffect(() => () => active.current?.abort(), []);
  useEffect(() => { if (accepted) acceptedLink.current?.focus(); }, [accepted]);

  function review(result: Schema['ArchiveUnitProposal']) {
    setProposal(result);
    setLabel(result.label);
    setKind(result.kind);
    setDate(result.date ?? '');
  }

  function chooseEdition(id: string) {
    active.current?.abort();
    setBusy(false);
    setEdition(id);
    setProposal(undefined);
    setDuplicate(undefined);
    setError(undefined);
  }

  async function loadPublications(cursor: string | null, controller = new AbortController()) {
    active.current = controller;
    setBusy(true);
    setError(undefined);
    try {
      const params = new URLSearchParams({ kind: 'magazine', limit: '100' });
      if (cursor) params.set('cursor', cursor);
      const page = await request<Page<Schema['PublicationSummary']>>(`/publications?${params}`, { signal: controller.signal });
      if (!controller.signal.aborted) {
        setPublications((previous) => cursor ? [...previous, ...page.items] : page.items);
        setPublicationCursor(page.next_cursor);
      }
    } catch (failure) {
      if (!controller.signal.aborted) setError(failure);
    } finally {
      if (!controller.signal.aborted) setBusy(false);
    }
  }

  async function choosePublication(id: string) {
    active.current?.abort();
    const controller = new AbortController();
    active.current = controller;
    setPublication(id);
    setEdition('');
    setEditions([]);
    setProposal(undefined);
    setDuplicate(undefined);
    setBusy(true);
    setError(undefined);
    try {
      const choices: Choice[] = [];
      let cursor: string | null = null;
      do {
        const params = new URLSearchParams({ limit: '100' });
        if (cursor) params.set('cursor', cursor);
        const page: Page<Schema['Edition']> = await request(`/publications/${encodeURIComponent(id)}/editions?${params}`, { signal: controller.signal });
        choices.push(...page.items.map((item) => ({ id: item.id, label: editionLabel(item, locale) })));
        cursor = page.next_cursor;
      } while (cursor && !controller.signal.aborted);
      if (!controller.signal.aborted) setEditions(choices);
    } catch (failure) {
      if (!controller.signal.aborted) setError(failure);
    } finally {
      if (!controller.signal.aborted) setBusy(false);
    }
  }

  async function propose() {
    active.current?.abort();
    const controller = new AbortController();
    active.current = controller;
    setBusy(true);
    setError(undefined);
    setDuplicate(undefined);
    try {
      const params = new URLSearchParams({ integration_id: integrationId, identifier: magazine.identifier, edition_id: edition });
      const result = await request<Schema['ArchiveUnitProposal']>(`/search/archive/proposal?${params}`, { signal: controller.signal });
      if (!controller.signal.aborted) review(result);
    } catch (failure) {
      if (controller.signal.aborted) return;
      if (failure instanceof ApiError && failure.code === 'duplicate_unit' && failure.archiveConflict) {
        review(failure.archiveConflict.proposal);
        setDuplicate(failure.archiveConflict.existingUnit);
      } else setError(failure);
    } finally {
      if (!controller.signal.aborted) setBusy(false);
    }
  }

  async function accept() {
    const controller = new AbortController();
    active.current = controller;
    const body = pending ?? { integration_id: integrationId, identifier: magazine.identifier, edition_id: edition, label, kind, date: date || null, idempotency_key: crypto.randomUUID() };
    setPending(body);
    setBusy(true);
    setError(undefined);
    try {
      const result = await request<Schema['ArchiveUnitAcceptance']>('/search/archive/accept', { method: 'POST', body: JSON.stringify(body), signal: controller.signal });
      if (controller.signal.aborted) return;
      if (!result?.unit?.id) throw new ApiError(200, 'invalid_response', '');
      setAccepted(result.unit);
      setPending(undefined);
    } catch (failure) {
      if (controller.signal.aborted) return;
      if (failure instanceof ApiError && failure.code === 'duplicate_unit' && failure.archiveConflict) {
        setDuplicate(failure.archiveConflict.existingUnit);
        setPending(undefined);
      } else {
        setError(failure);
        if (failure instanceof ApiError && [400, 404, 409, 422].includes(failure.status)) setPending(undefined);
      }
    } finally {
      if (!controller.signal.aborted) setBusy(false);
    }
  }

  return <section className="archive-unit-review" aria-labelledby="archive-unit-review-title" aria-busy={busy}>
    <h4 id="archive-unit-review-title" ref={heading} tabIndex={-1}>{t('archiveProposeUnit')}</h4>
    <p className="muted">{t('archiveProposalHint')}</p>
    <dl className="job-details">
      <dt>{t('archiveIdentifier')}</dt><dd>{magazine.identifier}</dd>
      <dt>{t('volume')}</dt><dd className="archive-raw">{(proposal?.raw_volumes ?? magazine.volumes).join(' / ') || t('unknown')}</dd>
      <dt>{t('archiveRawLabel')}</dt><dd className="archive-raw">{(proposal?.raw_labels ?? magazine.issues).join(' / ') || t('unknown')}</dd>
      <dt>{t('archiveRawDates')}</dt><dd className="archive-raw">{(proposal?.raw_dates ?? magazine.dates).join(' / ') || t('unknown')}</dd>
    </dl>
    {accepted ? <p role="status">{t('archiveUnitCreated')} <a ref={acceptedLink} href={`#/publication/${encodeURIComponent(publication)}`}>{accepted.label}</a></p> : <>
      <fieldset disabled={Boolean(pending)}>
        <div className="archive-proposal-fields">
          <label className="field"><span>{t('magazine')}</span><select value={publication} onChange={(event) => void choosePublication(event.target.value)}>
            <option value="" disabled>{t('archiveChoosePublication')}</option>
            {publications.map((item) => <option key={item.id} value={item.id}>{item.title}</option>)}
          </select></label>
          <label className="field"><span>{t('edition')}</span><select value={edition} disabled={busy || !publication || editions.length === 0} onChange={(event) => chooseEdition(event.target.value)}>
            <option value="" disabled>{t('archiveChooseEdition')}</option>
            {editions.map((item) => <option key={item.id} value={item.id}>{item.label}</option>)}
          </select></label>
        </div>
        {publicationCursor && <Button size="sm" disabled={busy} onClick={() => void loadPublications(publicationCursor)}>{t('archiveMorePublications')}</Button>}
        {!busy && publications.length === 0 && <p>{t('archiveNoPublications')}</p>}
        {publication && !busy && editions.length === 0 && <p>{t('archiveNoEditions')}</p>}
        {edition && !proposal && !duplicate && <Button disabled={busy} onClick={() => void propose()}>{t('archiveReviewProposal')}</Button>}
      </fieldset>
      {proposal && <form onSubmit={(event) => { event.preventDefault(); void accept(); }}>
        {proposal.date_warnings.length > 0 && <div role="status">
          <p>{t('archiveDateWarning')}</p>
          <ul>{proposal.date_warnings.map((unit) => <li key={unit.id}><a href={`#/publication/${encodeURIComponent(unit.publication_id)}`}>{unit.label}</a></li>)}</ul>
        </div>}
        {proposal.combined && <p><StatusBadge kind="warning" label={t('archiveCombinedWarning')} /></p>}
        <fieldset disabled={busy || Boolean(pending)}>
          <div className="archive-proposal-fields">
            <label className="field"><span>{t('label')}</span><input required value={label} onChange={(event) => { setLabel(event.target.value); setDuplicate(undefined); }} /></label>
            <label className="field"><span>{t('unitKind')}</span><select value={kind} onChange={(event) => {
              const value = event.target.value;
              if (value === 'issue' || value === 'chapter' || value === 'volume' || value === 'special' || value === 'combined') setKind(value);
            }}>{(['issue', 'chapter', 'volume', 'special', 'combined'] as const).filter((value) => !proposal.combined || value === 'issue' || value === 'combined').map((value) => <option key={value} value={value}>{t(value)}</option>)}</select></label>
            <label className="field"><span>{t('date')} ({t('archiveProposalDateFormat')})</span><input value={date} pattern="[0-9]{4}(-[0-9]{2}(-[0-9]{2})?)?" onChange={(event) => setDate(event.target.value)} /></label>
          </div>
          <p className="muted">{t('archiveDatePrecision')}: {t(date.length === 4 ? 'archiveYearPrecision' : date.length === 7 ? 'archiveMonthPrecision' : date.length === 10 ? 'archiveDayPrecision' : 'unknown')}</p>
        </fieldset>
        {pending && !busy && <p role="status">{t('archiveAcceptRetryHint')}</p>}
        <Button type="submit" variant="primary" disabled={busy}>{t(pending ? 'archiveRetryAccept' : 'archiveAcceptUnit')}</Button>
      </form>}
      {duplicate && <p role="alert">{t('archiveDuplicateWarning')} <a href={`#/publication/${encodeURIComponent(duplicate.publication_id)}`}>{duplicate.label}</a></p>}
    </>}
    <ErrorNotice error={error} retry={!publication ? () => void loadPublications(null) : editions.length === 0 ? () => void choosePublication(publication) : undefined} />
    {busy && <Loading />}
    <Button variant="ghost" disabled={busy} onClick={onClose}>{t(accepted ? 'close' : 'cancel')}</Button>
  </section>;
}
