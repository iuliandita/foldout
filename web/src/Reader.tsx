import {
  useCallback,
  useEffect,
  useId,
  useRef,
  useState,
  type FocusEvent as ReactFocusEvent,
  type PointerEvent as ReactPointerEvent,
  type ReactNode,
} from 'react';
import {
  IconAdjustmentsHorizontal,
  IconAlertTriangle,
  IconArrowLeft,
  IconChevronLeft,
  IconChevronRight,
  IconClock,
  IconCloudCheck,
  IconCloudUpload,
  IconKeyboard,
  IconLock,
  IconRefresh,
} from '@tabler/icons-react';
import { ApiError, request, requestImage, type Schema } from './lib/api/client';
import { useI18n } from './i18n';
import {
  Button,
  ErrorNotice,
  Icon,
  SegmentedControl,
  Skeleton,
  useResource,
  type TablerIcon,
} from './ui';
import './styles/reader.css';

type Progress = Schema['ReadingProgress'];
type Position = Pick<Progress, 'page' | 'direction'>;
type Direction = Position['direction'];
type Fit = 'page' | 'width';
const zooms = ['50', '75', '100', '125', '150', '200'] as const;
type Zoom = (typeof zooms)[number];
type Translate = ReturnType<typeof useI18n>['t'];

const HIDE_AFTER_MS = 3000;
const SWIPE_MIN_PX = 50;
const TAP_SLOP_PX = 10;

/* Title and Back come from the file's catalog context ("Saga #1", as in the document title and Activity);
   ?title= is only a fallback when that request fails. */
type Identity = { heading?: string; publication?: { id: string; title: string } };
function identityFrom(
  context: { data?: Schema['FileContextList']; error?: unknown },
  query: URLSearchParams,
  t: Translate,
): Identity {
  const first = context.data?.items[0];
  if (first) {
    const units = new Set(context.data!.items.map((item) => item.unit_id)).size;
    const heading =
      units > 1
        ? t('readerTitleMore')
            .replace('{publication}', first.publication_title)
            .replace('{unit}', first.unit_label)
            .replace('{count}', String(units - 1))
        : `${first.publication_title} ${first.unit_label}`.trim();
    return { heading, publication: { id: first.publication_id, title: first.publication_title } };
  }
  if (context.error === undefined) return {};
  const raw = query.get('title')?.trim();
  return raw ? { heading: raw } : {};
}

type BackTarget = { label: string; href?: string; onClick?: () => void };
function backTarget(query: URLSearchParams, publication: Identity['publication'], t: Translate): BackTarget {
  const back = query.get('back');
  const safeBack = back?.startsWith('/') ? back : undefined;
  if (publication) {
    const path = `/publication/${encodeURIComponent(publication.id)}`;
    /* The link's own back keeps the list filters when it already points at this publication. */
    const href = safeBack && (safeBack === path || safeBack.startsWith(`${path}?`)) ? safeBack : path;
    return { label: t('readerBackTo').replace('{name}', publication.title), href: `#${href}` };
  }
  if (safeBack) return { label: safeBack === '/' ? t('back') : t('readerBack'), href: `#${safeBack}` };
  if (history.length > 1) return { label: t('readerBack'), onClick: () => history.back() };
  return { label: t('back'), href: '#/' };
}

function BackLink({ target, secondary = false }: { target: BackTarget; secondary?: boolean }) {
  const className = secondary ? 'button' : 'button ghost reader-back';
  const content = (
    <>
      <Icon icon={IconArrowLeft} />
      <span className={secondary ? undefined : 'reader-label'}>{target.label}</span>
    </>
  );
  return target.href ? (
    <a className={className} href={target.href} title={target.label}>
      {content}
    </a>
  ) : (
    <button type="button" className={className} onClick={target.onClick} title={target.label}>
      {content}
    </button>
  );
}

