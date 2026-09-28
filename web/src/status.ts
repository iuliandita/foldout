import type { MessageKey } from './i18n';
import type { Schema } from './lib/api/client';
import type { StatusKind } from './ui';

export type UnitStatus = { kind: StatusKind; label?: MessageKey; readable: boolean };

/* One vocabulary for a unit's file state, shared by every screen:
   File, Missing, Not checked, Changed, Scan problem. Derived from persisted scan state only. */
export function unitStatus(item: Schema['WantedUnit']): UnitStatus {
  if (item.association_status === 'none') return { kind: 'missing', readable: false };
  if (item.counts.changed > 0) return { kind: 'warning', label: 'wantedStatusChanged', readable: true };
  if (item.counts.scan_error > 0 || item.counts.scan_unavailable > 0)
    return { kind: 'error', label: 'wantedStatusScanProblem', readable: true };
  if (item.availability === 'confirmed_present' || item.counts.present > 0) return { kind: 'file', readable: true };
  if (item.counts.missing > 0 && item.counts.missing >= item.counts.associated) return { kind: 'missing', readable: false };
  return { kind: 'unchecked', readable: true };
}
