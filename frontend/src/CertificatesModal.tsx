import { useId, useState } from 'react';
import { Fingerprint, LoaderCircle, Plus, Power, Trash2, Upload } from 'lucide-react';
import { errorMessage } from './api';
import type { ApiClient } from './api';
import type { Notify } from './App';
import { Badge, EmptyState, ErrorNotice, Modal, ResourceView } from './components';
import type { CertificateList } from './types';
import { fullDateTime } from './format';
import { useResource } from './useResource';

export function CertificatesModal({ username, api, revision, notify, onChange, onClose }: {
  username: string; api: ApiClient; revision: number; notify: Notify; onChange: () => void; onClose: () => void;
}) {
  const path = `/users/${encodeURIComponent(username)}/certificates`;
  const resource = useResource<CertificateList>(api, path, revision);
  const [adding, setAdding] = useState(false);
  const [label, setLabel] = useState('');
  const [pem, setPem] = useState('');
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [removing, setRemoving] = useState<string | null>(null);
  const fileId = useId();
  const mutate = async (route: string, method: 'POST' | 'PATCH' | 'DELETE', body?: unknown, success = 'Настройки сертификата сохранены.') => {
    setBusy(true); setError(null);
    try {
      await api(route, { method, body });
      if (method === 'POST') { setAdding(false); setLabel(''); setPem(''); }
      setRemoving(null); onChange(); notify(success);
    } catch (failure) { setError(errorMessage(failure)); }
    finally { setBusy(false); }
  };
  const readFile = async (file: File | undefined) => {
    if (!file) return;
    if (file.size > 12 * 1024) { setError('Выберите сертификат PEM размером до 12 КБ.'); return; }
    try { setPem(await file.text()); if (!label) setLabel(file.name.replace(/\.(pem|crt)$/i, '').slice(0, 80)); setError(null); }
    catch { setError('Откройте сертификат PEM и вставьте его содержимое в поле.'); }
  };
  return <Modal title="Сертификаты клиента" subtitle={username} wide onClose={() => { if (!busy) onClose(); }}>
    <div className="modal-body form-stack certificate-content">
      <p className="field-hint">Сертификат связывает устройство с MQTT-учётной записью. Добавьте открытый сертификат; закрытый ключ хранится на устройстве.</p>
      <ResourceView resource={resource}>{(data) => <>
        <div className="certificate-listener"><Fingerprint size={18} /><div><strong>{data.tls.mode === 'none' ? 'Вход по сертификату настраивается на TLS-порту' : data.tls.mode === 'required' ? 'TLS-порт требует сертификат клиента' : 'TLS-порт принимает сертификаты и обычные подключения'}</strong>{data.tls.address && <code>{data.tls.address}</code>}</div></div>
        <label className="certificate-policy"><input type="checkbox" aria-label="Требовать сертификат клиента" checked={data.requireClientCertificate} disabled={busy || (!data.requireClientCertificate && (data.tls.mode === 'none' || !data.certificates.some((certificate) => certificate.enabled && Date.parse(certificate.expiresAt) > Date.now())))} onChange={(event) => void mutate(`/users/${encodeURIComponent(username)}`, 'PATCH', { requireClientCertificate: event.target.checked }, 'Способ входа учётной записи обновлён.')} /><span><strong>Требовать сертификат клиента</strong><small>Для каждого подключения с этими учётными данными используется MQTT TLS-порт с зарегистрированным сертификатом.</small></span></label>
        {data.certificates.length ? <div className="certificate-list">{data.certificates.map((certificate) => {
          const expired = Date.parse(certificate.expiresAt) <= Date.now();
          return <article className="certificate-card" key={certificate.id}>
            <header><strong>{certificate.label}</strong><Badge tone={expired ? 'amber' : certificate.enabled ? 'green' : 'neutral'}>{expired ? 'Срок завершён' : certificate.enabled ? 'Активен' : 'Отключён'}</Badge></header>
            <p>{certificate.subject}</p><dl><dt>Действует до</dt><dd>{fullDateTime(certificate.expiresAt)}</dd><dt>SHA-256</dt><dd><code>{certificate.fingerprint}</code></dd></dl>
            {removing === certificate.id ? <div className="certificate-confirm"><p>Удаление сертификата закроет использующие его подключения. Для дальнейшего входа можно добавить новый сертификат.</p><div><button type="button" className="button button-secondary" disabled={busy} onClick={() => setRemoving(null)}>Отмена</button><button type="button" className="button button-danger" disabled={busy} onClick={() => void mutate(`${path}/${certificate.id}`, 'DELETE', undefined, 'Сертификат удалён.')} aria-label={`Подтвердить удаление ${certificate.label}`}>Удалить сертификат</button></div></div> : <div className="certificate-actions"><button type="button" className="button button-secondary" disabled={busy || expired} aria-label={`${certificate.enabled ? 'Отключить' : 'Включить'} сертификат ${certificate.label}`} onClick={() => void mutate(`${path}/${certificate.id}`, 'PATCH', { enabled: !certificate.enabled })}><Power size={14} />{certificate.enabled ? 'Отключить' : 'Включить'}</button><button type="button" className="icon-button" disabled={busy} aria-label={`Удалить сертификат ${certificate.label}`} onClick={() => setRemoving(certificate.id)}><Trash2 size={16} /></button></div>}
          </article>;
        })}</div> : <EmptyState icon={Fingerprint} title="Добавьте сертификат клиента" description="Для подключения используется сертификат, подписанный доверенным центром сертификации TLS-порта." />}
        {!adding && <button type="button" className="button button-secondary" disabled={busy || data.certificates.length >= 16} onClick={() => setAdding(true)}><Plus size={16} />Добавить сертификат</button>}
      </>}</ResourceView>
      {adding && <form className="form-stack certificate-form" onSubmit={(event) => { event.preventDefault(); void mutate(path, 'POST', { label, certificatePem: pem }, 'Сертификат добавлен.'); }}>
        <label className="field"><span>Название сертификата</span><input value={label} onChange={(event) => setLabel(event.target.value)} maxLength={80} required disabled={busy} placeholder="Например, контроллер в мастерской" /></label>
        <div><input className="sr-only" id={fileId} type="file" accept=".pem,.crt" disabled={busy} onChange={(event) => { void readFile(event.target.files?.[0]); event.target.value = ''; }} /><label className="button button-secondary" htmlFor={fileId}><Upload size={15} />Загрузить PEM</label></div>
        <label className="field"><span>Открытый сертификат PEM</span><textarea aria-label="Открытый сертификат PEM" className="mono" rows={5} value={pem} onChange={(event) => setPem(event.target.value)} required maxLength={12288} disabled={busy} placeholder={'-----BEGIN CERTIFICATE-----\n…\n-----END CERTIFICATE-----'} /></label>
        <div className="certificate-actions"><button className="button button-primary" type="submit" disabled={busy}>{busy && <LoaderCircle size={16} className="spin" />}Сохранить сертификат</button><button className="button button-secondary" type="button" disabled={busy} onClick={() => setAdding(false)}>Отмена добавления</button></div>
      </form>}
      {error && <ErrorNotice>{error}</ErrorNotice>}
    </div><footer className="modal-footer"><span className="field-hint">До 16 сертификатов на учётную запись</span><button className="button button-secondary" disabled={busy} onClick={onClose}>Закрыть</button></footer>
  </Modal>;
}