function useMediaQuery(query: string) {
  const [matches, setMatches] = useState(() => window.matchMedia(query).matches);
  useEffect(() => {
    const list = window.matchMedia(query);
    const update = () => setMatches(list.matches);
    update();
    list.addEventListener('change', update);
    return () => list.removeEventListener('change', update);
  }, [query]);
  return matches;
}

export function Reader({
  id,
  query,
  canSave,
}: {
  id: string;
  query: URLSearchParams;
  canSave: boolean;
}) {
  const { t } = useI18n();
  const manifest = useResource<Schema['ReaderManifest']>(
    `/library/files/${encodeURIComponent(id)}/manifest`,
  );
  const progress = useResource<Progress>(`/library/files/${encodeURIComponent(id)}/progress`);
  const context = useResource<Schema['FileContextList']>(`/library/files/${encodeURIComponent(id)}/context`);
  const identity = identityFrom(context, query, t);
  const heading = identity.heading || t('reader');
  const back = backTarget(query, identity.publication, t);
  useEffect(() => {
    const target =
      document.querySelector<HTMLElement>('main h1') ?? document.getElementById('main-content');
    if (target) {
      target.tabIndex = -1;
      target.focus();
    }
  }, [id, manifest.loading, progress.loading]);
  useEffect(() => {
    // App sets a generic route title in a parent effect, which runs after this one.
    let active = true;
    queueMicrotask(() => {
      if (active) document.title = `${heading} | ${t('app')}`;
    });
    return () => {
      active = false;
    };
  }, [heading, t]);
  const reload = () => {
    manifest.reload();
    progress.reload();
  };
  if (manifest.loading || progress.loading)
    return (
      <ReaderFrame heading={heading} back={back}>
        <PagePlaceholder />
      </ReaderFrame>
    );
  const invalid =
    !manifest.error &&
    !progress.error &&
    !!manifest.data &&
    !!progress.data &&
    (manifest.data.page_count < 1 ||
      !Number.isSafeInteger(manifest.data.page_count) ||
      progress.data.signature !== manifest.data.signature);
  if (manifest.error || progress.error || invalid)
    return (
      <ReaderFrame heading={heading} back={back}>
        <div className="reader-message">
          <p>{t('readerOpenFailed')}</p>
          <ErrorNotice
            error={manifest.error || progress.error || new ApiError(200, 'invalid_response', '')}
            retry={reload}
          />
          <BackLink target={back} secondary />
        </div>
      </ReaderFrame>
    );
  if (!manifest.data || !progress.data) return null;
  return (
    <ReaderDocument
      key={`${id}:${progress.data.revision}:${manifest.data.signature}`}
      manifest={manifest.data}
      initial={progress.data}
      canSave={canSave}
      heading={heading}
      back={back}
      reload={reload}
    />
  );
}

/* Static frame for loading and failure: Back and title stay visible. */
function ReaderFrame({
  heading,
  back,
  children,
}: {
  heading: string;
  back: BackTarget;
  children: ReactNode;
}) {
  return (
    <main id="main-content" className="reader-view" tabIndex={-1}>
      <div className="reader-stage">
        <div className="reader-center">{children}</div>
      </div>
      <div className="reader-chrome reader-top">
        <header className="reader-bar">
          <BackLink target={back} />
          <h1 title={heading}>{heading}</h1>
        </header>
      </div>
    </main>
  );
}

function PagePlaceholder() {
  const { t } = useI18n();
  return (
    <div className="reader-placeholder" role="status">
      <span className="sr-only">{t('loading')}</span>
      <Skeleton />
    </div>
  );
}

