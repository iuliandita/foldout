import { useRef, useState } from 'react';
import { useI18n, type MessageKey } from './i18n';
import { ApiError, request, type Schema } from './lib/api/client';
import { Button, EmptyState, ErrorNotice, Field, Loading, SaveForm, SectionHeader, useResource, value, optional } from './ui';

const kinds = {
  comicvine: 'Comic Vine',
  mangaupdates: 'MangaUpdates',
  mangadex: 'MangaDex',
  getcomics: 'GetComics',
  internetarchive: 'Internet Archive',
  prowlarr: 'Prowlarr',
  sabnzbd: 'SABnzbd',
  qbittorrent: 'qBittorrent',
} as const;
type Kind = Schema['IntegrationKind'];
type Integration = Schema['Integration'];
type SecretName = 'api_key' | 'username' | 'password';
type TestResult = Schema['IntegrationTestResult'];
const endpoint = '/settings/integrations';
/* Ways to start when nothing is configured; each opens the add form preset to a kind the API supports. */
const starts: { kind: Kind; title: MessageKey; hint: MessageKey }[] = [
  { kind: 'prowlarr', title: 'sourcesStartIndexer', hint: 'sourcesStartIndexerHint' },
  { kind: 'sabnzbd', title: 'sourcesStartClient', hint: 'sourcesStartClientHint' },
  { kind: 'getcomics', title: 'sourcesStartDirect', hint: 'sourcesStartDirectHint' },
];

export function Integrations() {
  const { t } = useI18n();
  const list = useResource<Schema['IntegrationList']>(endpoint);
  const [editing, setEditing] = useState<Integration | 'new'>();
  const [startKind, setStartKind] = useState<Kind>();
  return (
    <section>
      <SectionHeader
        title={t('sourcesAndClients')}
        description={t('integrationHint')}
        help={<p>{t('helpMonitor')}</p>}
        action={{
          label: t('addIntegration'),
          creates: true,
          disabled: !!editing,
          onClick: () => {
            setStartKind(undefined);
            setEditing('new');
          },
        }}
      />
      {editing && (
        <IntegrationForm
          key={editing === 'new' ? 'new' : editing.id}
          integration={editing === 'new' ? undefined : editing}
          initialKind={startKind}
          cancel={() => setEditing(undefined)}
          saved={() => {
            setEditing(undefined);
            list.reload();
          }}
        />
      )}
      {list.loading ? (
        <Loading />
      ) : list.error ? (
        <ErrorNotice error={list.error} retry={list.reload} />
      ) : (
        <ul className="integration-list">
          {list.data?.items.map((item) => (
            <IntegrationRow
              key={item.id}
              item={item}
              editing={!!editing}
              edit={() => setEditing(item)}
              reload={list.reload}
            />
          ))}
        </ul>
      )}
      {list.data?.items.length === 0 && !editing && (
        <EmptyState
          title={t('sourcesEmptyTitle')}
          action={
            <ul className="source-starts">
              {starts.map((start) => (
                <li key={start.kind}>
                  <Button
                    onClick={() => {
                      setStartKind(start.kind);
                      setEditing('new');
                    }}
                  >
                    {t(start.title)}
                  </Button>
                  <p>{t(start.hint)}</p>
                </li>
              ))}
            </ul>
          }
        >
          {t('sourcesEmptyText')}
        </EmptyState>
      )}
    </section>
  );
}

