import {
  useCallback,
  useEffect,
  useId,
  useRef,
  useState,
  type ButtonHTMLAttributes,
  type FormEvent,
  type InputHTMLAttributes,
  type KeyboardEvent,
  type ReactNode,
} from 'react';
import {
  IconAlertCircle,
  IconAlertTriangle,
  IconBell,
  IconBellOff,
  IconCircleDashed,
  IconDots,
  IconFileCheck,
  IconFileX,
  IconHelpCircle,
  IconInfoCircle,
  IconCircleCheck,
  IconClockHour3,
  IconPlus,
  IconSearch,
  IconX,
  type TablerIcon,
} from '@tabler/icons-react';
import { ApiError, request } from './lib/api/client';
import { useI18n } from './i18n';

const cx = (...names: (string | false | null | undefined)[]) => names.filter(Boolean).join(' ');

/** DOM id from arbitrary keys; ids referenced by aria-labelledby/describedby cannot contain spaces. */
export const domId = (...parts: string[]) => parts.join('-').replace(/[^A-Za-z0-9_-]/g, '_');

/** A long path truncated in the middle: the start gets the ellipsis, the file name stays visible.
    The full path is in the title attribute and read in full by screen readers. */
export function PathText({ path, className }: { path: string; className?: string }) {
  const slash = path.lastIndexOf('/', path.length - 2);
  const tailStart = Math.max(slash + 1, path.length - 40);
  const head = path.slice(0, tailStart);
  const tail = path.slice(tailStart);
  return (
    <span className={cx('path-text', className)} title={path}>
      <span className="sr-only">{path}</span>
      {head && (
        <span className="path-head" aria-hidden="true">
          {head}
        </span>
      )}
      <span className="path-tail" aria-hidden="true">
        {tail}
      </span>
    </span>
  );
}

export type { TablerIcon };
export function Icon({ icon: Glyph, size = 20 }: { icon: TablerIcon; size?: number }) {
  return <Glyph className="icon" size={size} stroke={1.75} aria-hidden="true" focusable="false" />;
}

/** Foldout mark: a page with its top-right corner folded over, in the accent color. */
export function BrandMark() {
  return (
    <svg className="brand-mark" viewBox="0 0 24 24" aria-hidden="true" focusable="false">
      <path className="brand-mark-page" d="M6 3h9l6 6v10a2 2 0 0 1-2 2H6a2 2 0 0 1-2-2V5a2 2 0 0 1 2-2z" />
      <path className="brand-mark-fold" d="M15 3v4a2 2 0 0 0 2 2h4z" />
    </svg>
  );
}

/** Keyboard navigation for long lists: j/k (and ArrowDown/ArrowUp once a row has focus) move between
    elements marked `data-row` inside the container. Enter on a focused row activates its first link or
    button (a row that is itself a link opens natively). Each letter in `keys` opens the hash in the
    focused row's `data-row-<letter>` attribute. Ignored while typing in a field or with a modifier held. */
export function useRowKeys(container: { current: HTMLElement | null }, keys: readonly string[] = []) {
  const extra = keys.join('|');
  useEffect(() => {
    const letters = extra ? extra.split('|') : [];
    function onKeyDown(event: globalThis.KeyboardEvent) {
      const root = container.current;
      if (!root || event.defaultPrevented || event.isComposing) return;
      if (event.altKey || event.ctrlKey || event.metaKey || event.shiftKey) return;
      const target = event.target as HTMLElement | null;
      if (target?.closest('input, textarea, select, [contenteditable="true"], [role="dialog"], [role="menu"]')) return;
      const rows = Array.from(root.querySelectorAll<HTMLElement>('[data-row]'));
      if (!rows.length) return;
      const index = rows.indexOf(document.activeElement as HTMLElement);
      const row = rows[index];
      const down = event.key === 'j' || (!!row && event.key === 'ArrowDown');
      const up = event.key === 'k' || (!!row && event.key === 'ArrowUp');
      if (down || up) {
        event.preventDefault();
        const next = row ? Math.min(Math.max(index + (down ? 1 : -1), 0), rows.length - 1) : down ? 0 : rows.length - 1;
        rows[next].focus();
        rows[next].scrollIntoView({ block: 'nearest' });
        return;
      }
      if (!row) return;
      if (event.key === 'Enter' && target === row && !row.matches('a[href]')) {
        const open = row.querySelector<HTMLElement>('a[href], button:not([aria-haspopup]):not(:disabled)');
        if (open) {
          event.preventDefault();
          open.click();
        }
        return;
      }
      const href = letters.includes(event.key) ? row.getAttribute(`data-row-${event.key}`) : null;
      if (href) {
        event.preventDefault();
        location.hash = href;
      }
    }
    document.addEventListener('keydown', onKeyDown);
    return () => document.removeEventListener('keydown', onKeyDown);
  }, [container, extra]);
}