function useProgressWriter(id: string, signature: string, initial: Progress, enabled: boolean) {
  const revision = useRef(initial.revision);
  const confirmed = useRef<Position>({ page: initial.page, direction: initial.direction });
  const pending = useRef<(Position & { reset?: boolean }) | null>(null);
  const writing = useRef(false);
  const blocked = useRef(initial.reset_required);
  const mounted = useRef(true);
  const [state, setState] = useState<'saved' | 'saving' | 'error'>('saved');
  const [error, setError] = useState<unknown>();
  const [resetRequired, setResetRequired] = useState(initial.reset_required);
  useEffect(() => {
    mounted.current = true;
    return () => {
      mounted.current = false;
    };
  }, []);
  const enqueue = useCallback(
    (position: Position, reset = false) => {
      if (!enabled || (blocked.current && !reset)) return;
      if (
        !reset &&
        !writing.current &&
        position.page === confirmed.current.page &&
        position.direction === confirmed.current.direction
      )
        return;
      if (reset) blocked.current = false;
      pending.current = { ...position, reset };
      if (writing.current) return;
      writing.current = true;
      setState('saving');
      setError(undefined);
      void (async () => {
        try {
          while (pending.current && !blocked.current) {
            const next = pending.current;
            pending.current = null;
            const input: Schema['ProgressUpdate'] = {
              ...next,
              signature,
              revision: revision.current,
            };
            const saved = await request<Progress>(
              `/library/files/${encodeURIComponent(id)}/progress`,
              { method: 'PUT', body: JSON.stringify(input) },
            );
            revision.current = saved.revision;
            confirmed.current = { page: saved.page, direction: saved.direction };
            if (mounted.current) setResetRequired(saved.reset_required);
          }
          if (mounted.current) setState('saved');
        } catch (failure) {
          blocked.current = true;
          pending.current = null;
          if (mounted.current) {
            setError(failure);
            setState('error');
          }
        } finally {
          writing.current = false;
        }
      })();
    },
    [enabled, id, signature],
  );
  return { enqueue, state, error, resetRequired };
}

/* Chrome auto-hides after idle time; `hold` keeps it up (open menu, failures), and so does
   keyboard focus inside it. Mouse movement and Tab bring it back; touch uses a tap. */
function useChrome(hold: boolean) {
  const [shown, setShown] = useState(true);
  const holdRef = useRef(hold);
  holdRef.current = hold;
  const shownRef = useRef(shown);
  shownRef.current = shown;
  const top = useRef<HTMLDivElement>(null);
  const bottom = useRef<HTMLElement>(null);
  const timer = useRef<number | undefined>(undefined);
  const pinned = useCallback(() => {
    if (holdRef.current) return true;
    const active = document.activeElement;
    return (
      active instanceof HTMLElement &&
      active.tagName !== 'H1' &&
      active.matches(':focus-visible') &&
      !!(top.current?.contains(active) || bottom.current?.contains(active))
    );
  }, []);
  const schedule = useCallback(() => {
    window.clearTimeout(timer.current);
    timer.current = window.setTimeout(function tick() {
      if (pinned()) timer.current = window.setTimeout(tick, HIDE_AFTER_MS);
      else setShown(false);
    }, HIDE_AFTER_MS);
  }, [pinned]);
  const reveal = useCallback(() => {
    setShown(true);
    schedule();
  }, [schedule]);
  const conceal = useCallback(() => {
    if (pinned()) return;
    window.clearTimeout(timer.current);
    setShown(false);
  }, [pinned]);
  useEffect(() => {
    if (hold) setShown(true);
    schedule();
  }, [hold, schedule]);
  useEffect(() => {
    const move = (event: PointerEvent) => {
      if (event.pointerType !== 'touch') reveal();
    };
    const key = (event: KeyboardEvent) => {
      if (event.key === 'Tab') reveal();
      else if (shownRef.current) schedule();
    };
    window.addEventListener('pointermove', move);
    window.addEventListener('keydown', key);
    return () => {
      window.removeEventListener('pointermove', move);
      window.removeEventListener('keydown', key);
      window.clearTimeout(timer.current);
    };
  }, [reveal, schedule]);
  return { shown, reveal, conceal, top, bottom };
}

