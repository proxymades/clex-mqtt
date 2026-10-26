import { useState } from 'react';
import type { FormEvent } from 'react';
import { Pencil, UsersRound } from 'lucide-react';
import type { ApiClient } from './api';
import { errorMessage } from './api';
import type { Notify } from './App';
import { Badge, EmptyState, ErrorNotice, Modal, ResourceView } from './components';
import { useResource } from './useResource';
import { dateTime } from './format';
import type { AccountLimits, ManagedAccount } from './accountTypes';

const fields: { key: keyof AccountLimits; title: string; min: number; max: number }[] = [
  { key: 'maxMqttUsers', title: 'MQTT-учётные записи', min: 1, max: 100 },
  { key: 'maxConnections', title: 'Одновременные подключения', min: 1, max: 100 },
  { key: 'maxSessions', title: 'Сохранённые сессии', min: 1, max: 1000 },
  { key: 'maxQueuedMessages', title: 'Хранимые сообщения', min: 1, max: 100000 },
  { key: 'maxQueuedBytes', title: 'Хранилище, байт', min: 1024, max: 67108864 },
  { key: 'maxPublishRate', title: 'Публикации в секунду', min: 1, max: 1000 },
];
export function AccountsPage({ api, revision, notify, onChange }: { api: ApiClient; revision: number; notify: Notify; onChange: () => void }) {
  const resource = useResource<ManagedAccount[]>(api, '/accounts', revision);
  const [editing, setEditing] = useState<ManagedAccount | null>(null);
  return <>
    <p className="subtle-note">Вход пользователей: <a href="/account">/account</a>. Самостоятельная регистрация включается настройкой сервера.</p>
    <ResourceView resource={resource}>{(accounts) => accounts.length ? <section className="card"><div className="table-scroll"><table><thead><tr><th>Аккаунт</th><th>Состояние</th><th>MQTT-учётные записи</th><th>Подключения</th><th>Создан</th><th>Действия</th></tr></thead><tbody>{accounts.map((account) => <tr key={account.id}><td><strong>{account.username}</strong><small className="muted account-topic">{account.topicPrefix}</small></td><td><Badge tone={account.enabled ? 'green' : 'neutral'}>{account.enabled ? 'Активен' : 'Заблокирован'}</Badge></td><td>{account.mqttUsers} / {account.limits.maxMqttUsers}</td><td>{account.connections} / {account.limits.maxConnections}</td><td>{dateTime(account.createdAt)}</td><td><button className="icon-button" aria-label={'Настроить аккаунт ' + account.username} onClick={() => setEditing(account)}><Pencil size={17} /></button></td></tr>)}</tbody></table></div></section> : <EmptyState icon={UsersRound} title="Аккаунтов пока нет" description="После регистрации пользователя здесь появятся его аккаунт, ограничения и состояние." />}</ResourceView>
    {editing && <AccountForm key={editing.id} account={editing} api={api} onClose={() => setEditing(null)} onSaved={() => { setEditing(null); onChange(); notify('Аккаунт обновлён. Его MQTT-клиенты переподключатся для применения ограничений.'); }} />}
  </>;
}
function AccountForm({ account, api, onClose, onSaved }: { account: ManagedAccount; api: ApiClient; onClose: () => void; onSaved: () => void }) {
  const [limits, setLimits] = useState(account.limits); const [enabled, setEnabled] = useState(account.enabled);
  const [password, setPassword] = useState(''); const [busy, setBusy] = useState(false); const [error, setError] = useState<string | null>(null);
  const submit = async (event: FormEvent) => { event.preventDefault(); setBusy(true); setError(null); try { await api('/accounts/' + account.id, { method: 'PATCH', body: { enabled, limits, ...(password ? { password } : {}) } }); setPassword(''); onSaved(); } catch (e) { setError(errorMessage(e)); } finally { setBusy(false); } };
  return <Modal title={'Аккаунт ' + account.username} subtitle="Изменения ограничений применяются ко всем MQTT-учётным записям аккаунта." onClose={() => { if (!busy) onClose(); }}>
    <form onSubmit={(event) => void submit(event)}><div className="modal-body form-stack">
      <label className="checkbox-field"><input type="checkbox" checked={enabled} onChange={(e) => setEnabled(e.target.checked)} disabled={busy} />Аккаунт включён</label>
      {fields.map(({ key, title, min, max }) => <label className="field" key={key}><span>{title}</span><input type="number" min={key === 'maxSessions' ? limits.maxConnections : min} max={max} step={1} required value={limits[key]} onChange={(e) => setLimits({ ...limits, [key]: Number(e.target.value) })} disabled={busy} /></label>)}
      <p className="muted">Хранилище включает очереди, входящий QoS 2, Retain и Will. Уменьшение лимита сохраняет уже принятые данные и ограничивает новые записи.</p>
      <label className="field"><span>Сбросить пароль аккаунта</span><input type="password" autoComplete="new-password" minLength={12} maxLength={256} value={password} onChange={(e) => setPassword(e.target.value)} disabled={busy} /><small>Необязательно. Новый пароль завершит все сеансы аккаунта; передайте его пользователю безопасным способом.</small></label>
      <p className="muted">Блокировка завершает сеансы аккаунта и отключает его MQTT-клиентов. Учётные записи и данные сохраняются.</p>
      {error && <ErrorNotice>{error}</ErrorNotice>}
    </div><div className="modal-footer"><button type="button" className="button button-secondary" disabled={busy} onClick={onClose}>Отмена</button><button className="button button-primary" disabled={busy}>Сохранить</button></div></form>
  </Modal>;
}
