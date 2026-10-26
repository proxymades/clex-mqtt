import { useState } from 'react';
import { Fingerprint, KeyRound, LoaderCircle, Pencil, Plus, Power, ShieldCheck, UsersRound } from 'lucide-react';
import { errorMessage } from './api';
import type { ApiClient } from './api';
import type { Notify } from './App';
import { Badge, ConfirmDialog, EmptyState, ResourceView, SearchField } from './components';
import { dateTime, number } from './format';
import type { BrokerUser } from './types';
import { useResource } from './useResource';
import { UserForm } from './UserForm';
import { CertificatesModal } from './CertificatesModal';
import type { UserFormMode } from './UserForm';

interface Props { api: ApiClient; revision: number; notify: Notify; onChange: () => void; groupId?: string }

function Filters({ values }: { values: string[] }) {
  return values.length ? <div className="filter-chips">{values.slice(0, 2).map((value) => <code key={value} title={value}>{value}</code>)}{values.length > 2 && <span>+{values.length - 2}</span>}</div> : <span className="muted">–</span>;
}

export function UsersPage({ api, revision, notify, onChange, groupId = '' }: Props) {
  const resource = useResource<BrokerUser[]>(api, '/users', revision);
  const [search, setSearch] = useState('');
  const [dialog, setDialog] = useState<{ mode: UserFormMode; user?: BrokerUser } | null>(null);
  const [toggling, setToggling] = useState<BrokerUser | null>(null);
  const [certificatesFor, setCertificatesFor] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const toggle = async () => {
    if (!toggling) return;
    setBusy(true); setError(null);
    try { await api(`/users/${encodeURIComponent(toggling.username)}`, { method: 'PATCH', body: { enabled: !toggling.enabled } }); notify(toggling.enabled ? 'Учётная запись отключена.' : 'Учётная запись включена.'); setToggling(null); onChange(); }
    catch (failure) { setError(errorMessage(failure)); }
    finally { setBusy(false); }
  };
  return <>
    <div className="page-actions"><div className="subtle-note"><ShieldCheck size={17} /><span>Отдельные учётные записи для устройств и приложений</span></div><button className="button button-primary" onClick={() => setDialog({ mode: 'create' })}><Plus size={17} />Добавить учётную запись</button></div>
    <ResourceView resource={resource}>{(users) => {
      const filtered = users.filter((user) => [user.username, user.ownerUsername ?? '', user.accountLabel ?? ''].some((value) => value.toLowerCase().includes(search.trim().toLowerCase())) && (!groupId || user.groups?.some((group) => group.id === groupId)));
      const groups = [...new Map(users.flatMap((user) => user.groups ?? []).map((group) => [group.id, group])).values()].sort((a, b) => a.name.localeCompare(b.name));
      return <section className="card"><div className="table-toolbar"><div className="toolbar-title"><h2>Учётные записи MQTT</h2><span className="count-pill">{number(users.length)}</span></div><div className="user-table-filters"><select aria-label="Группа учётных записей" value={groupId} onChange={(event) => { window.location.hash = event.target.value ? `users?${new URLSearchParams({ groupId: event.target.value })}` : 'users'; }}><option value="">Все группы</option>{groupId && !groups.some((group) => group.id === groupId) && <option value={groupId}>Выбранная группа</option>}{groups.map((group) => <option key={group.id} value={group.id}>{group.name}</option>)}</select><SearchField value={search} onChange={setSearch} placeholder="Найти MQTT-логин" /></div></div>
        {filtered.length ? <div className="table-scroll"><table className="users-table"><thead><tr><th>MQTT-логин</th><th>Состояние</th><th>Группы</th><th>Публикации</th><th>Подписки</th><th><span className="sr-only">Действия</span></th></tr></thead><tbody>{filtered.map((user) => <tr key={user.username}><td><div className="client-identity"><span className="table-avatar"><UsersRound size={17} /></span><span><strong className="mono">{user.username}</strong><small>{user.ownerUsername ? 'Кабинет ' + user.ownerUsername + ' · ' + (user.accountLabel ?? '') : 'Создана ' + dateTime(user.createdAt)}</small></span></div></td><td><Badge tone={user.enabled && user.accountEnabled !== false ? 'green' : 'neutral'}>{user.accountEnabled === false ? 'Кабинет заблокирован' : user.enabled ? 'Включён' : 'Отключён'}</Badge></td><td><div className="filter-chips">{(user.groups ?? []).slice(0, 2).map((group) => <a key={group.id} href={`#users?${new URLSearchParams({ groupId: group.id })}`} title={group.enabled ? group.name : `${group.name} · правила отключены`}>{group.name}</a>)}{(user.groups?.length ?? 0) > 2 && <span>+{user.groups.length - 2}</span>}{!user.groups?.length && <span className="muted">–</span>}</div></td><td><Filters values={user.publishFilters} /></td><td><Filters values={user.subscribeFilters} /></td><td className="row-actions"><div className="action-group"><a className="icon-button" aria-label={`Проверить доступ ${user.username}`} title="Проверить доступ" href={`#users?tab=check&${new URLSearchParams({ username: user.username })}`}><ShieldCheck size={16} /></a><button className="icon-button" aria-label={`Изменить права ${user.username}`} title="Изменить права" onClick={() => setDialog({ mode: 'edit', user })}><Pencil size={16} /></button><button className="icon-button" aria-label={`Сертификаты ${user.username}`} title={`Сертификаты: ${user.certificateCount ?? 0}`} onClick={() => setCertificatesFor(user.username)}><Fingerprint size={16} /></button><button className="icon-button" aria-label={`Сменить пароль ${user.username}`} title="Сменить пароль" onClick={() => setDialog({ mode: 'password', user })}><KeyRound size={16} /></button><button className={`icon-button ${user.enabled ? '' : 'action-enable'}`} aria-label={`${user.enabled ? 'Отключить' : 'Включить'} ${user.username}`} title={user.enabled ? 'Отключить учётную запись' : 'Включить учётную запись'} onClick={() => { setError(null); setToggling(user); }}><Power size={16} /></button></div></td></tr>)}</tbody></table></div> : <EmptyState icon={UsersRound} title={users.length ? 'Уточните MQTT-логин' : 'Добавьте первую учётную запись'} description={users.length ? 'Измените MQTT-логин или выбранную группу.' : 'Создайте отдельную учётную запись и укажите топики, доступные MQTT-клиенту.'} action={!users.length && <button className="button button-secondary" onClick={() => setDialog({ mode: 'create' })}><Plus size={16} />Добавить учётную запись</button>} />}
        <div className="table-footer"><span>Показано {number(filtered.length)} из {number(users.length)} загруженных</span><span>До 1 000 учётных записей</span></div>
      </section>;
    }}</ResourceView>
    {dialog && <UserForm mode={dialog.mode} user={dialog.user} api={api} onClose={() => setDialog(null)} onSaved={() => { notify(dialog.mode === 'create' ? 'Учётная запись создана.' : dialog.mode === 'password' ? 'Пароль изменён.' : 'Права доступа сохранены.'); setDialog(null); onChange(); }} />}
    {certificatesFor && <CertificatesModal username={certificatesFor} api={api} revision={revision} notify={notify} onChange={onChange} onClose={() => setCertificatesFor(null)} />}
    {toggling && <ConfirmDialog title={toggling.enabled ? 'Отключить учётную запись?' : 'Включить учётную запись?'} description={toggling.enabled ? <>Доступ учётной записи <strong className="mono">{toggling.username}</strong> будет отключён. Её активные подключения будут закрыты.</> : <>Учётные данные <strong className="mono">{toggling.username}</strong> снова будут доступны для MQTT-подключений с заданными правами.</>} label={toggling.enabled ? 'Отключить' : 'Включить'} busy={busy} error={error} danger={toggling.enabled} onConfirm={() => void toggle()} onClose={() => setToggling(null)} />}
    {busy && <span className="sr-only" role="status"><LoaderCircle />Сохраняем изменения</span>}
  </>;
}