function ReaderDocument({
  manifest,
  initial,
  canSave,
  heading,
  back,
  reload,
}: {
  manifest: Schema['ReaderManifest'];
  initial: Progress;
  canSave: boolean;
  heading: string;
  back: BackTarget;
  reload: () => void;
}) {
  const { t, locale } = useI18n();
  const total = manifest.page_count;
  const [page, setPage] = useState(initial.reset_required ? 0 : initial.page);
  const [direction, setDirection] = useState<Direction>(initial.direction);
  const [fit, setFit] = useState<Fit>(initial.direction === 'vertical' ? 'width' : 'page');
  const [zoom, setZoom] = useState<Zoom>('100');
  const [loadedPage, setLoadedPage] = useState<number>();
  const [pageFailed, setPageFailed] = useState(false);
  const [displayOpen, setDisplayOpen] = useState(false);
  const stage = useRef<HTMLDivElement>(null);
  const gesture = useRef<{ id: number; x: number; y: number; left: number; top: number } | null>(
    null,
  );
  const currentPage = useRef(page);
  currentPage.current = page;
  const writer = useProgressWriter(manifest.file_id, manifest.signature, initial, canSave);
  const chrome = useChrome(displayOpen || pageFailed || !!writer.error);
  const turn = useCallback(
    (delta: number) => setPage((value) => Math.max(0, Math.min(total - 1, value + delta))),
    [total],
  );
  /* Previous/next as seen on screen: in right-to-left reading the left side moves forward. */
  const leftDelta = direction === 'rtl' ? 1 : -1;
  useEffect(() => {
    if (loadedPage === page) writer.enqueue({ page, direction });
  }, [page, direction, loadedPage, writer.enqueue]);
  useEffect(() => {
    const element = stage.current;
    element?.scrollTo({ top: 0, left: direction === 'rtl' ? element.scrollWidth : 0 });
    // Only a page change resets the scroll position; display changes keep it.
  }, [page]);
  useEffect(() => {
    const navigate = (event: KeyboardEvent) => {
      const target = event.target;
      if (
        target instanceof HTMLElement &&
        (target.closest('input,select,textarea,[role="radiogroup"],[role="dialog"]') ||
          target.isContentEditable)
      )
        return;
      if (event.ctrlKey || event.metaKey || event.altKey || event.shiftKey) return;
      let delta = 0;
      if (direction === 'vertical') {
        if (event.key === 'PageDown') delta = 1;
        if (event.key === 'PageUp') delta = -1;
      } else {
        if (event.key === 'ArrowRight') delta = -leftDelta;
        if (event.key === 'ArrowLeft') delta = leftDelta;
      }
      if (delta) {
        event.preventDefault();
        turn(delta);
      }
    };
    window.addEventListener('keydown', navigate);
    return () => window.removeEventListener('keydown', navigate);
  }, [direction, leftDelta, turn]);

  function onPointerDown(event: ReactPointerEvent<HTMLDivElement>) {
    const element = stage.current;
    if (!element || !event.isPrimary || event.button !== 0) return;
    gesture.current = {
      id: event.pointerId,
      x: event.clientX,
      y: event.clientY,
      left: element.scrollLeft,
      top: element.scrollTop,
    };
  }
  function onPointerUp(event: ReactPointerEvent<HTMLDivElement>) {
    const start = gesture.current;
    gesture.current = null;
    const element = stage.current;
    if (!element || !start || start.id !== event.pointerId) return;
    if (event.target instanceof Element && event.target.closest('button,a,input,.reader-message'))
      return;
    const panned =
      Math.abs(element.scrollLeft - start.left) > 2 || Math.abs(element.scrollTop - start.top) > 2;
    if (panned) return;
    const dx = event.clientX - start.x;
    const dy = event.clientY - start.y;
    if (Math.abs(dx) < TAP_SLOP_PX && Math.abs(dy) < TAP_SLOP_PX) {
      const rect = element.getBoundingClientRect();
      const position = (event.clientX - rect.left) / rect.width;
      if (position < 1 / 3) {
        turn(leftDelta);
        chrome.conceal();
      } else if (position > 2 / 3) {
        turn(-leftDelta);
        chrome.conceal();
      } else if (chrome.shown) chrome.conceal();
      else chrome.reveal();
      return;
    }
    const zoomedIn = element.scrollWidth > element.clientWidth + 1;
    if (
      event.pointerType !== 'mouse' &&
      !zoomedIn &&
      Math.abs(dx) >= SWIPE_MIN_PX &&
      Math.abs(dx) > Math.abs(dy) * 1.5
    ) {
      // A leftward swipe brings in the page on the right.
      turn(dx < 0 ? -leftDelta : leftDelta);
      chrome.conceal();
    }
  }

  const status: { icon: TablerIcon; text: string; title: string; warn?: boolean } = !canSave
    ? { icon: IconLock, text: t('readerStatusReadOnly'), title: t('readerReadOnly') }
    : writer.error || writer.resetRequired
      ? {
          icon: IconAlertTriangle,
          text: t('readerStatusNotSaved'),
          title: t(writer.error ? 'progressNotSaved' : 'resetProgressHint'),
          warn: true,
        }
      : writer.state === 'saving'
        ? { icon: IconCloudUpload, text: t('readerStatusSaving'), title: t('savingProgress') }
        : loadedPage !== page
          ? { icon: IconClock, text: t('readerStatusWaiting'), title: t('waitingForPage') }
          : { icon: IconCloudCheck, text: t('readerStatusSaved'), title: t('savedProgress') };

  const turnButton = (side: 'left' | 'right') => {
    const delta = side === 'left' ? leftDelta : -leftDelta;
    const label = t(delta > 0 ? 'next' : 'previous');
    const disabled = delta > 0 ? page >= total - 1 : page <= 0;
    return (
      <Button className="reader-turn" disabled={disabled} title={label} onClick={() => turn(delta)}>
        {side === 'left' && <Icon icon={IconChevronLeft} />}
        <span className="reader-label">{label}</span>
        {side === 'right' && <Icon icon={IconChevronRight} />}
      </Button>
    );
  };

  return (
    <main
      className="reader-view"
      id="main-content"
      tabIndex={-1}
      data-chrome={chrome.shown ? 'shown' : 'hidden'}
    >
      <div
        ref={stage}
        className={`reader-stage fit-${fit}${fit === 'width' && Number(zoom) > 100 ? ' pannable' : ''}`}
        onPointerDown={onPointerDown}
        onPointerUp={onPointerUp}
        onPointerCancel={() => {
          gesture.current = null;
        }}
      >
        <PageImage
          fileId={manifest.file_id}
          page={page}
          total={total}
          fit={fit}
          zoom={zoom}
          onLoaded={(loaded) => {
            if (currentPage.current === loaded) setLoadedPage(loaded);
          }}
          onFailed={setPageFailed}
        />
      </div>
      <div className="reader-chrome reader-top" ref={chrome.top} onFocus={chrome.reveal}>
        <header className="reader-bar">
          <BackLink target={back} />
          <h1 title={heading}>{heading}</h1>
          <DisplayPanel
            open={displayOpen}
            setOpen={setDisplayOpen}
            direction={direction}
            onDirection={(value) => {
              setDirection(value);
              if (value === 'vertical') setFit('width');
            }}
            fit={fit}
            onFit={(value) => {
              setFit(value);
              setZoom('100');
            }}
            zoom={zoom}
            onZoom={(value) => {
              setZoom(value);
              setFit('width');
            }}
          />
        </header>
        {canSave && writer.resetRequired && !writer.error && (
          <div className="notice reader-notice">
            <p>{t('resetProgressHint')}</p>
            <Button
              disabled={writer.state === 'saving' || loadedPage !== page}
              onClick={() => writer.enqueue({ page, direction }, true)}
            >
              {t('resetProgress')}
            </Button>
          </div>
        )}
        {!!writer.error && (
          <div className="reader-notice">
            <ErrorNotice error={writer.error} context={t('progressNotSaved')} />
            <Button icon={IconRefresh} onClick={reload}>
              {t('reloadProgress')}
            </Button>
          </div>
        )}
      </div>
      <footer className="reader-chrome reader-bottom" ref={chrome.bottom} onFocus={chrome.reveal}>
        <nav className="reader-pager" aria-label={t('reader')}>
          {turnButton('left')}
          <label className="reader-page-number">
            <span>{t('page')}</span>
            <input
              key={page}
              type="text"
              inputMode="numeric"
              autoComplete="off"
              aria-label={t('goToPage')}
              size={Math.max(2, String(total).length)}
              defaultValue={page + 1}
              onKeyDown={(event) => {
                if (event.key === 'Enter') event.currentTarget.blur();
                if (event.key === 'Escape') {
                  event.currentTarget.value = String(page + 1);
                  event.currentTarget.blur();
                }
              }}
              onBlur={(event) => {
                const value = Number(event.currentTarget.value.trim());
                if (Number.isInteger(value) && value >= 1 && value <= total) setPage(value - 1);
                else event.currentTarget.value = String(page + 1);
              }}
            />
            <span>{t('readerOfTotal').replace('{total}', total.toLocaleString(locale))}</span>
          </label>
          {turnButton('right')}
        </nav>
        <p className={`reader-save${status.warn ? ' warn' : ''}`} role="status" title={status.title}>
          <Icon icon={status.icon} size={16} />
          {status.text}
        </p>
      </footer>
      {!chrome.shown && status.warn && (
        <p className="reader-unsaved" aria-hidden="true">
          <Icon icon={IconAlertTriangle} size={14} />
          {status.text}
        </p>
      )}
    </main>
  );
}

