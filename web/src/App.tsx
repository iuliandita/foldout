import { Component, Suspense, lazy, useEffect, useState, type ReactNode } from 'react';
import {
  IconActivity,
  IconBooks,
  IconDots,
  IconInbox,
  IconListSearch,
  IconLogout,
  IconPlus,
  IconSettings,
  type TablerIcon,
} from '@tabler/icons-react';
import { ApiError, request, type Schema } from './lib/api/client';
import { I18nProvider, useI18n, type MessageKey } from './i18n';
import { BrandMark, Button, ErrorNotice, Field, Icon, Loading, Menu, SaveForm, value } from './ui';
import { Library, PublicationDetail } from './Library';
import { Wanted } from './Wanted';
import { AllMonitors } from './Monitors';
import { Appearance, Settings } from './Settings';
import { Activity, JobDetail } from './Activity';
import { AcquisitionDetail } from './Acquisitions';
import { DirectAcquisitionDetail } from './Direct';
import { Review } from './Review';
import { CommandPalette, PaletteButton, PaletteIconButton } from './CommandPalette';
import './styles.css';

const Reader = lazy(() => import('./Reader').then((module) => ({ default: module.Reader })));
const Search = lazy(() => import('./Search').then((module) => ({ default: module.Search })));
const AddPublication = lazy(() => import('./AddPublication').then((module) => ({ default: module.AddPublication })));
class ReaderLoadBoundary extends Component<{ children: ReactNode }, { failed: boolean }> {
  state = { failed: false };
  static getDerivedStateFromError() {
    return { failed: true };
  }
  render() {
    return this.state.failed ? (
      <main id="main-content" className="reader" tabIndex={-1}>
        <ErrorNotice error={new Error('Reader failed to load')} retry={() => location.reload()} />
      </main>
    ) : this.props.children;
  }
}