function IntegrationRow({
  item,
  editing,
  edit,
  reload,
}: {
  item: Integration;
  editing: boolean;
  edit: () => void;
  reload: () => void;
}) {
  const { t } = useI18n();
  const [confirming, setConfirming] = useState(false);
  const [busy, setBusy] = useState<'test' | 'delete'>();
  const active = useRef(false);
  const [error, setError] = useState<unknown>();
  const [result, setResult] = useState<TestResult>();
  async function run(action: 'test' | 'delete') {
    if (active.current) return;
    active.current = true;
    setBusy(action);
    setError(undefined);
    setResult(undefined);
    try {
      if (action === 'test')
        setResult(
          await request<TestResult>(`${endpoint}/${item.id}/test`, { method: 'POST' }),
        );
      else {
        await request(`${endpoint}/${item.id}`, { method: 'DELETE' });
        reload();
      }
    } catch (failure) {
      setError(failure);
    } finally {
      active.current = false;
      setBusy(undefined);
    }
  }
  return (
    <li>
      <h3>{item.label}</h3>
      <p className="muted">
        {kinds[item.kind]}, {t(item.enabled ? 'enabled' : 'disabled')},{' '}
        {t(
          item.kind === 'internetarchive'
            ? 'archiveNoAuth'
            : item.credentials_configured
              ? 'credentialsReady'
              : 'credentialsMissing',
        )}
      </p>
      <p className="path">{item.base_url}</p>
      {item.kind === 'getcomics' && <p>{t('getComicsPlanned')}</p>}
      {item.kind === 'internetarchive' && <p>{t('archiveConfigurationHint')}</p>}
      <p className="muted">{t('testHint')}</p>
      <div className="actions">
        <button disabled={!!busy || editing} onClick={edit}>
          {t('edit')}
        </button>
        <button
          disabled={
            !!busy ||
            editing ||
            !item.enabled ||
            (item.kind !== 'internetarchive' && !item.credentials_configured)
          }
          onClick={() => void run('test')}
        >
          {t(busy === 'test' ? 'testingConnection' : 'testConnection')}
        </button>
        <button
          className="danger"
          disabled={!!busy || editing}
          onClick={() => setConfirming(true)}
        >
          {t('deleteIntegration')}
        </button>
      </div>
      {confirming && (
        <div className="notice">
          <p>{t('deleteIntegrationHint')}</p>
          <div className="actions">
            <button
              className="danger"
              disabled={!!busy || editing}
              onClick={() => void run('delete')}
            >
              {t('deleteIntegration')}
            </button>
            <button disabled={!!busy} onClick={() => setConfirming(false)}>
              {t('cancel')}
            </button>
          </div>
        </div>
      )}
      <ErrorNotice error={error} />
      {result && (
        <p className="notice" role="status">
          {t('connectionPassed')}
          {result.version && (
            <>
              {' '}
              {t('version')}: {result.version}
            </>
          )}
        </p>
      )}
    </li>
  );
}

function SecretField({ name, configured }: { name: SecretName; configured: boolean }) {
  const { t } = useI18n();
  const [mode, setMode] = useState('preserve');
  const label = t(name === 'api_key' ? 'apiKey' : name);
  return (
    <div>
      <label className="field">
        <span>
          {label}: {t(configured ? 'configured' : 'notConfigured')}
        </span>
        <select
          name={`${name}_mode`}
          value={mode}
          onChange={(event) => setMode(event.target.value)}
        >
          <option value="preserve">{t('keepSecret')}</option>
          <option value="replace">{t('replaceSecret')}</option>
          <option value="clear">{t('clearSecret')}</option>
        </select>
      </label>
      {mode === 'replace' && (
        <Field
          name={name}
          label={label}
          type="password"
          required
          maxLength={4096}
          autoComplete="new-password"
          spellCheck={false}
        />
      )}
    </div>
  );
}