function DisplayPanel({
  open,
  setOpen,
  direction,
  onDirection,
  fit,
  onFit,
  zoom,
  onZoom,
}: {
  open: boolean;
  setOpen: (open: boolean) => void;
  direction: Direction;
  onDirection: (value: Direction) => void;
  fit: Fit;
  onFit: (value: Fit) => void;
  zoom: Zoom;
  onZoom: (value: Zoom) => void;
}) {
  const { t } = useI18n();
  const id = useId();
  const root = useRef<HTMLDivElement>(null);
  const trigger = useRef<HTMLButtonElement>(null);
  const panel = useRef<HTMLDivElement>(null);
  const finePointer = useMediaQuery('(hover: hover) and (pointer: fine)');
  useEffect(() => {
    if (!open) return;
    panel.current?.querySelector<HTMLElement>('[role="radio"][tabindex="0"]')?.focus();
    const outside = (event: PointerEvent) => {
      if (!root.current?.contains(event.target as Node)) setOpen(false);
    };
    document.addEventListener('pointerdown', outside);
    return () => document.removeEventListener('pointerdown', outside);
  }, [open, setOpen]);
  const onBlur = (event: ReactFocusEvent<HTMLDivElement>) => {
    const next = event.relatedTarget;
    if (open && next instanceof Node && !root.current?.contains(next)) setOpen(false);
  };
  const label = t('readerDisplay');
  return (
    <div className="reader-display" ref={root} onBlur={onBlur}>
      <button
        ref={trigger}
        type="button"
        className="ghost reader-display-trigger"
        aria-expanded={open}
        aria-controls={open ? id : undefined}
        title={label}
        onClick={() => setOpen(!open)}
      >
        <Icon icon={IconAdjustmentsHorizontal} />
        <span className="reader-label">{label}</span>
      </button>
      {open && (
        <div
          ref={panel}
          id={id}
          className="reader-display-panel"
          role="dialog"
          aria-label={label}
          onKeyDown={(event) => {
            if (event.key === 'Escape') {
              event.preventDefault();
              setOpen(false);
              trigger.current?.focus();
            }
          }}
        >
          <div className="reader-option">
            <span aria-hidden="true">{t('readingDirection')}</span>
            <SegmentedControl<Direction>
              label={t('readingDirection')}
              value={direction}
              onChange={onDirection}
              options={[
                { value: 'ltr', label: t('readerDirLtr') },
                { value: 'rtl', label: t('readerDirRtl') },
                { value: 'vertical', label: t('readerDirVertical') },
              ]}
            />
          </div>
          <div className="reader-option">
            <span aria-hidden="true">{t('readerFit')}</span>
            <SegmentedControl<Fit>
              label={t('readerFit')}
              value={fit}
              onChange={onFit}
              options={[
                { value: 'width', label: t('readerFitWidth') },
                { value: 'page', label: t('readerFitPage') },
              ]}
            />
          </div>
          <div className="reader-option">
            <span aria-hidden="true">{t('zoom')}</span>
            <SegmentedControl<Zoom>
              label={t('zoom')}
              value={zoom}
              onChange={onZoom}
              options={zooms.map((value) => ({ value, label: `${value}%` }))}
            />
          </div>
          {finePointer && (
            <p className="reader-keys">
              <Icon icon={IconKeyboard} size={18} />
              <span>
                <strong>{t('readerKeyboard')}</strong>{' '}
                {t(
                  direction === 'rtl'
                    ? 'rtlKeys'
                    : direction === 'vertical'
                      ? 'verticalKeys'
                      : 'ltrKeys',
                )}
              </span>
            </p>
          )}
        </div>
      )}
    </div>
  );
}

