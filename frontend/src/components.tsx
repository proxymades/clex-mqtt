import { useEffect, useId, useRef } from 'react';
import type { ReactNode } from 'react';
import { CircleAlert, LoaderCircle, Search, X } from 'lucide-react';
import type { LucideIcon } from 'lucide-react';
import type { Resource } from './useResource';

export function Badge({ children, tone = 'neutral' }: { children: ReactNode; tone?: 'neutral' | 'green' | 'blue' | 'amber' | 'red' }) {
  return <span className={`badge badge-${tone}`}><span className="badge-dot" />{children}</span>;
}

export function ErrorNotice({ children }: { children: ReactNode }) {
  return <div className="notice notice-error" role="alert"><CircleAlert size={17} /><span>{children}</span></div>;
}

export function Loading({ label = 'Загружаем данные' }: { label?: string }) {
  return <div className="loading-state" role="status"><LoaderCircle className="spin" size={24} /><span>{label}</span></div>;
}

export function EmptyState({ icon: Icon, title, description, action }: { icon: LucideIcon; title: string; description: string; action?: ReactNode }) {
  return <div className="empty-state"><span className="empty-icon"><Icon size={26} strokeWidth={1.6} /></span><h3>{title}</h3><p>{description}</p>{action}</div>;
}

export function ResourceView<T>({ resource, children }: { resource: Resource<T>; children: (data: T) => ReactNode }) {
  if (resource.loading) return <div className="card"><Loading /></div>;
  if (resource.data === null) return <div className="card error-state"><CircleAlert size={28} /><h3>Ошибка загрузки данных</h3><p>{resource.error}</p><button className="button button-secondary" onClick={() => void resource.refresh()}>Повторить запрос</button></div>;
  return <>{resource.error && <ErrorNotice>Ошибка обновления. Показаны последние загруженные данные. {resource.error}</ErrorNotice>}{children(resource.data)}</>;
}

export function SearchField({ value, onChange, placeholder }: { value: string; onChange: (value: string) => void; placeholder: string }) {
  return <label className="search-field"><Search size={17} /><span className="sr-only">{placeholder}</span><input type="search" value={value} onChange={(event) => onChange(event.target.value)} placeholder={placeholder} /></label>;
}

export function Modal({ title, subtitle, children, onClose, wide = false }: { title: string; subtitle?: string; children: ReactNode; onClose: () => void; wide?: boolean }) {
  const titleId = useId();
  const dialog = useRef<HTMLDivElement>(null);
  const close = useRef(onClose);
  close.current = onClose;
  useEffect(() => {
    const previous = document.activeElement instanceof HTMLElement ? document.activeElement : null;
    const overflow = document.body.style.overflow;
    document.body.style.overflow = 'hidden';
    const frame = requestAnimationFrame(() => { dialog.current?.querySelector<HTMLElement>('input:not([disabled]), textarea:not([disabled]), button:not([disabled])')?.focus(); });
    const onKey = (event: KeyboardEvent) => {
      if (event.key === 'Escape') close.current();
      if (event.key !== 'Tab') return;
      const elements = dialog.current?.querySelectorAll<HTMLElement>('button:not([disabled]), input:not([disabled]), textarea:not([disabled]), select:not([disabled]), [tabindex="0"]');
      if (!elements?.length) return;
      const first = elements[0]; const last = elements[elements.length - 1];
      if (event.shiftKey && (document.activeElement === first || !dialog.current?.contains(document.activeElement))) { event.preventDefault(); last.focus(); }
      else if (!event.shiftKey && document.activeElement === last) { event.preventDefault(); first.focus(); }
    };
    document.addEventListener('keydown', onKey);
    return () => { cancelAnimationFrame(frame); document.body.style.overflow = overflow; document.removeEventListener('keydown', onKey); previous?.focus(); };
  }, []);
  return <div className="modal-backdrop" onMouseDown={(event) => { if (event.target === event.currentTarget) onClose(); }}><div className={`modal ${wide ? 'modal-wide' : ''}`} role="dialog" aria-modal="true" aria-labelledby={titleId} ref={dialog}><header className="modal-header"><div><h2 id={titleId}>{title}</h2>{subtitle && <p>{subtitle}</p>}</div><button type="button" className="icon-button" aria-label="Закрыть" onClick={onClose}><X size={20} /></button></header>{children}</div></div>;
}

export function ConfirmDialog({ title, description, label, busy, error, onConfirm, onClose, danger = true }: { title: string; description: ReactNode; label: string; busy: boolean; error: string | null; onConfirm: () => void; onClose: () => void; danger?: boolean }) {
  return <Modal title={title} onClose={() => { if (!busy) onClose(); }}><div className="modal-body"><p className="confirm-description">{description}</p>{error && <ErrorNotice>{error}</ErrorNotice>}</div><footer className="modal-footer"><button className="button button-secondary" disabled={busy} onClick={onClose}>Отмена</button><button className={`button ${danger ? 'button-danger' : 'button-primary'}`} disabled={busy} onClick={onConfirm}>{busy && <LoaderCircle className="spin" size={16} />}{label}</button></footer></Modal>;
}

export function DetailRow({ label, children }: { label: string; children: ReactNode }) {
  return <div className="detail-row"><dt>{label}</dt><dd>{children}</dd></div>;
}