function IntegrationForm({
  integration,
  initialKind,
  cancel,
  saved,
}: {
  integration?: Integration;
  initialKind?: Kind;
  cancel: () => void;
  saved: () => void;
}) {
  const { t } = useI18n();
  const [kind, setKind] = useState<Kind>(integration?.kind ?? initialKind ?? 'comicvine');
  const options = integration?.options;
  const prowlarr = options && 'indexer_id' in options ? options : undefined;
  const client = options && 'category' in options ? options : undefined;
  function categories(data: FormData, name: string): number[] {
    const raw = value(data, name);
    if (!raw) return [];
    const parts = raw.split(',').map((part) => part.trim());
    if (
      parts.length > 100 ||
      parts.some(
        (part) => !/^\d+$/.test(part) || Number(part) < 1 || Number(part) > 4294967295,
      )
    )
      throw new ApiError(0, 'invalid_input', t('invalidCategories'));
    return parts.map(Number);
  }
  return (
    <SaveForm
      cancel={cancel}
      label={t(integration ? 'save' : 'addIntegration')}
      submit={async (data) => {
        let options: Schema['IntegrationOptionsInput'] = {};
        if (kind === 'prowlarr')
          options = {
            indexer_id: Number(value(data, 'indexer_id')),
            protocol: value(data, 'protocol') === 'torrent' ? 'torrent' : 'usenet',
            categories: {
              comics: categories(data, 'comics'),
              manga: categories(data, 'manga'),
              magazines: categories(data, 'magazines'),
            },
          };
        if (kind === 'sabnzbd' || kind === 'qbittorrent') {
          const remote = optional(data, 'remote_path');
          const local = optional(data, 'local_path');
          if (
            Boolean(remote) !== Boolean(local) ||
            [remote, local].some(
              (path) => path && (!path.startsWith('/') || path.split('/').includes('..')),
            )
          )
            throw new ApiError(0, 'invalid_input', t('invalidMapping'));
          options = {
            category: value(data, 'category'),
            remote_path: remote,
            local_path: local,
          };
        }
        const secrets: Partial<Record<SecretName, string | null>> = {};
        for (const name of ['api_key', 'username', 'password'] as const) {
          if (value(data, `${name}_mode`) === 'clear') secrets[name] = null;
          if (value(data, `${name}_mode`) === 'replace')
            secrets[name] = String(data.get(name) ?? '');
        }
        const fields = {
          label: value(data, 'label'),
          base_url: value(data, 'base_url'),
          enabled: value(data, 'enabled') === 'true',
          options,
          ...secrets,
        };
        const input: Schema['CreateIntegration'] | Schema['UpdateIntegration'] = integration
          ? fields
          : { ...fields, kind };
        await request(integration ? `${endpoint}/${integration.id}` : endpoint, {
          method: integration ? 'PATCH' : 'POST',
          body: JSON.stringify(input),
        });
        saved();
      }}
    >
      <h3>{t(integration ? 'editIntegration' : 'addIntegration')}</h3>
      <label className="field">
        <span>{t('integrationKind')}</span>
        <select
          disabled={!!integration}
          value={kind}
          onChange={(event) => {
            const next = event.target.value;
            if (next in kinds) setKind(next as Kind);
          }}
        >
          {Object.entries(kinds).map(([id, label]) => (
            <option key={id} value={id}>
              {label}
            </option>
          ))}
        </select>
      </label>
      <Field
        name="label"
        label={t('label')}
        defaultValue={integration?.label}
        required
        maxLength={100}
        autoFocus
      />
      <Field
        key={`base-url-${kind}`}
        name="base_url"
        label={t('baseUrl')}
        hint={t('baseUrlHint')}
        type="url"
        defaultValue={
          integration?.base_url ??
          (kind === 'internetarchive' ? 'https://archive.org/' : undefined)
        }
        required
        maxLength={2048}
      />
      <label className="field">
        <span>{t('state')}</span>
        <select name="enabled" defaultValue={String(integration?.enabled ?? false)}>
          <option value="false">{t('disabled')}</option>
          <option value="true">{t('enabled')}</option>
        </select>
      </label>
      {kind === 'getcomics' && <p className="notice">{t('getComicsPlanned')}</p>}
      {kind === 'internetarchive' && <p className="notice">{t('archiveConfigurationHint')}</p>}
      <div key={`options-${kind}`}>
        {kind === 'prowlarr' && (
          <>
            <Field
              name="indexer_id"
              label={t('indexerId')}
              type="number"
              min={1}
              max={2147483647}
              step={1}
              required
              defaultValue={prowlarr?.indexer_id}
            />
            <label className="field">
              <span>{t('protocol')}</span>
              <select name="protocol" defaultValue={prowlarr?.protocol ?? 'usenet'}>
                <option value="usenet">{t('usenet')}</option>
                <option value="torrent">{t('torrent')}</option>
              </select>
            </label>
            <p className="muted">{t('categoriesHint')}</p>
            <Field
              name="comics"
              label={t('comic')}
              defaultValue={prowlarr?.categories?.comics.join(', ')}
            />
            <Field
              name="manga"
              label={t('manga')}
              defaultValue={prowlarr?.categories?.manga.join(', ')}
            />
            <Field
              name="magazines"
              label={t('magazine')}
              defaultValue={prowlarr?.categories?.magazines.join(', ')}
            />
          </>
        )}
        {(kind === 'sabnzbd' || kind === 'qbittorrent') && (
          <>
            <Field
              name="category"
              label={t('category')}
              hint={t('categoryHint')}
              required
              maxLength={100}
              pattern="[A-Za-z0-9_\-]+"
              defaultValue={client?.category}
            />
            <p className="muted">{t('mappingHint')}</p>
            <Field
              name="remote_path"
              label={t('remotePath')}
              maxLength={4096}
              defaultValue={client?.remote_path ?? ''}
            />
            <Field
              name="local_path"
              label={t('localPath')}
              maxLength={4096}
              defaultValue={client?.local_path ?? ''}
            />
          </>
        )}
        {kind === 'internetarchive' ? (
          <p className="muted">{t('archiveNoAuth')}</p>
        ) : (
          <>
            <p className="muted">{t('secretHandling')}</p>
            {(['api_key', 'username', 'password'] as const).map((name) => (
              <SecretField
                key={name}
                name={name}
                configured={integration?.[`${name}_configured`] ?? false}
              />
            ))}
          </>
        )}
      </div>
    </SaveForm>
  );
}