/* Old hashes keep working: they are rewritten in place so back/forward stays clean. */
function redirect(hash: string) {
  const [path, search = ''] = hash.replace(/^#/, '').split('?');
  if (path === '/monitors' && !search) return '#/wanted?monitoring=monitored';
  if (path === '/storage') return '#/settings?section=storage';
  if (path === '/acquisitions' || path === '/direct-acquisitions') return '#/activity?kind=downloads';
  return undefined;
}
function currentHash() {
  const target = redirect(location.hash);
  if (target) history.replaceState(history.state, '', target);
  return location.hash;
}
function useRoute() {
  const [hash, setHash] = useState(currentHash);
  useEffect(() => {
    const update = () => setHash(currentHash());
    window.addEventListener('hashchange', update);
    return () => window.removeEventListener('hashchange', update);
  }, []);
  const [path, search = ''] = hash.replace(/^#/, '').split('?');
  return { path: path || '/', query: new URLSearchParams(search) };
}
type Section = 'library' | 'wanted' | 'activity' | 'review' | 'settings';
function sectionOf(path: string): Section {
  if (path === '/wanted' || path === '/monitors' || path === '/search') return 'wanted';
  if (
    path === '/activity' ||
    path === '/acquisitions' ||
    path === '/direct-acquisitions' ||
    path.startsWith('/acquisition/') ||
    path.startsWith('/direct-acquisition/') ||
    path.startsWith('/job/')
  )
    return 'activity';
  if (path === '/review') return 'review';
  if (path === '/settings' || path === '/storage') return 'settings';
  return 'library';
}
const navItems: { id: Section; href: string; icon: TablerIcon }[] = [
  { id: 'library', href: '#/', icon: IconBooks },
  { id: 'wanted', href: '#/wanted', icon: IconListSearch },
  { id: 'activity', href: '#/activity', icon: IconActivity },
  { id: 'review', href: '#/review', icon: IconInbox },
  { id: 'settings', href: '#/settings', icon: IconSettings },
];
type ReviewFeed = { items?: unknown[]; totals?: Record<string, unknown> };
function reviewCount(feed: unknown): number | undefined {
  if (!feed || typeof feed !== 'object') return undefined;
  const { items, totals } = feed as ReviewFeed;
  if (totals && typeof totals === 'object') {
    if (typeof totals.total === 'number') return totals.total;
    const numbers = Object.values(totals).filter((entry): entry is number => typeof entry === 'number');
    if (numbers.length) return numbers.reduce((sum, entry) => sum + entry, 0);
  }
  return Array.isArray(items) ? items.length : undefined;
}
/* Count for the Review nav badge; undefined (badge hidden) when the feed is missing or fails. */
function useReviewCount(enabled: boolean, path: string) {
  const [count, setCount] = useState<number>();
  useEffect(() => {
    if (!enabled) return;
    const controller = new AbortController();
    const load = () =>
      request<unknown>('/review', { signal: controller.signal })
        .then((feed) => {
          if (!controller.signal.aborted) setCount(reviewCount(feed));
        })
        .catch(() => {
          if (!controller.signal.aborted) setCount(undefined);
        });
    void load();
    const timer = window.setInterval(() => void load(), 60_000);
    return () => {
      controller.abort();
      window.clearInterval(timer);
    };
  }, [enabled, path]);
  return count;
}
function NavBadge({ count }: { count?: number }) {
  const { t } = useI18n();
  if (!count) return null;
  return (
    <span className="nav-badge">
      <span aria-hidden="true">{count > 99 ? '99+' : count}</span>
      <span className="sr-only">{`, ${t('reviewWaiting').replace('{count}', String(count))}`}</span>
    </span>
  );
}
function Manager() {
  const { t } = useI18n();
  const route = useRoute();
  const [auth, setAuth] = useState<'loading' | 'setup' | 'login' | 'ready'>('loading');
  const [session, setSession] = useState<Schema['Session']>();
  const [error, setError] = useState<unknown>();
  const [expired, setExpired] = useState(false);
  const [loggingOut, setLoggingOut] = useState(false);
  const [requiresSetupToken, setRequiresSetupToken] = useState(false);
  const reviewTotal = useReviewCount(auth === 'ready', route.path);
  async function loadSession() {
    setAuth('loading');
    setError(undefined);
    try {
      const setup = await request<Schema['SetupStatus']>('/auth/setup');
      setRequiresSetupToken(setup.requires_token ?? false);
      if (!setup.configured) {
        setAuth('setup');
        return;
      }
      try {
        setSession(await request<Schema['Session']>('/auth/session'));
        setAuth('ready');
      } catch (failure) {
        if (failure instanceof ApiError && failure.status === 401) setAuth('login');
        else throw failure;
      }
    } catch (failure) {
      setError(failure);
    }
  }
  useEffect(() => {
    void loadSession();
    const unauthorized = () => {
      setSession(undefined);
      setAuth('login');
      setExpired(true);
    };
    window.addEventListener('library:unauthorized', unauthorized);
    return () => window.removeEventListener('library:unauthorized', unauthorized);
  }, []);
  useEffect(() => {
    const title: MessageKey = route.path.startsWith('/reader/')
      ? 'reader'
      : route.path === '/search'
        ? 'search'
        : route.path === '/monitors'
          ? 'monitors'
          : route.path === '/new'
            ? 'addPublication'
            : sectionOf(route.path);
    document.title = `${t(title)} | ${t('app')}`;
  }, [route.path, t]);
  useEffect(() => {
    if (auth !== 'ready') return;
    const heading = document.querySelector<HTMLElement>('main h1');
    if (heading) {
      heading.tabIndex = -1;
      heading.focus();
    } else document.getElementById('main-content')?.focus();
  }, [route.path, auth]);
  if (auth !== 'ready')
    return (
      <main className="auth-layout">
        <div className="auth-brand">
          <span className="brand">
            <BrandMark />
            {t('app')}
          </span>
          <p>{t('brandCaption')}</p>
        </div>
        <div className="auth-content">
          {auth === 'loading' ? (
            error ? (
              <ErrorNotice error={error} retry={() => void loadSession()} />
            ) : (
              <Loading />
            )
          ) : (
            <>
              <h1>{t(auth === 'setup' ? 'setup' : 'login')}</h1>
              <p>{t(auth === 'setup' ? 'setupIntro' : 'loginIntro')}</p>
              {expired && (
                <p className="notice" role="status">
                  {t('sessionExpired')}
                </p>
              )}
              <SaveForm
                key={auth}
                label={t(auth === 'setup' ? 'setup' : 'login')}
                submit={async (data) => {
                  try {
                    const result = await request<Schema['Session']>(`/auth/${auth}`, {
                      method: 'POST',
                      headers:
                        auth === 'setup' && requiresSetupToken
                          ? { 'X-Setup-Token': value(data, 'setupToken') }
                          : undefined,
                      body: JSON.stringify({
                        username: value(data, 'username'),
                        password: String(data.get('password') ?? ''),
                      } satisfies Schema['Credentials']),
                    });
                    setSession(result);
                    setAuth('ready');
                    setExpired(false);
                    setError(undefined);
                  } catch (failure) {
                    if (failure instanceof ApiError && failure.code === 'already_configured')
                      setAuth('login');
                    throw failure;
                  }
                }}
              >
                {auth === 'setup' && requiresSetupToken && (
                  <Field
                    label={t('setupToken')}
                    name="setupToken"
                    type="password"
                    required
                    autoComplete="off"
                    maxLength={43}
                    hint={t('setupTokenHint')}
                  />
                )}
                <Field
                  label={t('username')}
                  name="username"
                  required
                  autoComplete="username"
                  autoFocus
                  maxLength={100}
                />
                <Field
                  label={t('password')}
                  name="password"
                  type="password"
                  required
                  autoComplete={auth === 'setup' ? 'new-password' : 'current-password'}
                  minLength={auth === 'setup' ? 12 : undefined}
                  maxLength={1024}
                  hint={auth === 'setup' ? t('passwordHint') : undefined}
                />
              </SaveForm>
            </>
          )}
          <Appearance />
        </div>
      </main>
    );
  const canManage = session?.scope !== 'read';
  const isAdmin = session?.scope === 'admin';
  if (/^\/reader\/[^/]+$/.test(route.path))
    return (
      <ReaderLoadBoundary key={route.path}>
        <Suspense fallback={<main id="main-content" className="reader" tabIndex={-1}><Loading /></main>}>
          <Reader
            id={route.path.split('/')[2]}
            query={route.query}
            canSave={canManage}
          />
        </Suspense>
      </ReaderLoadBoundary>
    );
  /* Search is reached from several places, so no nav item claims it. */
  const section = route.path === '/search' ? null : sectionOf(route.path);
  async function logout() {
    setLoggingOut(true);
    setError(undefined);
    try {
      await request('/auth/session', { method: 'DELETE' });
      setSession(undefined);
      setAuth('login');
    } catch (failure) {
      if (failure instanceof ApiError && failure.status === 401) {
        setSession(undefined);
        setAuth('login');
      } else setError(failure);
    } finally {
      setLoggingOut(false);
    }
  }
  const brand = (
    <a className="brand" href="#/" aria-label={t('app')}>
      <BrandMark />
      <span className="brand-name">{t('app')}</span>
    </a>
  );
  return (
    <div className="app-shell">
      <a
        className="skip-link"
        href="#main-content"
        onClick={(event) => {
          event.preventDefault();
          document.getElementById('main-content')?.focus();
        }}
      >
        {t('skipToContent')}
      </a>
      <header className="topbar">
        {brand}
        <PaletteIconButton />
        {canManage && (
          <a
            className="button ghost icon-button topbar-add"
            href="#/new"
            aria-label={t('addPublication')}
            title={t('addPublication')}
          >
            <Icon icon={IconPlus} />
          </a>
        )}
      </header>
      <aside className="sidebar">
        {brand}
        <p className="brand-caption">{t('brandCaption')}</p>
        <PaletteButton />
        <nav aria-label={t('mainNavigation')}>
          {navItems.map((item) => (
            <a key={item.id} href={item.href} aria-current={section === item.id ? 'page' : undefined}>
              <Icon icon={item.icon} />
              {t(item.id)}
              {item.id === 'review' && <NavBadge count={reviewTotal} />}
            </a>
          ))}
        </nav>
        <div className="sidebar-footer">
          <Button variant="ghost" icon={IconLogout} disabled={loggingOut} onClick={() => void logout()}>
            {t(loggingOut ? 'signingOut' : 'logout')}
          </Button>
        </div>
      </aside>
      <nav className="tabbar" aria-label={t('mainNavigation')}>
        {navItems
          .filter((item) => item.id !== 'settings')
          .map((item) => (
            <a key={item.id} href={item.href} aria-current={section === item.id ? 'page' : undefined}>
              <span className="tab-icon">
                <Icon icon={item.icon} size={22} />
                {item.id === 'review' && <NavBadge count={reviewTotal} />}
              </span>
              <span className="tab-label">{t(item.id)}</span>
            </a>
          ))}
        <Menu
          label={t('more')}
          placement="top"
          triggerClassName={section === 'settings' ? 'current' : undefined}
          renderTrigger={
            <>
              <span className="tab-icon">
                <Icon icon={IconDots} size={22} />
              </span>
              <span className="tab-label">{t('more')}</span>
            </>
          }
          items={[
            { label: t('settings'), icon: IconSettings, href: '#/settings' },
            {
              label: t(loggingOut ? 'signingOut' : 'logout'),
              icon: IconLogout,
              disabled: loggingOut,
              onSelect: () => void logout(),
            },
          ]}
        />
      </nav>
      <CommandPalette canManage={canManage} />
      <main id="main-content" tabIndex={-1}>
        <ErrorNotice error={error} />
        {!canManage && <p className="notice">{t('readOnly')}</p>}
        <Suspense fallback={<Loading />}>
        {route.path === '/' ? (
          <Library query={route.query} canManage={canManage} isAdmin={isAdmin} />
        ) : route.path === '/search' && canManage ? (
          <Search query={route.query} userId={session!.user_id} />
        ) : route.path === '/wanted' ? (
          <Wanted query={route.query} canManage={canManage} />
        ) : route.path === '/monitors' ? (
          <AllMonitors query={route.query} canManage={canManage} />
        ) : /^\/direct-acquisition\/[^/]+$/.test(route.path) && canManage ? (
          <DirectAcquisitionDetail key={route.path} id={route.path.split('/')[2]} />
        ) : /^\/acquisition\/[^/]+$/.test(route.path) ? (
          <AcquisitionDetail
            key={route.path}
            id={route.path.split('/')[2]}
            canManage={canManage}
          />
        ) : route.path === '/activity' ? (
          <Activity query={route.query} canManage={canManage} />
        ) : /^\/job\/[^/]+$/.test(route.path) ? (
          <JobDetail key={route.path} id={route.path.split('/')[2]} canManage={canManage} />
        ) : route.path === '/review' ? (
          <Review />
        ) : route.path === '/settings' ? (
          <Settings isAdmin={isAdmin} />
        ) : route.path === '/new' && canManage ? (
          <AddPublication query={route.query} isAdmin={isAdmin} />
        ) : /^\/publication\/[^/]+$/.test(route.path) ? (
          <PublicationDetail
            key={route.path}
            id={route.path.split('/')[2]}
            query={route.query}
            canManage={canManage}
          />
        ) : (
          <>
            <h1>{t('notFound')}</h1>
            <a className="button" href="#/">
              {t('back')}
            </a>
          </>
        )}
        </Suspense>
      </main>
    </div>
  );
}
export default function App() {
  return (
    <I18nProvider>
      <Manager />
    </I18nProvider>
  );
}
