import { useState } from 'react';
import { IconBook, IconBook2, IconNews } from '@tabler/icons-react';
import type { Schema } from './lib/api/client';
import { Icon } from './ui';

const typeIcons = { comic: IconBook, manga: IconBook2, magazine: IconNews } as const;

/** Cover thumbnail at a fixed 5:7 aspect; falls back to a neutral tile with the title or initial and a type icon. */
export function Cover({
  fileId,
  title,
  contentType,
  width = 40,
  decorative = true,
}: {
  fileId?: string | null;
  title: string;
  contentType: Schema['PublicationSummary']['content_type'];
  /** Numeric widths are fixed sizes; 'fill' takes the container width. */
  width?: 40 | 56 | 120 | 'fill';
  /** Decorative next to a visible title: empty alt. */
  decorative?: boolean;
}) {
  const [failed, setFailed] = useState<string | null>(null);
  const size = width === 'fill' ? 'fill' : width === 120 ? 'lg' : width === 56 ? 'md' : 'sm';
  const alt = decorative ? '' : title;
  if (fileId && failed !== fileId)
    return (
      <span className={`cover ${size}`}>
        <img
          src={`/api/v1/library/files/${encodeURIComponent(fileId)}/thumbnail`}
          alt={alt}
          loading="lazy"
          decoding="async"
          width={width === 'fill' ? 240 : width}
          height={width === 'fill' ? 336 : (width * 7) / 5}
          onError={() => setFailed(fileId)}
        />
      </span>
    );
  const initial = Array.from(title.trim())[0]?.toLocaleUpperCase() ?? '';
  return (
    <span
      className={`cover placeholder ${size} ${contentType}`}
      role={decorative ? undefined : 'img'}
      aria-label={decorative ? undefined : title}
      aria-hidden={decorative ? true : undefined}
    >
      {size === 'fill' ? <span className="cover-title">{title}</span> : <span className="cover-initial">{initial}</span>}
      <Icon icon={typeIcons[contentType]} size={size === 'fill' ? 24 : size === 'sm' ? 14 : 20} />
    </span>
  );
}