const imageDecodeError = (t: Translate) => new ApiError(422, 'image_decode_failed', t('imageFailed'));

/* Keeps the previous page on screen (dimmed) until the next one arrives, so turning pages
   does not flash an empty canvas. Only the requested page is fetched. */
function PageImage({
  fileId,
  page,
  total,
  fit,
  zoom,
  onLoaded,
  onFailed,
}: {
  fileId: string;
  page: number;
  total: number;
  fit: Fit;
  zoom: Zoom;
  onLoaded: (page: number) => void;
  onFailed: (failed: boolean) => void;
}) {
  const { t, locale } = useI18n();
  const [version, setVersion] = useState(0);
  const [shown, setShown] = useState<{ page: number; url: string }>();
  const [failure, setFailure] = useState<{ page: number; error: unknown }>();
  useEffect(() => {
    const controller = new AbortController();
    setFailure(undefined);
    requestImage(`/library/files/${encodeURIComponent(fileId)}/pages/${page}`, controller.signal)
      .then((blob) => {
        if (controller.signal.aborted) return;
        setShown({ page, url: URL.createObjectURL(blob) });
      })
      .catch((error) => {
        if (!controller.signal.aborted) setFailure({ page, error });
      });
    return () => controller.abort();
  }, [fileId, page, version]);
  const url = shown?.url;
  useEffect(
    () => () => {
      if (url) URL.revokeObjectURL(url);
    },
    [url],
  );
  const failed = failure?.page === page;
  useEffect(() => onFailed(failed), [failed, onFailed]);
  if (failed)
    return (
      <div className="reader-center">
        <div className="reader-message">
          <ErrorNotice error={failure.error} retry={() => setVersion((value) => value + 1)} />
        </div>
      </div>
    );
  if (!shown)
    return (
      <div className="reader-center">
        <PagePlaceholder />
      </div>
    );
  const stale = shown.page !== page;
  return (
    <img
      key={shown.url}
      className={`reader-page${stale ? ' stale' : ''}`}
      src={shown.url}
      alt={t('readerPageOf')
        .replace('{page}', (shown.page + 1).toLocaleString(locale))
        .replace('{total}', total.toLocaleString(locale))}
      aria-busy={stale || undefined}
      draggable={false}
      style={fit === 'width' ? { width: `${zoom}%` } : undefined}
      onLoad={() => onLoaded(shown.page)}
      onError={() => setFailure({ page: shown.page, error: imageDecodeError(t) })}
    />
  );
}
