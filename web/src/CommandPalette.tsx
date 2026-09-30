import { useCallback, useEffect, useId, useRef, useState, type KeyboardEvent } from 'react';
import {
  IconActivity,
  IconBooks,
  IconChevronRight,
  IconInbox,
  IconKeyboard,
  IconListSearch,
  IconPlus,
  IconSearch,
  IconSettings,
  IconX,
  type TablerIcon,
} from '@tabler/icons-react';
import { type Schema, request } from './lib/api/client';
import { type MessageKey, useI18n } from './i18n';
import { Cover } from './Cover';
import { Button, Icon, IconButton } from './ui';
import {
  printableShortcutsEnabled,
  setPrintableShortcuts,
  subscribePrintableShortcuts,
  usePrintableShortcuts,
} from './shortcuts';
import './styles/palette.css';

type Mode = 'commands' | 'help';
const openEvent = 'library:palette';
/** Opens the palette from anywhere (sidebar and top bar buttons). */
export function openCommandPalette(mode: Mode = 'commands') {
  window.dispatchEvent(new CustomEvent<Mode>(openEvent, { detail: mode }));
}

const isMac = () => /Mac|iPhone|iPad/.test(navigator.userAgent);
const typeNames = { comic: 'typeComic', manga: 'typeManga', magazine: 'typeMagazine' } as const;

const jumps: { key: string; section: MessageKey; href: string; icon: TablerIcon }[] = [
  { key: 'l', section: 'library', href: '#/', icon: IconBooks },
  { key: 'w', section: 'wanted', href: '#/wanted', icon: IconListSearch },
  { key: 'a', section: 'activity', href: '#/activity', icon: IconActivity },
  { key: 'r', section: 'review', href: '#/review', icon: IconInbox },
  { key: 's', section: 'settings', href: '#/settings', icon: IconSettings },
];

function isTyping(target: EventTarget | null) {
  if (!(target instanceof HTMLElement)) return false;
  return (
    target.isContentEditable ||
    target instanceof HTMLInputElement ||
    target instanceof HTMLTextAreaElement ||
    target instanceof HTMLSelectElement ||
    target.getAttribute('role') === 'combobox'
  );
}

/** Sidebar trigger with the shortcut hint; hidden on phones, where the top bar button is used. */
export function PaletteButton() {
  const { t } = useI18n();
  const mac = isMac();
  return (
    <button
      type="button"
      className="palette-trigger"
      aria-keyshortcuts={mac ? 'Meta+K' : 'Control+K'}
      onClick={() => openCommandPalette()}
    >
      <Icon icon={IconSearch} size={18} />
      <span>{t('paletteTrigger')}</span>
      <kbd aria-hidden="true">{mac ? 'Cmd K' : 'Ctrl K'}</kbd>
    </button>
  );
}
export function PaletteIconButton() {
  const { t } = useI18n();
  return (
    <IconButton
      icon={IconSearch}
      className="palette-topbar"
      aria-label={t('paletteOpen')}
      onClick={() => openCommandPalette()}
    />
  );
}

type Command = {
  id: string;
  label: string;
  meta?: string;
  icon?: TablerIcon;
  publication?: Schema['PublicationSummary'];
  shortcut?: string;
  run: () => void;
};
type Group = { id: string; label: string; items: Command[] };
type Found = { query: string; items?: Schema['PublicationSummary'][]; error?: boolean };

