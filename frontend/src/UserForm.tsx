import { useEffect, useState } from 'react';
import type { FormEvent } from 'react';
import { Eye, EyeOff, LoaderCircle, ShieldCheck } from 'lucide-react';
import { errorMessage } from './api';
import type { ApiClient } from './api';
import { ErrorNotice, Modal } from './components';
import type { AccessGroup, BrokerUser } from './types';
import { useResource } from './useResource';

export type UserFormMode = 'create' | 'edit' | 'password';

interface Props { mode: UserFormMode; user?: BrokerUser; api: ApiClient; onClose: () => void; onSaved: () => void }

function filters(value: string): string[] {
  return [...new Set(value.split('\n').map((line) => line.trim()).filter(Boolean))];
}

export function UserForm({ mode, user, api, onClose, onSaved }: Props) {
  const [username, setUsername] = useState(user?.username ?? '');
  const [password, setPassword] = useState('');
  const [showPassword, setShowPassword] = useState(false);
  const [publishFilters, setPublishFilters] = useState(user?.publishFilters.join('\n') ?? '');
  const [subscribeFilters, setSubscribeFilters] = useState(user?.subscribeFilters.join('\n') ?? '');
  const groups = useResource<AccessGroup[]>(api, '/groups');
  const [groupIds, setGroupIds] = useState<string[]>((user?.groups ?? []).map((group) => group.id));
  const [groupSearch, setGroupSearch] = useState('');
  useEffect(() => {
    if (groups.data) {
      const existing = new Set(groups.data.map((group) => group.id));
      setGroupIds((previous) => {
        const next = previous.filter((id) => existing.has(id));
        return next.length === previous.length ? previous : next;
      });
    }
  }, [groups.data]);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const title = mode === 'create' ? 'Новая учётная запись MQTT' : mode === 'edit' ? 'Права доступа' : 'Изменить пароль';
  const submit = async (event: FormEvent) => {
    event.preventDefault();
    if (mode !== 'edit' && new TextEncoder().encode(password).length > 256) { setError('Пароль слишком длинный. Используйте более короткую фразу.'); return; }
    setBusy(true); setError(null);
    try {
      const rules = { publishFilters: filters(publishFilters), subscribeFilters: filters(subscribeFilters), groupIds };
      if (mode === 'create') await api('/users', { method: 'POST', body: { username: username.trim(), password, ...rules } });
      else await api(`/users/${encodeURIComponent(user!.username)}`, { method: 'PATCH', body: mode === 'password' ? { password } : rules });
      onSaved();
    } catch (failure) { setError(errorMessage(failure)); }
    finally { setBusy(false); }
  };
  return <Modal title={title} subtitle={mode === 'create' ? 'Отдельная учётная запись для MQTT-подключений' : user?.username} onClose={() => { if (!busy) onClose(); }} wide={mode !== 'password'}>
    <form onSubmit={(event) => void submit(event)}>
      <div className="modal-body form-stack">
        {mode !== 'create' && <p className="field-hint">Методы входа: {user?.requireClientCertificate ? 'сертификат клиента (обязателен)' : 'логин и пароль'}{user?.scramReady && ' · SCRAM-SHA-256'}{!user?.requireClientCertificate && !!user?.certificateCount && ' · сертификат клиента'}.</p>}
        {mode === 'create' && <label className="field"><span>MQTT-логин</span><input aria-label="MQTT-логин" aria-describedby="mqtt-username-hint" value={username} onChange={(event) => setUsername(event.target.value)} autoComplete="off" autoCapitalize="none" spellCheck={false} required maxLength={128} pattern={'[A-Za-z0-9_.@\\-]+'} title="Латинские буквы, цифры и символы . _ @ -" disabled={busy} /><small id="mqtt-username-hint">Латинские буквы, цифры и символы . _ @ -</small></label>}
        {mode !== 'edit' && <label className="field"><span>{mode === 'create' ? 'Пароль' : 'Новый пароль'}</span><span className="password-input"><input aria-label={mode === 'create' ? 'Пароль' : 'Новый пароль'} aria-describedby="mqtt-password-hint" type={showPassword ? 'text' : 'password'} autoComplete="new-password" value={password} onChange={(event) => setPassword(event.target.value)} required minLength={12} maxLength={256} disabled={busy} /><button className="password-toggle" type="button" aria-label={showPassword ? 'Скрыть пароль' : 'Показать пароль'} onClick={() => setShowPassword(!showPassword)}>{showPassword ? <EyeOff size={17} /> : <Eye size={17} />}</button></span><small id="mqtt-password-hint">От 12 символов. Сохраните пароль перед закрытием формы.</small></label>}
        {mode !== 'password' && <>
          <div className="form-section-heading"><ShieldCheck size={17} /><div><h3>Группы доступа</h3><p>Выберите общие наборы прав. До 32 групп на учётную запись.</p></div></div>
          {groups.error && <ErrorNotice>{groups.error}</ErrorNotice>}
          {!!groups.data?.length && <label className="field"><span>Найти группу доступа</span><input value={groupSearch} onChange={(event) => setGroupSearch(event.target.value)} placeholder="Название группы" disabled={busy} /></label>}
          <div className="access-memberships">{groups.data?.length ? groups.data.filter((group) => group.name.toLowerCase().includes(groupSearch.toLowerCase())).map((group) => <label key={group.id} className="access-membership"><input type="checkbox" checked={groupIds.includes(group.id)} disabled={busy || (!groupIds.includes(group.id) && groupIds.length >= 32)} onChange={(event) => setGroupIds(event.target.checked ? [...groupIds, group.id] : groupIds.filter((id) => id !== group.id))} /><span><strong>{group.name}</strong><small>{group.enabled ? group.description || 'Правила группы активны' : 'Правила группы отключены'}</small></span></label>) : <p className="field-hint">{groups.loading ? 'Загружаем группы…' : 'Создайте общие наборы прав во вкладке «Группы».'}</p>}</div>
          {!!groups.data?.length && <p className="field-hint">Выбрано групп: {groupIds.length} из 32.</p>}
          <div className="form-section-heading"><ShieldCheck size={17} /><div><h3>Личные разрешения</h3><p>Укажите MQTT-фильтры по одному на строку. Личные разрешения дополняют правила активных групп; явные запреты имеют приоритет.</p></div></div>
          <div className="acl-fields"><label className="field"><span>Публикация</span><textarea aria-label="Публикация" aria-describedby="mqtt-publish-hint" className="mono" value={publishFilters} onChange={(event) => setPublishFilters(event.target.value)} placeholder={'devices/sensor-01/state\ndevices/sensor-01/events/#'} rows={5} spellCheck={false} disabled={busy} /><small id="mqtt-publish-hint">Куда клиент может отправлять сообщения</small></label><label className="field"><span>Подписка</span><textarea aria-label="Подписка" aria-describedby="mqtt-subscribe-hint" className="mono" value={subscribeFilters} onChange={(event) => setSubscribeFilters(event.target.value)} placeholder="devices/sensor-01/commands/#" rows={5} spellCheck={false} disabled={busy} /><small id="mqtt-subscribe-hint">На какие топики клиент может подписываться</small></label></div>
          <p className="field-hint wildcard-hint"><code>+</code> – один уровень топика. <code>#</code> – текущий и все следующие уровни.</p>
        </>}
        {mode !== 'create' && <div className="notice notice-info"><ShieldCheck size={17} /><span>После сохранения активные подключения этой учётной записи будут закрыты для повторной авторизации.</span></div>}
        {error && <ErrorNotice>{error}</ErrorNotice>}
      </div>
      <footer className="modal-footer"><button className="button button-secondary" type="button" disabled={busy} onClick={onClose}>Отмена</button><button className="button button-primary" type="submit" disabled={busy}>{busy && <LoaderCircle size={16} className="spin" />}{mode === 'create' ? 'Создать учётную запись' : mode === 'edit' ? 'Сохранить права' : 'Изменить пароль'}</button></footer>
    </form>
  </Modal>;
}
