import { useId, useState, type ReactNode } from 'react';
import { IconKey, IconPlugConnected, IconAdjustmentsHorizontal, IconDatabase } from '@tabler/icons-react';
import { Integrations } from './Integrations';
import { Storage } from './Storage';
import { useI18n, preference, savePreference } from './i18n';
import { post, request, type Schema } from './lib/api/client';
import {
  Button,
  ErrorNotice,
  Field,
  Loading,
  PageHeader,
  SaveForm,
  SectionHeader,
  SegmentedControl,
  Tabs,
  useResource,
  value,
  type TabItem,
} from './ui';
import './styles/settings.css';
export type Theme = 'system' | 'light' | 'dark';
export function applyTheme(theme: Theme) {
  document.documentElement.dataset.theme = theme;
}
const storedTheme = preference('library.theme', 'system');
applyTheme(storedTheme === 'light' || storedTheme === 'dark' ? storedTheme : 'system');
function useTheme() {
  const [theme, setTheme] = useState<Theme>(() => {
    const current = document.documentElement.dataset.theme;
    return current === 'light' || current === 'dark' ? current : 'system';
  });
  return [
    theme,
    (next: Theme) => {
      setTheme(next);
      applyTheme(next);
      savePreference('library.theme', next);
    },
  ] as const;
}
function ThemeControl() {
  const { t } = useI18n();
  const [theme, setTheme] = useTheme();
  return (
    <SegmentedControl<Theme>
      label={t('theme')}
      value={theme}
      options={(['system', 'light', 'dark'] as const).map((option) => ({
        value: option,
        label: t(option),
      }))}
      onChange={setTheme}
    />
  );
}
function LanguageSelect({ id, describedBy }: { id?: string; describedBy?: string }) {
  const { t, locale, setLocale } = useI18n();
  return (
    <select
      id={id}
      aria-describedby={describedBy}
      value={locale}
      onChange={(event) => setLocale(event.target.value === 'es' ? 'es' : 'en')}
    >
      <option value="en">{t('english')}</option>
      <option value="es">{t('spanish')}</option>
    </select>
  );
}
/** Theme and language controls for the sign-in screen. */
export function Appearance() {
  const { t } = useI18n();
  return (
    <div className="preferences">
      <div>
        <span className="preference-label" aria-hidden="true">
          {t('theme')}
        </span>
        <ThemeControl />
      </div>
      <label className="field">
        <span>{t('interfaceLanguage')}</span>
        <LanguageSelect />
      </label>
    </div>
  );
}
function SettingRow({
  label,
  description,
  control,
  htmlFor,
}: {
  label: string;
  description?: string;
  control: (describedBy?: string) => ReactNode;
  htmlFor?: string;
}) {
  const id = useId();
  const describedBy = description ? `${id}-description` : undefined;
  return (
    <li className="setting-row">
      <div className="setting-row-text">
        {htmlFor ? (
          <label className="setting-row-label" htmlFor={htmlFor}>
            {label}
          </label>
        ) : (
          <span className="setting-row-label">{label}</span>
        )}
        {description && (
          <p className="setting-row-description" id={describedBy}>
            {description}
          </p>
        )}
      </div>
      <div className="setting-row-control">{control(describedBy)}</div>
    </li>
  );
}
/* Server version from /about; nothing is shown until the server reports one. */
function useServerVersion() {
  return useResource<Schema['About']>('/about').data?.version;
}
function General() {
  const { t } = useI18n();
  const languageId = useId();
  const version = useServerVersion();
  return (
    <section>
      <SectionHeader title={t('general')} description={t('generalHint')} />
      <ul className="setting-list">
        <SettingRow label={t('theme')} description={t('themeHint')} control={() => <ThemeControl />} />
        <SettingRow
          label={t('interfaceLanguage')}
          description={t('interfaceLanguageHint')}
          htmlFor={languageId}
          control={(describedBy) => <LanguageSelect id={languageId} describedBy={describedBy} />}
        />
        <SettingRow
          label={t('about')}
          description={t('aboutHint')}
          control={() => (
            <span className="setting-row-value">
              {t('app')}
              {version && ` ${version}`}
            </span>
          )}
        />
      </ul>
    </section>
  );
}
type Section = 'general' | 'sources' | 'storage' | 'keys';
export function Settings({ isAdmin }: { isAdmin: boolean }) {
  const { t } = useI18n();
  const sections: TabItem<Section>[] = [
    { id: 'general', label: t('general'), icon: IconAdjustmentsHorizontal },
    ...(isAdmin
      ? ([
          { id: 'sources', label: t('sourcesAndClients'), icon: IconPlugConnected },
          { id: 'storage', label: t('storage'), icon: IconDatabase },
        ] as const)
      : []),
    { id: 'keys', label: t('apiKeys'), icon: IconKey },
  ];
  return (
    <>
      <PageHeader title={t('settings')} />
      <div className="settings-tabs">
        <Tabs label={t('settingsSections')} items={sections} urlParam="section">
          {(section) =>
            section === 'general' ? (
              <General />
            ) : section === 'sources' ? (
              <Integrations />
            ) : section === 'storage' ? (
              <Storage />
            ) : (
              isAdmin ? (
                <Keys />
              ) : (
                <section>
                  <SectionHeader title={t('apiKeys')} />
                  <p>{t('accessRestricted')}</p>
                </section>
              )
            )
          }
        </Tabs>
      </div>
    </>
  );
}
function Keys() {
  const { t } = useI18n();
  const keys = useResource<Schema['KeyInfo'][]>('/auth/keys');
  const [secret, setSecret] = useState<Schema['CreatedKey']>();
  const [creating, setCreating] = useState(false);
  const [revoking, setRevoking] = useState<string>();
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<unknown>();
  return (
    <section>
      <SectionHeader
        title={t('apiKeys')}
        description={t('keysHint')}
        action={{
          label: t('createKey'),
          creates: true,
          disabled: creating || !!secret,
          onClick: () => setCreating(true),
        }}
      />
      {secret && (
        <div className="notice secret">
          <h3>{t('secretTitle')}</h3>
          <p>{t('secretHint')}</p>
          <label className="field">
            <span>{t('secret')}</span>
            <textarea
              readOnly
              value={secret.secret}
              spellCheck={false}
              autoComplete="off"
              onFocus={(event) => event.currentTarget.select()}
            />
          </label>
          <Button onClick={() => setSecret(undefined)}>{t('dismissSecret')}</Button>
        </div>
      )}
      {creating && (
        <SaveForm
          label={t('createKey')}
          cancel={() => setCreating(false)}
          submit={async (data) => {
            const scope = value(data, 'scope');
            const input: Schema['KeyInput'] = {
              name: value(data, 'name'),
              scope: scope === 'admin' ? 'admin' : scope === 'manage' ? 'manage' : 'read',
            };
            const created = await post<Schema['CreatedKey']>('/auth/keys', input);
            setSecret(created);
            setCreating(false);
            keys.reload();
          }}
        >
          <Field name="name" label={t('keyName')} required autoFocus maxLength={100} />
          <label className="field">
            <span>{t('scope')} *</span>
            <select name="scope" defaultValue="read">
              {(['read', 'manage', 'admin'] as const).map((scope) => (
                <option key={scope} value={scope}>
                  {t(scope)}
                </option>
              ))}
            </select>
          </label>
        </SaveForm>
      )}
      <ErrorNotice error={error} />
      {keys.loading ? (
        <Loading />
      ) : keys.error ? (
        <ErrorNotice error={keys.error} retry={keys.reload} />
      ) : (
        keys.data &&
        (keys.data.length ? (
          <ul className="key-list">
            {keys.data.map((key) => (
              <li key={key.id}>
                <div>
                  <strong>{key.name}</strong>
                  <span className="muted">{t(key.scope)}</span>
                </div>
                {revoking === key.id ? (
                  <div className="revocation">
                    <p>{t('confirmRevoke')}</p>
                    <div className="actions">
                      <button
                        className="danger"
                        disabled={busy}
                        onClick={async () => {
                          if (busy) return;
                          setBusy(true);
                          setError(undefined);
                          try {
                            await request(`/auth/keys/${key.id}`, { method: 'DELETE' });
                            setRevoking(undefined);
                            if (secret?.id === key.id) setSecret(undefined);
                            keys.reload();
                          } catch (failure) {
                            setError(failure);
                          } finally {
                            setBusy(false);
                          }
                        }}
                      >
                        {busy ? t('saving') : t('confirm')}
                      </button>
                      <button disabled={busy} onClick={() => setRevoking(undefined)}>
                        {t('cancel')}
                      </button>
                    </div>
                  </div>
                ) : (
                  <button className="danger" onClick={() => setRevoking(key.id)}>
                    {t('revoke')}
                  </button>
                )}
              </li>
            ))}
          </ul>
        ) : (
          <p className="empty-inline">{t('noKeys')}</p>
        ))
      )}
    </section>
  );
}