/** Ctrl/Cmd+K palette plus the single-key shortcuts ("/", "g" then a letter, "?"). */
export function CommandPalette({ canManage }: { canManage: boolean }) {
  const [mode, setMode] = useState<Mode | null>(null);
  const openRef = useRef(false);
  useEffect(() => {
    openRef.current = mode !== null;
  }, [mode]);
  useEffect(() => {
    let pendingG = 0;
    const unsubscribe = subscribePrintableShortcuts(() => {
      pendingG = 0;
    });
    function onKeyDown(event: globalThis.KeyboardEvent) {
      if (event.defaultPrevented || event.isComposing) return;
      if ((event.ctrlKey || event.metaKey) && !event.altKey && !event.shiftKey && event.key.toLowerCase() === 'k') {
        pendingG = 0;
        event.preventDefault();
        setMode((current) => (current ? null : 'commands'));
        return;
      }
      if (openRef.current || event.ctrlKey || event.metaKey || event.altKey || event.repeat) {
        pendingG = 0;
        return;
      }
      if (!printableShortcutsEnabled()) {
        pendingG = 0;
        return;
      }
      if (isTyping(event.target) || document.querySelector('[role="menu"]')) {
        pendingG = 0;
        return;
      }
      if (pendingG && Date.now() - pendingG < 1500) {
        pendingG = 0;
        const jump = jumps.find((item) => item.key === event.key.toLowerCase());
        if (jump) {
          event.preventDefault();
          location.hash = jump.href;
        }
        return;
      }
      pendingG = 0;
      if (event.key === 'g') {
        pendingG = Date.now();
      } else if (event.key === '/') {
        event.preventDefault();
        const field = document.querySelector<HTMLInputElement>('main input[type="search"]');
        if (field) {
          field.focus();
          field.select();
        } else setMode('commands');
      } else if (event.key === '?') {
        event.preventDefault();
        setMode('help');
      }
    }
    const onOpen = (event: Event) => setMode((event as CustomEvent<Mode>).detail ?? 'commands');
    window.addEventListener('keydown', onKeyDown);
    window.addEventListener(openEvent, onOpen);
    return () => {
      unsubscribe();
      window.removeEventListener('keydown', onKeyDown);
      window.removeEventListener(openEvent, onOpen);
    };
  }, []);
  return mode ? (
    <PaletteDialog mode={mode} setMode={setMode} close={() => setMode(null)} canManage={canManage} />
  ) : null;
}

