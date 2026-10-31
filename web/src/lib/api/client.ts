import type { components } from './schema';
export type Schema = components['schemas'];
export type Page<T> = { items: T[]; next_cursor: string | null };
export class ApiError extends Error {
  constructor(
    public status: number,
    public code: string,
    message: string,
    public trace?: string,
    public existingUnitId?: string,
    public archiveConflict?: { existingUnit: Schema['ArchiveUnitMatch']; proposal: Schema['ArchiveUnitProposal'] },
  ) {
    super(message);
  }
}
async function fetchResponse(path: string, options: RequestInit = {}): Promise<Response> {
  let response: Response;
  try {
    response = await fetch(`/api/v1${path}`, {
      ...options,
      credentials: 'same-origin',
      cache: 'no-store',
      headers: {
        Accept: 'application/json',
        ...(options.body ? { 'Content-Type': 'application/json' } : {}),
        ...options.headers,
      },
    });
  } catch (error) {
    if (error instanceof DOMException && error.name === 'AbortError') throw error;
    throw new ApiError(
      0,
      options.method && options.method !== 'GET' ? 'uncertain_result' : 'network_error',
      '',
    );
  }
  if (!response.ok) {
    let body: unknown;
    try {
      body = await response.json();
    } catch {
      body = null;
    }
    const detail = body && typeof body === 'object' && 'error' in body ? body.error : null;
    const message =
      detail &&
      typeof detail === 'object' &&
      'message' in detail &&
      typeof detail.message === 'string'
        ? detail.message
        : '';
    const code =
      detail && typeof detail === 'object' && 'code' in detail && typeof detail.code === 'string'
        ? detail.code
        : 'request_failed';
    const trace =
      detail &&
      typeof detail === 'object' &&
      'trace_id' in detail &&
      typeof detail.trace_id === 'string'
        ? detail.trace_id
        : undefined;
    if (response.status === 401 && !['/auth/setup', '/auth/login', '/auth/session'].includes(path))
      window.dispatchEvent(new Event('library:unauthorized'));
    const existingUnitId = body && typeof body === 'object' && 'existing_unit_id' in body && typeof body.existing_unit_id === 'string'
      ? body.existing_unit_id : undefined;
    const archiveConflict = isRecord(body) && isArchiveUnitMatch(body.existing_unit) && isArchiveUnitProposal(body.proposal)
      ? { existingUnit: body.existing_unit, proposal: body.proposal } : undefined;
    throw new ApiError(response.status, code, message, trace, existingUnitId, archiveConflict);
  }
  return response;
}

export async function requestImage(path: string, signal: AbortSignal): Promise<Blob> {
  const response = await fetchResponse(path, { signal, headers: { Accept: 'image/*' } });
  const blob = await response.blob();
  if (!blob.type.startsWith('image/')) throw new ApiError(response.status, 'invalid_response', '');
  return blob;
}

export async function request<T>(path: string, options: RequestInit = {}): Promise<T> {
  const response = await fetchResponse(path, options);
  if (response.status === 204) return undefined as T;
  try {
    const data: unknown = await response.json();
    const endpoint = path.split('?')[0];
    const paginated =
      (endpoint === '/publications' && !options.method) ||
      (endpoint === '/jobs' && !options.method) ||
      (endpoint === '/monitors' && !options.method) ||
      (endpoint === '/wanted' && !options.method) ||
      (/\/(editions|units|entries)$/.test(endpoint) && !options.method);
    if (
      paginated &&
      (!data ||
        typeof data !== 'object' ||
        !('items' in data) ||
        !Array.isArray(data.items) ||
        !('next_cursor' in data) ||
        (data.next_cursor !== null && typeof data.next_cursor !== 'string'))
    )
      throw new Error('Invalid page');
    return data as T;
  } catch {
    throw new ApiError(response.status, 'invalid_response', '');
  }
}
export const post = <T>(path: string, body: unknown) =>
  request<T>(path, { method: 'POST', body: JSON.stringify(body) });

function isRecord(value: unknown): value is Record<string, unknown> {
  return value !== null && typeof value === 'object';
}
function isArchiveUnitMatch(value: unknown): value is Schema['ArchiveUnitMatch'] {
  return isRecord(value) && typeof value.id === 'string' && typeof value.label === 'string'
    && typeof value.edition_id === 'string' && typeof value.publication_id === 'string';
}
function isArchiveUnitProposal(value: unknown): value is Schema['ArchiveUnitProposal'] {
  const strings = (items: unknown): items is string[] => Array.isArray(items) && items.every((item: unknown) => typeof item === 'string');
  return isRecord(value) && typeof value.edition_id === 'string' && typeof value.label === 'string'
    && typeof value.kind === 'string' && ['issue', 'chapter', 'volume', 'special', 'combined'].includes(value.kind)
    && (value.date === null || typeof value.date === 'string')
    && (value.date_precision === null || value.date_precision === 'year' || value.date_precision === 'month' || value.date_precision === 'day')
    && strings(value.raw_labels) && strings(value.raw_volumes) && strings(value.raw_dates)
    && typeof value.combined === 'boolean' && typeof value.identifier === 'string' && typeof value.source_url === 'string'
    && Array.isArray(value.date_warnings) && value.date_warnings.every(isArchiveUnitMatch);
}
