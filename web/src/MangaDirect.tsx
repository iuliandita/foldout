import { Fragment, useEffect, useRef, useState, type ReactNode } from 'react';
import { IconSearch } from '@tabler/icons-react';
import { useI18n } from './i18n';
import { request, type Schema } from './lib/api/client';
import type { UnitSelection } from './Storage';
import { Button, EmptyState, ErrorNotice, Loading } from './ui';

type Props = {
  unit: UnitSelection & { language: string };
  choices: Schema['IntegrationChoice'][];
  renderSelection: (
    chapter: Schema['DirectChapter'],
    title: string,
    cancel: () => void,
  ) => ReactNode;
};
type MetadataSearch = {
  integration_id: string;
  query: string;
  page: number;
  limit: number;
};
type SearchRequest =
  | { kind: 'metadata'; input: MetadataSearch }
  | { kind: 'chapters'; input: Schema['DirectChapterSearch'] };

export function MangaDirect(props: Props) {
  return <MangaPicker key={`${props.unit.id}/${props.unit.language}`} {...props} />;
}

function MangaPicker({ unit, choices, renderSelection }: Props) {
  const { t, locale } = useI18n();
  const sources = choices.filter((choice) => choice.kind === 'mangadex');
  const [source, setSource] = useState(sources.length === 1 ? sources[0].id : '');
  const [query, setQuery] = useState(unit.title);
  const [search, setSearch] = useState<MetadataSearch>();
  const [metadata, setMetadata] = useState<Schema['MetadataPage']>();
  const [series, setSeries] = useState<Schema['MetadataCandidate']>();
  const [chapters, setChapters] = useState<Schema['DirectChapterPage']>();
  const [selected, setSelected] = useState<Schema['DirectChapter']>();
  const [lastRequest, setLastRequest] = useState<SearchRequest>();
  const [error, setError] = useState<unknown>();
  const [busy, setBusy] = useState(false);
  const active = useRef<AbortController | null>(null);

  useEffect(() => () => active.current?.abort(), []);

  function clearSearch() {
    setSearch(undefined);
    setMetadata(undefined);
    setSeries(undefined);
    setChapters(undefined);
    setSelected(undefined);
    setLastRequest(undefined);
    setError(undefined);
  }

  async function run(job: SearchRequest) {
    if (active.current) return;
    const controller = new AbortController();
    active.current = controller;
    setBusy(true);
    setError(undefined);
    setLastRequest(job);
    setChapters(undefined);
    setSelected(undefined);
    try {
      if (job.kind === 'metadata') {
        setSearch(job.input);
        setMetadata(undefined);
        setSeries(undefined);
        const params = new URLSearchParams({
          ...job.input,
          page: String(job.input.page),
          limit: String(job.input.limit),
        });
        const result = await request<Schema['MetadataPage']>(`/search/metadata?${params}`, {
          signal: controller.signal,
        });
        if (!controller.signal.aborted) setMetadata(result);
      } else {
        const result = await request<Schema['DirectChapterPage']>('/direct/chapters', {
          method: 'POST',
          body: JSON.stringify(job.input),
          signal: controller.signal,
        });
        if (!controller.signal.aborted) setChapters(result);
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

  async function loadChapters(candidate: Schema['MetadataCandidate'], page = 1, limit = 20) {
    if (active.current || !search) return;
    setSeries(candidate);
    await run({
      kind: 'chapters',
      input: {
        integration_id: search.integration_id,
        manga_id: candidate.external_id,
        language: unit.language,
        page,
        limit,
      },
    });
  }

  if (selected && series) {
    const title = [
      series.title,
      `${t('chapter')} ${selected.chapter ?? t('unknown')}`,
      selected.title,
    ]
      .filter(Boolean)
      .join(' / ');
    return renderSelection(selected, title, () => setSelected(undefined));
  }

  const candidates = metadata?.candidates.filter(
    (candidate) => candidate.provider === 'manga_dex' && candidate.content_type === 'manga',
  );
  if (sources.length === 0)
    return (
      <div className="releases">
        <EmptyState
          title={t('directNotConfigured')}
          action={
            <a className="button" href="#/settings?section=sources">
              {t('openSourceSettings')}
            </a>
          }
        >
          {t('mangaNoSource')}
        </EmptyState>
      </div>
    );
  return (
    <div className="releases" aria-busy={busy}>
      <p className="muted releases-hint">{t('mangaDirectHint')}</p>
      <fieldset disabled={busy}>
        <form
          className="release-search-form"
          onSubmit={(event) => {
            event.preventDefault();
            void run({
              kind: 'metadata',
              input: { integration_id: source, query: query.trim(), page: 1, limit: 20 },
            });
          }}
        >
          <label className="field">
            <span>{t('source')}</span>
            <select
              name="manga_source"
              required
              value={source}
              onChange={(event) => {
                if (active.current) return;
                setSource(event.target.value);
                clearSearch();
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
              name="manga_query"
              required
              maxLength={512}
              value={query}
              onChange={(event) => {
                if (active.current) return;
                setQuery(event.target.value);
                clearSearch();
              }}
            />
          </label>
          <Button type="submit" variant="primary" icon={IconSearch}>
            {t('search')}
          </Button>
        </form>
        <ErrorNotice
          error={error}
          retry={lastRequest ? () => void run(lastRequest) : undefined}
        />
        {!series && metadata && search && candidates && (
          <>
            <h3 className="release-subheading">{t('mangaChooseSeries')}</h3>
            {candidates.length ? (
              <ul className="release-list">
                {candidates.map((candidate) => (
                  <li key={candidate.external_id}>
                    <span>
                      <span className="release-name">{candidate.title}</span>
                      {candidate.date && <span className="muted"> · {candidate.date}</span>}
                    </span>
                    <Button size="sm" onClick={() => void loadChapters(candidate)}>
                      {t('mangaViewChapters')}
                    </Button>
                  </li>
                ))}
              </ul>
            ) : (
              <EmptyState title={t('noReleasesFound')}>{t('noReleasesFoundHint')}</EmptyState>
            )}
            {(search.page > 1 || metadata.next_page !== null) && (
              <div className="pagination">
                <Button
                  size="sm"
                  disabled={search.page <= 1}
                  onClick={() =>
                    void run({
                      kind: 'metadata',
                      input: { ...search, page: search.page - 1, limit: metadata.page_size },
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
                  disabled={metadata.next_page === null}
                  onClick={() =>
                    metadata.next_page !== null &&
                    void run({
                      kind: 'metadata',
                      input: { ...search, page: metadata.next_page, limit: metadata.page_size },
                    })
                  }
                >
                  {t('next')}
                </Button>
              </div>
            )}
          </>
        )}
        {series && (
          <div className="release-detail">
            <div className="section-header">
              <h3>{series.title}</h3>
              <Button
                variant="ghost"
                size="sm"
                onClick={() => {
                  if (active.current) return;
                  setSeries(undefined);
                  setChapters(undefined);
                  setSelected(undefined);
                  setLastRequest(undefined);
                  setError(undefined);
                }}
              >
                {t('mangaChangeSeries')}
              </Button>
            </div>
            {chapters && (
              <>
                {chapters.chapters.length ? (
                  <ul className="release-list">
                    {chapters.chapters.map((chapter) => (
                      <li key={chapter.link_handle}>
                        <div>
                          <span className="release-name">
                            {chapter.title || `${t('chapter')} ${chapter.chapter ?? ''}`.trim()}
                          </span>
                          <p className="release-row-meta">
                            {[
                              chapter.chapter && `${t('chapter')} ${chapter.chapter}`,
                              chapter.volume && `${t('volume')} ${chapter.volume}`,
                              chapter.language,
                              chapter.scanlation_groups
                                .map((group) => group.name)
                                .filter(Boolean)
                                .join(', '),
                              `${chapter.page_count.toLocaleString(locale)} ${t('mangaPages')}`,
                            ]
                              .filter(Boolean)
                              .join(' · ')}
                          </p>
                          {chapter.state !== 'direct' && (
                            <p className="release-row-meta">
                              {t(
                                chapter.state === 'manual_action'
                                  ? 'mangaExternalChapter'
                                  : 'unsupportedMirror',
                              )}
                            </p>
                          )}
                          <details>
                            <summary>{t('sourceDetails')}</summary>
                            <dl className="job-details">
                              <dt>{t('mangaChapterId')}</dt>
                              <dd>{chapter.chapter_id}</dd>
                              <dt>{t('version')}</dt>
                              <dd>{chapter.version.toLocaleString(locale)}</dd>
                              {chapter.scanlation_groups.map((group) => (
                                <Fragment key={group.id}>
                                  <dt>{group.name || t('unknown')}</dt>
                                  <dd>{group.id}</dd>
                                </Fragment>
                              ))}
                            </dl>
                          </details>
                        </div>
                        {chapter.state === 'direct' && (
                          <Button
                            size="sm"
                            onClick={() => {
                              if (!active.current) setSelected(chapter);
                            }}
                          >
                            {t('releaseSelect')}
                          </Button>
                        )}
                      </li>
                    ))}
                  </ul>
                ) : (
                  <EmptyState title={t('mangaNoChapters')} />
                )}
                {(chapters.page > 1 || chapters.next_page !== null) && (
                  <div className="pagination">
                    <Button
                      size="sm"
                      disabled={chapters.page <= 1}
                      onClick={() =>
                        void loadChapters(series, chapters.page - 1, chapters.page_size)
                      }
                    >
                      {t('previous')}
                    </Button>
                    <span>
                      {t('page')} {chapters.page.toLocaleString(locale)}
                    </span>
                    <Button
                      size="sm"
                      disabled={chapters.next_page === null}
                      onClick={() =>
                        chapters.next_page !== null &&
                        void loadChapters(series, chapters.next_page, chapters.page_size)
                      }
                    >
                      {t('next')}
                    </Button>
                  </div>
                )}
              </>
            )}
          </div>
        )}
      </fieldset>
      {busy && <Loading />}
    </div>
  );
}