function PaletteDialog({
  mode,
  setMode,
  close,
  canManage,
}: {
  mode: Mode;
  setMode: (mode: Mode) => void;
  close: () => void;
  canManage: boolean;
}) {
  const { t } = useI18n();
  const printableShortcuts = usePrintableShortcuts();
  const dialog = useRef<HTMLDialogElement>(null);
  const input = useRef<HTMLInputElement>(null);
  const base = useId();
  const [returnTo] = useState(() => document.activeElement as HTMLElement | null);
  const [query, setQuery] = useState('');
  const [active, setActive] = useState(0);
  const [found, setFound] = useState<Found>({ query: '' });
  const q = query.trim();

  useEffect(() => {
    const node = dialog.current;
    if (node && !node.open) node.showModal();
    return () => {
      if (returnTo?.isConnected) returnTo.focus();
    };
  }, [returnTo]);
  useEffect(() => {
    if (mode === 'commands') input.current?.focus();
  }, [mode]);

  useEffect(() => {
    if (!q) return;
    const controller = new AbortController();
    const timer = window.setTimeout(() => {
      const params = new URLSearchParams({ limit: '8', q });
      request<Schema['PublicationPage']>(`/publications?${params}`, { signal: controller.signal })
        .then((page) => {
          if (!controller.signal.aborted) setFound({ query: q, items: page.items });
        })
        .catch(() => {
          if (!controller.signal.aborted) setFound({ query: q, error: true });
        });
    }, 200);
    return () => {
      window.clearTimeout(timer);
      controller.abort();
    };
  }, [q]);

  const go = useCallback(
    (href: string) => {
      close();
      location.hash = href;
    },
    [close],
  );
  const match = (label: string) => !q || label.toLocaleLowerCase().includes(q.toLocaleLowerCase());
  const navigation: Command[] = jumps
    .map((jump) => ({
      id: `go-${jump.key}`,
      label: t(jump.section),
      icon: jump.icon,
      shortcut: printableShortcuts ? `g ${jump.key}` : undefined,
      run: () => go(jump.href),
    }))
    .filter((command) => match(command.label));
  const actions: Command[] = [
    ...(canManage
      ? [{ id: 'add', label: t('addPublication'), icon: IconPlus, run: () => go('#/new') }]
      : []),
    { id: 'help', label: t('shortcutsTitle'), icon: IconKeyboard, shortcut: printableShortcuts ? '?' : undefined, run: () => setMode('help') },
  ].filter((command) => match(command.label));
  /* While a new search runs, the previous results stay visible instead of flickering away. */
  const current = found.query === q ? found : undefined;
  const publications: Command[] = q
    ? (found.items ?? []).map((item) => ({
        id: `pub-${item.id}`,
        label: item.title,
        meta: [t(typeNames[item.content_type]), item.run_label].filter(Boolean).join(', '),
        publication: item,
        run: () => go(`#/publication/${encodeURIComponent(item.id)}`),
      }))
    : [];
  const groups: Group[] = [
    { id: 'publications', label: t('palettePublications'), items: publications },
    { id: 'navigation', label: t('paletteNavigation'), items: navigation },
    { id: 'actions', label: t('paletteActions'), items: actions },
  ].filter((group) => group.items.length);
  const flat = groups.flatMap((group) => group.items);
  const activeIndex = Math.min(active, Math.max(flat.length - 1, 0));
  const optionId = (command: Command) => `${base}-${command.id}`;
  const searching = !!q && !current;
  const status = searching
    ? t('paletteSearching')
    : current?.error
      ? t('paletteSearchFailed')
      : flat.length
        ? t('paletteResultCount').replace('{count}', String(flat.length))
        : t('paletteNoResults');

  useEffect(() => {
    const option = flat[activeIndex];
    if (option) document.getElementById(optionId(option))?.scrollIntoView({ block: 'nearest' });
  });

  function onInputKeyDown(event: KeyboardEvent<HTMLInputElement>) {
    if (event.key === 'ArrowDown' || event.key === 'ArrowUp') {
      event.preventDefault();
      if (!flat.length) return;
      const step = event.key === 'ArrowDown' ? 1 : -1;
      setActive((activeIndex + step + flat.length) % flat.length);
    } else if (event.key === 'Enter') {
      event.preventDefault();
      flat[activeIndex]?.run();
    }
  }
  /* Keep Tab inside the dialog; the page behind it is inert while the dialog is modal. */
  function onDialogKeyDown(event: KeyboardEvent<HTMLDialogElement>) {
    if (event.key !== 'Tab') return;
    const focusable = Array.from(
      dialog.current?.querySelectorAll<HTMLElement>('input, button:not(:disabled), a[href]') ?? [],
    );
    if (!focusable.length) return;
    const first = focusable[0];
    const last = focusable[focusable.length - 1];
    if (event.shiftKey && document.activeElement === first) {
      event.preventDefault();
      last.focus();
    } else if (!event.shiftKey && document.activeElement === last) {
      event.preventDefault();
      first.focus();
    }
  }

  const mac = isMac();
  return (
    <dialog
      ref={dialog}
      className="palette"
      aria-modal="true"
      aria-labelledby={`${base}-title`}
      onKeyDown={onDialogKeyDown}
      onCancel={(event) => {
        event.preventDefault();
        close();
      }}
      onClick={(event) => {
        if (event.target === dialog.current) close();
      }}
    >
      {mode === 'help' ? (
        <div className="palette-help">
          <header className="palette-help-header">
            <h2 id={`${base}-title`}>{t('shortcutsTitle')}</h2>
            <IconButton icon={IconX} aria-label={t('close')} onClick={close} autoFocus />
          </header>
          <dl className="shortcut-list">
            <div>
              <dt>
                <kbd>{mac ? 'Cmd' : 'Ctrl'}</kbd> <kbd>K</kbd>
              </dt>
              <dd>{t('shortcutPalette')}</dd>
            </div>
            {printableShortcuts && (
              <>
                <div>
                  <dt>
                    <kbd>/</kbd>
                  </dt>
                  <dd>{t('shortcutSearch')}</dd>
                </div>
                {jumps.map((jump) => (
                  <div key={jump.key}>
                    <dt>
                      <kbd>g</kbd> <kbd>{jump.key}</kbd>
                    </dt>
                    <dd>{t('shortcutGoTo').replace('{section}', t(jump.section))}</dd>
                  </div>
                ))}
                <div>
                  <dt>
                    <kbd>?</kbd>
                  </dt>
                  <dd>{t('shortcutHelp')}</dd>
                </div>
                <div>
                  <dt>
                    <kbd>j</kbd> <kbd>k</kbd>
                  </dt>
                  <dd>{t('shortcutRowMove')}</dd>
                </div>
              </>
            )}
            <div>
              <dt>
                <kbd>{t('keyUp')}</kbd> <kbd>{t('keyDown')}</kbd>
              </dt>
              <dd>{t('shortcutRowMove')}</dd>
            </div>
            <div>
              <dt>
                <kbd>Enter</kbd>
              </dt>
              <dd>{t('shortcutRowOpen')}</dd>
            </div>
            {printableShortcuts && (
              <div>
                <dt>
                  <kbd>f</kbd>
                </dt>
                <dd>{t('shortcutRowFind')}</dd>
              </div>
            )}
            <div>
              <dt>
                <kbd>Esc</kbd>
              </dt>
              <dd>{t('shortcutClose')}</dd>
            </div>
          </dl>
          <p className="palette-note">{t(printableShortcuts ? 'shortcutsNote' : 'printableShortcutsDisabled')}</p>
          <div className="actions">
            {!printableShortcuts && (
              <Button onClick={() => setPrintableShortcuts(true)}>{t('enablePrintableShortcuts')}</Button>
            )}
            <Button icon={IconSearch} onClick={() => setMode('commands')}>
              {t('paletteOpen')}
            </Button>
          </div>
        </div>
      ) : (
        <>
          <h2 id={`${base}-title`} className="sr-only">
            {t('paletteOpen')}
          </h2>
          <div className="palette-search">
            <Icon icon={IconSearch} size={20} />
            <input
              ref={input}
              type="text"
              role="combobox"
              aria-label={t('paletteInput')}
              aria-expanded="true"
              aria-controls={`${base}-list`}
              aria-autocomplete="list"
              aria-activedescendant={flat[activeIndex] ? optionId(flat[activeIndex]) : undefined}
              placeholder={t('palettePlaceholder')}
              autoComplete="off"
              spellCheck={false}
              value={query}
              onChange={(event) => {
                setQuery(event.target.value);
                setActive(0);
              }}
              onKeyDown={onInputKeyDown}
            />
            <IconButton icon={IconX} aria-label={t('close')} onClick={close} />
          </div>
          <div id={`${base}-list`} role="listbox" aria-label={t('paletteResults')} className="palette-list">
            {groups.map((group) => (
              <div key={group.id} role="group" aria-label={group.label} className="palette-group">
                <div className="palette-group-label" aria-hidden="true">
                  {group.label}
                </div>
                {group.items.map((command) => {
                  const index = flat.indexOf(command);
                  return (
                    <div
                      key={command.id}
                      id={optionId(command)}
                      role="option"
                      aria-selected={index === activeIndex}
                      className="palette-option"
                      onMouseMove={() => index !== activeIndex && setActive(index)}
                      onMouseDown={(event) => event.preventDefault()}
                      onClick={command.run}
                    >
                      {command.publication ? (
                        <Cover
                          fileId={command.publication.cover_file_id}
                          title={command.publication.title}
                          contentType={command.publication.content_type}
                        />
                      ) : (
                        command.icon && (
                          <span className="palette-option-icon">
                            <Icon icon={command.icon} size={18} />
                          </span>
                        )
                      )}
                      <span className="palette-option-text">
                        <span className="palette-option-label">{command.label}</span>
                        {command.meta && <span className="palette-option-meta">{command.meta}</span>}
                      </span>
                      {command.shortcut ? (
                        <kbd className="palette-option-shortcut" aria-hidden="true">
                          {command.shortcut}
                        </kbd>
                      ) : (
                        <Icon icon={IconChevronRight} size={16} />
                      )}
                    </div>
                  );
                })}
              </div>
            ))}
          </div>
          {(searching || current?.error || !flat.length) && (
            <p className="palette-empty" aria-hidden="true">
              {status}
            </p>
          )}
          <p className="sr-only" role="status">
            {status}
          </p>
          <footer className="palette-footer" aria-hidden="true">
            <span>
              <kbd>{t('keyUp')}</kbd> <kbd>{t('keyDown')}</kbd> {t('paletteHintMove')}
            </span>
            <span>
              <kbd>Enter</kbd> {t('paletteHintOpen')}
            </span>
            <span>
              <kbd>Esc</kbd> {t('paletteHintClose')}
            </span>
          </footer>
        </>
      )}
    </dialog>
  );
}
