import { useCallback, useEffect, useRef, useState } from 'react';
import {
  IconAlertTriangle,
  IconBellRinging,
  IconChevronRight,
  IconDownload,
  IconFileImport,
  type TablerIcon,
} from '@tabler/icons-react';
import { type Schema, request } from './lib/api/client';
import { en } from './locales/en';
import { useI18n, type MessageKey } from './i18n';
import { RelativeTime } from './Activity';
import { unitDisplay } from './format';
import { EmptyState, ErrorNotice, Icon, Loading, PageHeader, domId } from './ui';
import './styles/review.css';

type ReviewItem = Schema['ReviewItem'];
type ReviewTotals = Schema['ReviewTotals'];
type ReviewFeed = Schema['ReviewFeed'];
type ReviewKind = ReviewItem['kind'];

const pollMs = 30_000;

/* Calls `tick` every 30s while the tab is visible, and once when it becomes visible again.
   No aria-live region wraps the list, so a poll refresh never triggers a screen-reader announcement. */
function useVisiblePoll(tick: () => void) {
  const latest = useRef(tick);
  useEffect(() => {
    latest.current = tick;
  });
  useEffect(() => {
    let timer: number | undefined;
    const start = () => {
      window.clearInterval(timer);
      timer = window.setInterval(() => latest.current(), pollMs);
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
  }, []);
}

function useReviewFeed() {
  const [data, setData] = useState<ReviewFeed>();
  const [error, setError] = useState<unknown>();
  const [loading, setLoading] = useState(true);
  const inflight = useRef<AbortController | undefined>(undefined);
  const load = useCallback(() => {
    inflight.current?.abort();
    const controller = new AbortController();
    inflight.current = controller;
    request<ReviewFeed>('/review', { signal: controller.signal })
      .then((feed) => {
        if (controller.signal.aborted) return;
        setData(feed);
        setError(undefined);
        setLoading(false);
      })
      .catch((failure) => {
        if (controller.signal.aborted) return;
        setError(failure);
        setLoading(false);
      });
  }, []);
  useEffect(() => {
    load();
    return () => inflight.current?.abort();
  }, [load]);
  useVisiblePoll(load);
  return { data, error, loading, reload: load };
}

/* Reason and state codes are shared with Acquisitions/Direct/Monitors and already have locale keys
   (e.g. uncertain_submission, challenge_required, source_cooldown, pending_association, failed).
   Uncataloged codes fall back to generic localized text; the linked detail page shows specifics. */
function localize(t: (key: MessageKey) => string, code: string | null | undefined): string | undefined {
  if (!code || !Object.hasOwn(en, code)) return undefined;
  return t(code as MessageKey);
}
function whatHappened(t: (key: MessageKey) => string, item: ReviewItem): string {
  if (item.kind === 'job_failed' && item.reason === 'uncertain_effect') return t('uncertainEffect');
  return localize(t, item.reason) ?? localize(t, item.state) ?? t('statusAttention');
}
function jobKindLabel(t: (key: MessageKey) => string, kind: string | null): string | undefined {
  if (!kind) return undefined;
  if (kind === 'library.scan') return t('libraryScan');
  if (kind === 'acquisition.pipeline') return t('activityDownload');
  return t('activityTask');
}
function itemMeta(t: (key: MessageKey) => string, item: ReviewItem, locale: string): string | undefined {
  switch (item.kind) {
    case 'file_to_link':
      return item.relative_path ?? undefined;
    case 'job_failed':
      return jobKindLabel(t, item.job_kind);
    case 'acquisition_needs_review':
    case 'direct_acquisition_needs_review':
    case 'monitor_needs_review': {
      if (!item.unit) return item.publication_title ?? undefined;
      const display = unitDisplay(item.unit, locale);
      const kind = t(item.unit.kind);
      const label = display.label.toLocaleLowerCase(locale);
      const namesKind = label.includes(kind.toLocaleLowerCase(locale)) || label.includes(item.unit.kind);
      return [item.publication_title, namesKind ? undefined : kind, display.label, display.date].filter(Boolean).join(' ');
    }
    default:
      return undefined;
  }
}
function itemHref(item: ReviewItem): string {
  switch (item.kind) {
    case 'acquisition_needs_review':
      return `#/acquisition/${encodeURIComponent(item.acquisition_id ?? item.id)}`;
    case 'direct_acquisition_needs_review':
      return `#/direct-acquisition/${encodeURIComponent(item.id)}`;
    case 'file_to_link':
      return item.root_id && item.entry_id
        ? `#/settings?section=storage&root_id=${encodeURIComponent(item.root_id)}&entry_id=${encodeURIComponent(item.entry_id)}&from=review`
        : '#/settings?section=storage';
    case 'job_failed':
      return `#/job/${encodeURIComponent(item.job_id ?? item.id)}`;
    case 'monitor_needs_review':
      return item.unit_id ? `#/monitors?unit_id=${encodeURIComponent(item.unit_id)}` : '#/monitors';
  }
}

type SectionDef = {
  id: string;
  kinds: readonly ReviewKind[];
  heading: MessageKey;
  icon: TablerIcon;
};
const sectionDefs: readonly SectionDef[] = [
  {
    id: 'downloads',
    kinds: ['acquisition_needs_review', 'direct_acquisition_needs_review'],
    heading: 'reviewSectionDownloads',
    icon: IconDownload,
  },
  { id: 'files', kinds: ['file_to_link'], heading: 'reviewSectionFiles', icon: IconFileImport },
  { id: 'failed', kinds: ['job_failed'], heading: 'reviewSectionFailed', icon: IconAlertTriangle },
  { id: 'monitors', kinds: ['monitor_needs_review'], heading: 'reviewSectionMonitors', icon: IconBellRinging },
];

function buildSections(items: readonly ReviewItem[], totals: ReviewTotals) {
  return sectionDefs
    .map((def) => {
      const sectionItems = items.filter((item) => (def.kinds as readonly string[]).includes(item.kind));
      const kindTotals = def.kinds.map((kind) => totals[kind]);
      const allKnown = kindTotals.every((value) => value !== null);
      const knownTotal = allKnown ? kindTotals.reduce<number>((sum, value) => sum + (value ?? 0), 0) : null;
      return {
        ...def,
        items: sectionItems,
        total: knownTotal,
        totalUnavailable: !allKnown,
        truncated: knownTotal !== null && knownTotal > sectionItems.length,
      };
    })
    .filter((section) => section.items.length > 0);
}

export function Review() {
  const { t, locale } = useI18n();
  const { data, error, loading, reload } = useReviewFeed();
  const sections = data ? buildSections(data.items, data.totals) : [];
  return (
    <>
      <PageHeader title={t('review')} meta={t('reviewIntro')} />
      {loading ? (
        <Loading />
      ) : error ? (
        <ErrorNotice error={error} context={t('reviewLoadError')} retry={reload} />
      ) : sections.length ? (
        sections.map((section) => (
          <section key={section.id} className="review-section">
            <h2 className="review-heading">
              {t(section.heading)}
              <span className="review-count">{section.total ?? section.items.length}</span>
            </h2>
            {(section.totalUnavailable || section.truncated) && (
              <p className="review-note">
                {section.totalUnavailable && t('reviewCountUnavailable')}
                {section.totalUnavailable && section.truncated && ' '}
                {section.truncated && t('reviewShowingNewest')}
              </p>
            )}
            <ul className="review-list">
              {section.items.map((item) => {
                const reason = whatHappened(t, item);
                const meta = itemMeta(t, item, locale);
                const diagnostics = item.unit_id ? `${t('unitIdentity')}: ${item.unit_id}` : undefined;
                const original = item.unit && [item.publication_title, item.unit.label].filter(Boolean).join(' ');
                const title = (item.kind === 'file_to_link' ? meta?.split('/').at(-1) : meta) || reason;
                const showPath = item.kind === 'file_to_link' && !!meta && meta !== title;
                const showReason = title !== reason;
                return (
                  <li key={`${item.kind}:${item.id}`}>
                    <a
                      className="review-row"
                      href={itemHref(item)}
                      aria-labelledby={domId('review', item.kind, item.id, 'title')}
                      aria-describedby={[
                        showPath && domId('review', item.kind, item.id, 'path'),
                        showReason && domId('review', item.kind, item.id, 'reason'),
                        diagnostics && domId('review', item.kind, item.id, 'diagnostics'),
                        domId('review', item.kind, item.id, 'time'),
                      ]
                        .filter(Boolean)
                        .join(' ')}
                    >
                      <span className="review-icon">
                        <Icon icon={section.icon} />
                      </span>
                      <span className="review-main">
                        <strong id={domId('review', item.kind, item.id, 'title')} title={[meta || title, original, diagnostics].filter(Boolean).join('\n')}>
                          {title}
                        </strong>
                        {showPath && (
                          <span className="sr-only" id={domId('review', item.kind, item.id, 'path')}>
                            {meta}
                          </span>
                        )}
                        {diagnostics && (
                          <span className="sr-only" id={domId('review', item.kind, item.id, 'diagnostics')}>
                            {diagnostics}
                          </span>
                        )}
                        {showReason && (
                          <span className="review-meta" id={domId('review', item.kind, item.id, 'reason')}>
                            {reason}
                          </span>
                        )}
                      </span>
                      <span className="review-time" id={domId('review', item.kind, item.id, 'time')}>
                        {item.created_at !== null ? <RelativeTime seconds={item.created_at} /> : t('unavailable')}
                      </span>
                      <span className="review-action">
                        {t('reviewAction')}
                        <Icon icon={IconChevronRight} size={18} />
                      </span>
                    </a>
                  </li>
                );
              })}
            </ul>
          </section>
        ))
      ) : (
        <EmptyState
          title={t('reviewEmptyTitle')}
          action={
            <>
              <a className="button primary" href="#/wanted">
                {t('reviewSeeMissing')}
              </a>
              <a className="button ghost" href="#/activity">
                {t('reviewSeeActivity')}
              </a>
            </>
          }
        >
          {t('reviewEmptyText')}
        </EmptyState>
      )}
    </>
  );
}