export type ButtonVariant = 'primary' | 'secondary' | 'ghost' | 'danger';
export type ButtonSize = 'sm' | 'md';
type ButtonProps = ButtonHTMLAttributes<HTMLButtonElement> & {
  variant?: ButtonVariant;
  size?: ButtonSize;
  icon?: TablerIcon;
};
export function Button({
  variant = 'secondary',
  size = 'md',
  icon,
  type = 'button',
  className,
  children,
  ...props
}: ButtonProps) {
  return (
    <button
      {...props}
      type={type}
      className={cx(variant !== 'secondary' && variant, size === 'sm' && 'sm', className)}
    >
      {icon && <Icon icon={icon} size={size === 'sm' ? 16 : 18} />}
      {children}
    </button>
  );
}
export function IconButton({
  icon,
  variant = 'ghost',
  size = 'md',
  className,
  title,
  ...props
}: Omit<ButtonProps, 'children' | 'icon'> & { icon: TablerIcon; 'aria-label': string }) {
  return (
    <Button
      {...props}
      variant={variant}
      size={size}
      title={title ?? props['aria-label']}
      className={cx('icon-button', className)}
    >
      <Icon icon={icon} size={size === 'sm' ? 18 : 20} />
    </Button>
  );
}

/* Roving focus for radiogroup/tablist/menu: returns the index to move to, or undefined. */
function nextIndex(key: string, index: number, count: number, vertical = false) {
  const back = vertical ? 'ArrowUp' : 'ArrowLeft';
  const forward = vertical ? 'ArrowDown' : 'ArrowRight';
  if (key === back || (!vertical && key === 'ArrowUp')) return (index - 1 + count) % count;
  if (key === forward || (!vertical && key === 'ArrowDown')) return (index + 1) % count;
  if (key === 'Home') return 0;
  if (key === 'End') return count - 1;
  return undefined;
}

export type Option<T extends string> = { value: T; label: string; icon?: TablerIcon };
export function SegmentedControl<T extends string>({
  label,
  value,
  options,
  onChange,
  disabled,
}: {
  label: string;
  value: T;
  options: readonly Option<T>[];
  onChange: (value: T) => void;
  disabled?: boolean;
}) {
  const refs = useRef<(HTMLButtonElement | null)[]>([]);
  const current = Math.max(
    0,
    options.findIndex((option) => option.value === value),
  );
  function onKeyDown(event: KeyboardEvent<HTMLButtonElement>, index: number) {
    const next = nextIndex(event.key, index, options.length);
    if (next === undefined) return;
    event.preventDefault();
    onChange(options[next].value);
    refs.current[next]?.focus();
  }
  return (
    <div className="segmented" role="radiogroup" aria-label={label}>
      {options.map((option, index) => (
        <button
          key={option.value}
          ref={(node) => {
            refs.current[index] = node;
          }}
          type="button"
          role="radio"
          aria-checked={index === current}
          tabIndex={index === current ? 0 : -1}
          disabled={disabled}
          onClick={() => onChange(option.value)}
          onKeyDown={(event) => onKeyDown(event, index)}
        >
          {option.icon && <Icon icon={option.icon} size={18} />}
          {option.label}
        </button>
      ))}
    </div>
  );
}

