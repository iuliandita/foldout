import { useState } from 'react';
import { useI18n } from './i18n';
import { ApiError, post, type Schema } from './lib/api/client';
import { ErrorNotice, Field, Loading, PageHeader, SaveForm, useResource, value } from './ui';
import {
  ActivityStatus,
  BackLink,
  RelativeTime,
  TechnicalDetails,
  unitName,
  usePolled,
  useUnitContext,
} from './Activity';

export function readDestination(
  data: FormData,
  message: string,
): Schema['DestinationSelection'] | null {
  const root = value(data, 'destination_root');
  if (!root) return null;
  return {
    root_id: root,
    relative_path: relativePath(value(data, 'destination_path'), message),
  };
}
function relativePath(path: string, message: string): string {
  if (
    !path ||
    path.length > 4096 ||
    /[\\\x00-\x1f\x7f]/.test(path) ||
    path.split('/').some((part) => !part || part === '.' || part === '..')
  )
    throw new ApiError(0, 'invalid_input', message);
  return path;
}
export function DestinationFields({ optional = false }: { optional?: boolean }) {
  const { t } = useI18n();
  const roots = useResource<Schema['RootChoiceList']>('/acquisition/roots');
  const [root, setRoot] = useState('');
  return (
    <>
      <label className="field">
        <span>
          {t('destinationRoot')}
          {!optional && ' *'}
        </span>
        <select
          name="destination_root"
          required={!optional}
          value={root}
          onChange={(event) => setRoot(event.target.value)}
        >
          <option value="">{t(optional ? 'chooseLater' : 'chooseRoot')}</option>
          {roots.data?.items.map((item) => (
            <option key={item.id} value={item.id}>
              {item.label}
            </option>
          ))}
        </select>
      </label>
      {roots.loading && <Loading />}
      <ErrorNotice error={roots.error} retry={roots.reload} />
      {roots.data?.items.length === 0 && <p>{t('noRoots')}</p>}
      {root && (
        <Field
          name="destination_path"
          label={t('destinationPath')}
          hint={t('relativePathHint')}
          required
          maxLength={4096}
        />
      )}
    </>
  );
}
export function AcquisitionDetail({ id, canManage }: { id: string; canManage: boolean }) {
  const { t, locale } = useI18n();
  const result = usePolled<Schema['Acquisition']>(`/acquisition/${encodeURIComponent(id)}`);
  const item = result.data;
  const unit = useUnitContext(item?.unit_id);
  return (
    <>
      <BackLink />
      {result.loading ? (
        <Loading />
      ) : !item ? (
        <ErrorNotice error={result.error} retry={result.reload} />
      ) : (
        <div className="activity-detail">
          <PageHeader
            title={
              unit.data
                ? t('activityDownloadOf').replace('{title}', unitName(unit.data))
                : t('activityDownload')
            }
            meta={
              <span className="activity-detail-meta">
                <ActivityStatus state={item.state} attention={item.state === 'downloaded'} />
                <span>
                  {t('updated')} <RelativeTime seconds={item.updated_at} />
                </span>
              </span>
            }
          />
          <ErrorNotice error={result.error} retry={result.reload} />
          {unit.data && (
            <p className="activity-detail-link">
              <a href={`#/publication/${encodeURIComponent(unit.data.publication.id)}`}>
                {unit.data.publication.title}
              </a>
            </p>
          )}
          {item.reason && <p className="notice">{t(item.reason)}</p>}
          <dl className="job-details">
            <dt>{t('activitySource')}</dt>
            <dd>{t('activityDownloadClient')}</dd>
            <dt>{t('activitySent')}</dt>
            <dd>{t(item.submitted ? 'submissionAttempted' : 'submissionNotAttempted')}</dd>
            <dt>{t('updated')}</dt>
            <dd>{new Date(item.updated_at * 1000).toLocaleString(locale)}</dd>
          </dl>
          <div className="actions">
            <a className="button" href={`#/job/${encodeURIComponent(item.job_id)}`}>
              {t('activityOpenTask')}
            </a>
          </div>
          {item.state === 'completed' && <p className="notice">{t('completedImportHint')}</p>}
          {item.state === 'downloaded' && (
            <>
              <p className="notice">{t('downloadedHint')}</p>
              {canManage && (
                <FileAssociation id={id} reserved={item.destination_reserved} saved={result.reload} />
              )}
            </>
          )}
          <TechnicalDetails
            rows={[
              [t('acquisitionId'), <code>{item.id}</code>],
              [t('activityUnitId'), <code>{item.unit_id}</code>],
              [t('activityTaskId'), <code>{item.job_id}</code>],
              [t('activityImportId'), item.import_id ? <code>{item.import_id}</code> : t('unknown')],
              [t('receiptCount'), item.receipt_count],
            ]}
          />
        </div>
      )}
    </>
  );
}
function FileAssociation({
  id,
  reserved,
  saved,
}: {
  id: string;
  reserved: boolean;
  saved: () => void;
}) {
  const { t } = useI18n();
  const roots = useResource<Schema['RootChoiceList']>('/acquisition/roots');
  return (
    <SaveForm
      label={t('confirmFileAssociation')}
      submit={async (data) => {
        const destination = reserved
          ? undefined
          : readDestination(data, t('invalidRelativePath'));
        if (!reserved && !destination) throw new ApiError(0, 'invalid_input', t('chooseRoot'));
        const input: Schema['FileAssociation'] = {
          source_root_id: value(data, 'source_root'),
          source_relative_path: relativePath(
            value(data, 'source_path'),
            t('invalidRelativePath'),
          ),
          destination,
        };
        await post<Schema['Acquisition']>(`/acquisition/${id}/files`, input);
        saved();
      }}
    >
      <p>{t('copyAssociationHint')}</p>
      {roots.loading && <Loading />}
      <ErrorNotice error={roots.error} retry={roots.reload} />
      <label className="field">
        <span>{t('sourceRoot')} *</span>
        <select name="source_root" required defaultValue="">
          <option value="" disabled>
            {t('chooseRoot')}
          </option>
          {roots.data?.items.map((root) => (
            <option key={root.id} value={root.id}>
              {root.label}
            </option>
          ))}
        </select>
      </label>
      <Field
        name="source_path"
        label={t('sourceRelativePath')}
        hint={t('relativePathHint')}
        required
        maxLength={4096}
      />
      {reserved ? (
        <p className="notice">{t('reservedDestinationHint')}</p>
      ) : (
        <DestinationFields />
      )}
    </SaveForm>
  );
}