function hashParam(name: string) {
  const [, search = ''] = location.hash.replace(/^#/, '').split('?');
  return new URLSearchParams(search).get(name);
}
function setHashParam(name: string, value: string, replace: boolean) {
  const [path, search = ''] = location.hash.replace(/^#/, '').split('?');
  const params = new URLSearchParams(search);
  params.set(name, value);
  const next = `#${path || '/'}?${params}`;
  if (replace) {
    history.replaceState(history.state, '', next);
    window.dispatchEvent(new HashChangeEvent('hashchange'));
  } else location.hash = next;
}

export type TabItem<T extends string> = { id: T; label: string; icon?: TablerIcon };
export function Tabs<T extends string>({
  label,
  items,
  selected,
  onSelect,
  urlParam,
  children,
}: {
  label: string;
  items: readonly TabItem<T>[];
  selected?: T;
  onSelect?: (id: T) => void;
  /** Keeps the selected tab in this hash query parameter (back/forward and links work). */
  urlParam?: string;
  children: (id: T) => ReactNode;
}) {
  const base = useId();
  const refs = useRef<(HTMLButtonElement | null)[]>([]);
  const [local, setLocal] = useState<T | undefined>(selected);
  const [fromUrl, setFromUrl] = useState(() => (urlParam ? hashParam(urlParam) : null));
  useEffect(() => {
    if (!urlParam) return;
    const update = () => setFromUrl(hashParam(urlParam));
    window.addEventListener('hashchange', update);
    return () => window.removeEventListener('hashchange', update);
  }, [urlParam]);
  const wanted = urlParam ? (fromUrl ?? undefined) : (selected ?? local);
  const active = items.find((item) => item.id === wanted)?.id ?? items[0]?.id;
  function choose(id: T, replace: boolean) {
    if (urlParam) setHashParam(urlParam, id, replace);
    else setLocal(id);
    onSelect?.(id);
  }
  function onKeyDown(event: KeyboardEvent<HTMLButtonElement>, index: number) {
    if (event.key === 'ArrowUp' || event.key === 'ArrowDown') return;
    const next = nextIndex(event.key, index, items.length);
    if (next === undefined) return;
    event.preventDefault();
    choose(items[next].id, true);
    refs.current[next]?.focus();
  }
  if (active === undefined) return null;
  return (
    <div className="tabs">
      <div role="tablist" aria-label={label} aria-orientation="horizontal">
        {items.map((item, index) => (
          <button
            key={item.id}
            ref={(node) => {
              refs.current[index] = node;
            }}
            type="button"
            role="tab"
            id={`${base}-tab-${item.id}`}
            aria-controls={`${base}-panel`}
            aria-selected={item.id === active}
            tabIndex={item.id === active ? 0 : -1}
            onClick={() => item.id !== active && choose(item.id, false)}
            onKeyDown={(event) => onKeyDown(event, index)}
          >
            {item.icon && <Icon icon={item.icon} size={18} />}
            {item.label}
          </button>
        ))}
      </div>
      <div
        className="tab-panel"
        role="tabpanel"
        id={`${base}-panel`}
        aria-labelledby={`${base}-tab-${active}`}
        tabIndex={0}
      >
        {children(active)}
      </div>
    </div>
  );
}

export type StatusKind =
  | 'file'
  | 'missing'
  | 'unchecked'
  | 'monitored'
  | 'not_monitored'
  | 'warning'
  | 'error'
  | 'info'
  | 'done'
  | 'running';
const statusIcons: Record<StatusKind, TablerIcon> = {
  file: IconFileCheck,
  missing: IconFileX,
  unchecked: IconCircleDashed,
  monitored: IconBell,
  not_monitored: IconBellOff,
  warning: IconAlertTriangle,
  error: IconAlertCircle,
  info: IconInfoCircle,
  done: IconCircleCheck,
  running: IconClockHour3,
};
const statusLabels = {
  file: 'statusFile',
  missing: 'statusMissing',
  unchecked: 'statusUnchecked',
  monitored: 'statusMonitored',
  not_monitored: 'statusNotMonitored',
  warning: 'statusWarning',
  error: 'statusError',
  info: 'statusInfo',
  done: 'statusDone',
  running: 'statusRunning',
} as const;
export function StatusBadge({ kind, label }: { kind: StatusKind; label?: string }) {
  const { t } = useI18n();
  return (
    <span className={cx('status-badge', kind)}>
      <Icon icon={statusIcons[kind]} size={14} />
      {label ?? t(statusLabels[kind])}
    </span>
  );
}

export type MenuItem = {
  label: string;
  icon?: TablerIcon;
  href?: string;
  onSelect?: () => void;
  danger?: boolean;
  disabled?: boolean;
};
export function Menu({
  label,
  items,
  icon = IconDots,
  showLabel = false,
  placement = 'bottom',
  triggerClassName,
  renderTrigger,
}: {
  /** Accessible name of the trigger. */
  label: string;
  items: readonly MenuItem[];
  icon?: TablerIcon;
  /** Shows the label next to the icon instead of an icon-only trigger. */
  showLabel?: boolean;
  placement?: 'bottom' | 'top';
  triggerClassName?: string;
  /** Custom trigger content (icon and text); the button, ARIA and behavior stay managed here. */
  renderTrigger?: ReactNode;
}) {
  const id = useId();
  const [open, setOpen] = useState(false);
  const root = useRef<HTMLDivElement>(null);
  const trigger = useRef<HTMLButtonElement>(null);
  const itemRefs = useRef<(HTMLElement | null)[]>([]);
  const focusable = () =>
    itemRefs.current.filter(
      (item): item is HTMLElement => !!item && !(item instanceof HTMLButtonElement && item.disabled),
    );
  const close = useCallback((restoreFocus: boolean) => {
    setOpen(false);
    if (restoreFocus) trigger.current?.focus();
  }, []);
  useEffect(() => {
    if (!open) return;
    focusable()[0]?.focus();
    const outside = (event: PointerEvent) => {
      if (!root.current?.contains(event.target as Node)) close(false);
    };
    document.addEventListener('pointerdown', outside);
    return () => document.removeEventListener('pointerdown', outside);
  }, [open, close]);
  function onMenuKeyDown(event: KeyboardEvent<HTMLDivElement>) {
    if (event.key === 'Escape') {
      event.preventDefault();
      close(true);
      return;
    }
    if (event.key === 'Tab') {
      close(false);
      return;
    }
    const enabled = focusable();
    const index = enabled.indexOf(document.activeElement as HTMLElement);
    const next = nextIndex(event.key, Math.max(index, 0), enabled.length, true);
    if (next === undefined) return;
    event.preventDefault();
    enabled[next]?.focus();
  }
  return (
    <div className="menu" ref={root}>
      <button
        ref={trigger}
        type="button"
        className={cx(!showLabel && !renderTrigger && 'icon-button ghost', triggerClassName)}
        aria-haspopup="menu"
        aria-expanded={open}
        aria-controls={open ? id : undefined}
        aria-label={showLabel ? undefined : label}
        title={showLabel || renderTrigger ? undefined : label}
        onClick={() => setOpen((value) => !value)}
        onKeyDown={(event) => {
          if (event.key === 'ArrowDown' || event.key === 'ArrowUp') {
            event.preventDefault();
            setOpen(true);
          }
        }}
      >
        {renderTrigger ?? (
          <>
            <Icon icon={icon} />
            {showLabel && label}
          </>
        )}
      </button>
      {open && (
        <div
          className={cx('menu-popup', placement === 'top' && 'top')}
          id={id}
          role="menu"
          aria-label={label}
          onKeyDown={onMenuKeyDown}
        >
          {items.map((item, index) => {
            const content = (
              <>
                {item.icon && <Icon icon={item.icon} size={18} />}
                {item.label}
              </>
            );
            const ref = (node: HTMLElement | null) => {
              itemRefs.current[index] = node;
            };
            return item.href && !item.disabled ? (
              <a
                key={item.label}
                ref={ref}
                role="menuitem"
                tabIndex={-1}
                href={item.href}
                className={cx(item.danger && 'danger')}
                onClick={() => close(false)}
              >
                {content}
              </a>
            ) : (
              <button
                key={item.label}
                ref={ref}
                type="button"
                role="menuitem"
                tabIndex={-1}
                disabled={item.disabled}
                className={cx(item.danger && 'danger')}
                onClick={() => {
                  close(true);
                  item.onSelect?.();
                }}
              >
                {content}
              </button>
            );
          })}
        </div>
      )}
    </div>
  );
}

export function SearchField({
  label,
  value,
  onChange,
  placeholder,
  delay = 250,
  name,
  autoFocus,
}: {
  label: string;
  value: string;
  /** Called 250ms after typing stops, immediately on Enter or clear. */
  onChange: (value: string) => void;
  placeholder?: string;
  delay?: number;
  name?: string;
  autoFocus?: boolean;
}) {
  const { t } = useI18n();
  const id = useId();
  const input = useRef<HTMLInputElement>(null);
  const [text, setText] = useState(value);
  const timer = useRef<number | undefined>(undefined);
  const latest = useRef(onChange);
  useEffect(() => {
    latest.current = onChange;
  }, [onChange]);
  useEffect(() => {
    if (timer.current === undefined) setText(value);
  }, [value]);
  useEffect(() => () => window.clearTimeout(timer.current), []);
  function flush(next: string) {
    window.clearTimeout(timer.current);
    timer.current = undefined;
    if (next !== value) latest.current(next);
  }
  return (
    <div className="search-field" role="search">
      <label className="sr-only" htmlFor={id}>
        {label}
      </label>
      <Icon icon={IconSearch} size={18} />
      <input
        ref={input}
        id={id}
        name={name}
        type="search"
        value={text}
        placeholder={placeholder ?? label}
        autoFocus={autoFocus}
        autoComplete="off"
        spellCheck={false}
        onChange={(event) => {
          const next = event.target.value;
          setText(next);
          window.clearTimeout(timer.current);
          timer.current = window.setTimeout(() => flush(next), delay);
        }}
        onKeyDown={(event) => {
          if (event.key === 'Enter') {
            event.preventDefault();
            flush(text);
          }
        }}
      />
      {text && (
        <IconButton
          icon={IconX}
          size="sm"
          aria-label={t('clearSearch')}
          onClick={() => {
            setText('');
            flush('');
            input.current?.focus();
          }}
        />
      )}
    </div>
  );
}

export function Skeleton({ variant = 'block', short }: { variant?: 'block' | 'line'; short?: boolean }) {
  return <div className={cx('skeleton', variant === 'line' && 'line', short && 'short')} aria-hidden="true" />;
}

export function EmptyState({
  title,
  children,
  action,
}: {
  title: string;
  /** One sentence that teaches the next step. */
  children?: ReactNode;
  action?: ReactNode;
}) {
  return (
    <div className="empty-state">
      <h2>{title}</h2>
      {children && <p>{children}</p>}
      {action && <div className="actions">{action}</div>}
    </div>
  );
}

export function PageHeader({
  title,
  meta,
  actions,
}: {
  title: ReactNode;
  meta?: ReactNode;
  actions?: ReactNode;
}) {
  return (
    <header className="page-header">
      <div>
        <h1>{title}</h1>
        {meta && <p className="page-meta">{meta}</p>}
      </div>
      {actions && <div className="actions">{actions}</div>}
    </header>
  );
}

/** A "What's this?" disclosure: a real button that shows or hides the explanation (works by touch and keyboard, not hover). */
export function HelpTip({ label, children }: { label?: string; children: ReactNode }) {
  const { t } = useI18n();
  const id = useId();
  const [open, setOpen] = useState(false);
  return (
    <div className="help-tip">
      <button
        type="button"
        className="help-tip-toggle ghost"
        aria-expanded={open}
        aria-controls={id}
        onClick={() => setOpen((value) => !value)}
      >
        <Icon icon={IconHelpCircle} size={16} />
        {label ?? t('whatsThis')}
      </button>
      <div id={id} className="help-tip-body" hidden={!open}>
        {children}
      </div>
    </div>
  );
}

export type SectionAction = {
  label: string;
  onClick: () => void;
  disabled?: boolean;
  /** Creates something: shows the "+" icon. */
  creates?: boolean;
};
/** Section title with an optional one-line description and one primary action at the top right (stacked on narrow screens). */
export function SectionHeader({
  title,
  description,
  action,
  help,
}: {
  title: ReactNode;
  description?: ReactNode;
  action?: SectionAction;
  /** Short explanation of the terms used in this section, behind a "What's this?" disclosure. */
  help?: ReactNode;
}) {
  return (
    <header className="section-head">
      <div className="section-head-text">
        <h2>{title}</h2>
        {description && <p className="section-head-description">{description}</p>}
        {help && <HelpTip>{help}</HelpTip>}
      </div>
      {action && (
        <Button
          variant="primary"
          icon={action.creates ? IconPlus : undefined}
          disabled={action.disabled}
          onClick={action.onClick}
        >
          {action.label}
        </Button>
      )}
    </header>
  );
}

export function ErrorNotice({ error, retry, context }: { error: unknown; retry?: () => void; context?: string }) {
  const { t } = useI18n();
  if (!error) return null;
  const message =
    error instanceof ApiError
      ? error.code === 'uncertain_result'
        ? t('uncertainResult')
        : error.code === 'network_error'
          ? t('networkError')
          : error.code === 'invalid_response'
            ? t('invalidResponse')
            : error.message || t('requestFailed')
      : t('requestFailed');
  return (
    <div className="notice error" role="alert">
      {context && <p>{context}</p>}
      <p>{message}</p>
      {error instanceof ApiError && error.trace && (
        <small>
          {t('trace')}: {error.trace}
        </small>
      )}
      {retry && <Button onClick={retry}>{t('retry')}</Button>}
    </div>
  );
}
/** Loads `path` (null waits without requesting). */
export function useResource<T>(path: string | null) {
  const [version, setVersion] = useState(0);
  const [state, setState] = useState<{ path: string | null; data?: T; error?: unknown; loading: boolean }>(
    { path, loading: path !== null },
  );
  useEffect(() => {
    if (path === null) {
      setState({ path, loading: false });
      return;
    }
    const controller = new AbortController();
    setState({ path, loading: true });
    request<T>(path, { signal: controller.signal })
      .then((data) => {
        if (!controller.signal.aborted) setState({ path, data, loading: false });
      })
      .catch((error) => {
        if (!controller.signal.aborted) setState({ path, error, loading: false });
      });
    return () => controller.abort();
  }, [path, version]);
  return {
    ...(state.path === path
      ? state
      : { path, loading: path !== null, data: undefined, error: undefined }),
    reload: () => setVersion((value) => value + 1),
  };
}
/** A cursor-paged list that appends pages on loadMore; a new `path` (null waits) starts over. */
export function usePagedList<T>(path: string | null) {
  const [version, setVersion] = useState(0);
  const [state, setState] = useState<{
    key: string;
    items: T[];
    next: string | null;
    loading: boolean;
    loadingMore: boolean;
    error?: unknown;
    moreError?: unknown;
  }>({ key: '', items: [], next: null, loading: path !== null, loadingMore: false });
  const key = `${version}:${path}`;
  const more = useRef<AbortController | undefined>(undefined);
  useEffect(() => {
    more.current?.abort();
    if (path === null) {
      setState({ key, items: [], next: null, loading: false, loadingMore: false });
      return;
    }
    const controller = new AbortController();
    setState({ key, items: [], next: null, loading: true, loadingMore: false });
    request<{ items: T[]; next_cursor: string | null }>(path, { signal: controller.signal })
      .then((page) => {
        if (!controller.signal.aborted)
          setState({ key, items: page.items, next: page.next_cursor, loading: false, loadingMore: false });
      })
      .catch((error) => {
        if (!controller.signal.aborted)
          setState({ key, items: [], next: null, loading: false, loadingMore: false, error });
      });
    return () => {
      controller.abort();
      more.current?.abort();
    };
  }, [key]);
  const current =
    state.key === key ? state : { key, items: [] as T[], next: null, loading: path !== null, loadingMore: false };
  async function loadMore() {
    if (!path || !current.next || current.loadingMore) return;
    const controller = new AbortController();
    more.current = controller;
    const cursor = current.next;
    setState((value) => (value.key === key ? { ...value, loadingMore: true, moreError: undefined } : value));
    try {
      const page = await request<{ items: T[]; next_cursor: string | null }>(
        `${path}${path.includes('?') ? '&' : '?'}cursor=${encodeURIComponent(cursor)}`,
        { signal: controller.signal },
      );
      if (controller.signal.aborted) return;
      setState((value) =>
        value.key === key && value.next === cursor
          ? { ...value, items: [...value.items, ...page.items], next: page.next_cursor, loadingMore: false }
          : value,
      );
    } catch (error) {
      if (controller.signal.aborted) return;
      setState((value) => (value.key === key ? { ...value, loadingMore: false, moreError: error } : value));
    }
  }
  return { ...current, loadMore: () => void loadMore(), reload: () => setVersion((value) => value + 1) };
}
export function Loading() {
  const { t } = useI18n();
  return (
    <div className="loading" role="status">
      <span className="sr-only">{t('loading')}</span>
      <Skeleton />
      <Skeleton />
      <Skeleton short />
    </div>
  );
}
export function Field({
  label,
  hint,
  showRequiredMark = true,
  ...props
}: InputHTMLAttributes<HTMLInputElement> & { label: string; hint?: string; showRequiredMark?: boolean }) {
  const { t } = useI18n();
  const id = props.id ?? props.name;
  return (
    <label className="field" htmlFor={id}>
      <span>
        {label} {props.required ? (showRequiredMark ? '*' : null) : <small>({t('optional')})</small>}
      </span>
      <input {...props} id={id} aria-describedby={hint ? `${id}-hint` : undefined} />
      {hint && <small id={`${id}-hint`}>{hint}</small>}
    </label>
  );
}
export function SaveForm({
  children,
  submit,
  label,
  cancel,
  requiredHint = false,
}: {
  children: ReactNode;
  submit: (data: FormData) => Promise<void>;
  label?: string;
  cancel?: () => void;
  /** Shows "Fields marked * are required" above the fields. */
  requiredHint?: boolean;
}) {
  const { t } = useI18n();
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<unknown>();
  const active = useRef(false);
  async function onSubmit(event: FormEvent<HTMLFormElement>) {
    event.preventDefault();
    if (active.current) return;
    active.current = true;
    setBusy(true);
    setError(undefined);
    try {
      await submit(new FormData(event.currentTarget));
    } catch (failure) {
      setError(failure);
    } finally {
      active.current = false;
      setBusy(false);
    }
  }
  return (
    <form className="editor" onSubmit={onSubmit} aria-busy={busy}>
      {requiredHint && <p className="muted">{t('requiredHint')}</p>}
      <fieldset disabled={busy}>{children}</fieldset>
      <ErrorNotice error={error} />
      <div className="actions">
        <Button type="submit" variant="primary" disabled={busy}>
          {busy ? t('saving') : (label ?? t('save'))}
        </Button>
        {cancel && (
          <Button disabled={busy} onClick={cancel}>
            {t('cancel')}
          </Button>
        )}
      </div>
    </form>
  );
}
/** Decimal sizes (1 kB = 1000 bytes) in the current locale; one decimal from 1 MB up. Bytes use the symbol B. */
export function formatSize(bytes: number, locale: string) {
  if (bytes < 1000) return `${new Intl.NumberFormat(locale).format(bytes)} B`;
  const units = ['byte', 'kilobyte', 'megabyte', 'gigabyte', 'terabyte'] as const;
  let value = bytes;
  let index = 0;
  while (value >= 1000 && index < units.length - 1) {
    value /= 1000;
    index += 1;
  }
  return new Intl.NumberFormat(locale, {
    style: 'unit',
    unit: units[index],
    unitDisplay: 'short',
    maximumFractionDigits: index < 2 ? 0 : 1,
  }).format(value);
}
export const value = (data: FormData, key: string) => String(data.get(key) ?? '').trim();
export const optional = (data: FormData, key: string) => value(data, key) || null;
