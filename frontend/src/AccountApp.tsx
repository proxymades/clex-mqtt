import { useCallback, useEffect, useMemo, useState } from 'react';
import type { FormEvent } from 'react';
import { ChevronRight, Copy, KeyRound, LoaderCircle, LogOut, Menu, MessagesSquare, Network, Pencil, Plus, Power, RefreshCw, Settings2, Trash2 } from 'lucide-react';
import { ApiError, errorMessage, request } from './api';
import type { ApiClient, RequestOptions } from './api';
import { Badge, ConfirmDialog, EmptyState, ErrorNotice, Loading, Modal, ResourceView } from './components';
import { ClexMark } from './ClexMark';
import { bytes, dateTime } from './format';
import { useResource } from './useResource';
import { useAccountMessageStream } from './useAccountMessageStream';
import { MessageViewer } from './MessageViewer';
import { MqttTopicFilters } from './MqttTopicFilters';
import { RetainedPage } from './RetainedPage';
import type { AccountSession, PersonalMqttUser } from './accountTypes';

type Notify = (message: string, error?: boolean) => void;
export function AccountApp() {
  const [session, setSession] = useState<AccountSession | null>(null);
  const [checking, setChecking] = useState(true);
  const [notice, setNotice] = useState<{ text: string; error: boolean } | null>(null);
  const notify: Notify = useCallback((text, error = false) => setNotice({ text, error }), []);
  useEffect(() => {
    if (!notice) return;
    const timer = window.setTimeout(() => setNotice(null), notice.error ? 7000 : 4000);
    return () => window.clearTimeout(timer);
  }, [notice]);

  useEffect(() => {
    const controller = new AbortController();
    request<AccountSession>('/account/session', { signal: controller.signal }).then(setSession).catch((error: unknown) => {
      if (!controller.signal.aborted && !(error instanceof ApiError && error.status === 401)) notify(errorMessage(error), true);
    }).finally(() => { if (!controller.signal.aborted) setChecking(false); });
    return () => controller.abort();
  }, [notify]);
  const api = useMemo<ApiClient>(() => async <T,>(path: string, options: RequestOptions = {}) => {
    try { return await request<T>('/account' + path, { ...options, csrfToken: session?.csrfToken }); }
    catch (error) {
      if (error instanceof ApiError && error.status === 401) { setSession(null); notify('Сеанс завершён. Войдите снова.', true); }
      throw error;
    }
  }, [session, notify]);
  useEffect(() => { if (session) window.history.replaceState(null, '', '/account'); }, [session]);
  if (checking) return <div className="auth-shell"><Loading label="Проверяем сеанс" /></div>;
  return <>
    {session ? <AccountConsole session={session} api={api} notify={notify} onLogout={() => setSession(null)} />
      : <AccountLogin onLogin={(next) => { setSession(next); setNotice(null); }} />}
    {notice && <div className={'toast ' + (notice.error ? 'toast-error' : '')} role="status"><span>{notice.text}</span><button aria-label="Закрыть уведомление" onClick={() => setNotice(null)}>×</button></div>}
  </>;
}

function AccountLogin({ onLogin }: { onLogin: (session: AccountSession) => void }) {
  const [options, setOptions] = useState<{ registrationEnabled: boolean; csrfToken: string } | null>(null);
  const [requestedMode, setMode] = useState<'login' | 'register'>(() => window.location.pathname.replace(/\/+$/, '') === '/account/register' ? 'register' : 'login');
  const mode = options?.registrationEnabled ? requestedMode : 'login';
  const [username, setUsername] = useState(''); const [password, setPassword] = useState(''); const [confirm, setConfirm] = useState('');
  const [busy, setBusy] = useState(false); const [error, setError] = useState<string | null>(null);
  const reload = useCallback(() => {
    const controller = new AbortController();
    request<{ registrationEnabled: boolean; csrfToken: string }>('/account/options', { signal: controller.signal }).then(setOptions)
      .catch((failure: unknown) => { if (!controller.signal.aborted) setError(errorMessage(failure)); });
    return controller;
  }, []);
  useEffect(() => { const controller = reload(); return () => controller.abort(); }, [reload]);
  const submit = async (event: FormEvent) => {
    event.preventDefault(); setError(null);
    if (!options) return;
    if (mode === 'register' && password !== confirm) { setError('Пароли не совпадают.'); return; }
    setBusy(true);
    try { onLogin(await request<AccountSession>(mode === 'register' ? '/account/register' : '/account/session', { method: 'POST', csrfToken: options.csrfToken, body: { username, password } })); }
    catch (failure) { setError(errorMessage(failure)); } finally { setBusy(false); }
  };
  return <main className="auth-shell"><section className="auth-card">
    <div className="auth-brand"><span className="brand-mark"><ClexMark size={43} /></span><span>CLEX <strong>MQTT</strong></span></div>
    <div className="auth-heading">{mode === 'login' && <p className="eyebrow">Личный кабинет</p>}<h1>{mode === 'register' ? 'Создать аккаунт' : 'Ваши устройства на связи'}</h1><p>Управляйте своими MQTT-учётными записями, подключениями и сообщениями.</p></div>
    <form onSubmit={(event) => void submit(event)}>
      <label className="field"><span>Логин</span><input autoComplete="username" autoCapitalize="none" spellCheck={false} required minLength={3} maxLength={32} pattern="[a-zA-Z0-9_-]+" value={username} onChange={(e) => setUsername(e.target.value)} disabled={busy} autoFocus /></label>
      <label className="field"><span>Пароль</span><input type="password" autoComplete={mode === 'register' ? 'new-password' : 'current-password'} minLength={mode === 'register' ? 12 : undefined} maxLength={256} value={password} onChange={(e) => setPassword(e.target.value)} required disabled={busy} /></label>
      {mode === 'register' && <label className="field"><span>Повторите пароль</span><input type="password" autoComplete="new-password" minLength={12} maxLength={256} value={confirm} onChange={(e) => setConfirm(e.target.value)} required disabled={busy} /></label>}
      {error && <ErrorNotice>{error}</ErrorNotice>}
      <button className="button button-primary auth-submit" disabled={busy || !options}>{busy ? <LoaderCircle size={18} className="spin" /> : mode === 'register' ? 'Создать аккаунт' : 'Войти в кабинет'}</button>
    </form>
    <div className="account-auth-links">
      {options?.registrationEnabled && <button type="button" className="text-button" disabled={busy} onClick={() => { setMode(mode === 'login' ? 'register' : 'login'); window.history.replaceState(null, '', mode === 'login' ? '/account/register' : '/account'); setError(null); setPassword(''); setConfirm(''); }}>{mode === 'login' ? 'Создать аккаунт' : 'Уже есть аккаунт? Войти'}</button>}
      {!options && <button type="button" className="text-button" onClick={() => { setError(null); reload(); }}>Повторить подключение</button>}
      {options && !options.registrationEnabled && <p className="muted">Самостоятельная регистрация отключена.</p>}
      <p className="muted">Для восстановления доступа обратитесь к администратору сервера.</p><a href="/">На главную</a>
    </div>
  </section></main>;
}

type Page = 'users' | 'clients' | 'messages' | 'settings';
const accountPages = [
  { id: 'users', title: 'MQTT-доступ', description: 'Учётные записи и разрешения для подключения устройств и приложений.', icon: KeyRound },
  { id: 'clients', title: 'Подключения', description: 'Активные подключения устройств и сохранённые MQTT-сессии.', icon: Network },
  { id: 'messages', title: 'Сообщения', description: 'Публикации и значения Retain в вашем пространстве.', icon: MessagesSquare },
  { id: 'settings', title: 'Настройки', description: 'Пароль и безопасность вашего аккаунта.', icon: Settings2 },
] as const;
function AccountConsole({ session, api, notify, onLogout }: { session: AccountSession; api: ApiClient; notify: Notify; onLogout: () => void }) {
  const [page, setPage] = useState<Page>('users'); const [revision, setRevision] = useState(0); const [busy, setBusy] = useState(false);
  const [mobileOpen, setMobileOpen] = useState(false);
  const profile = useResource<AccountSession>(api, '/session', revision); const data = profile.data ?? session;
  const activePage = accountPages.find(({ id }) => id === page)!;
  const navigate = (next: Page) => { setPage(next); setMobileOpen(false); };
  const refresh = () => setRevision((v) => v + 1);
  const logout = async () => { setBusy(true); try { await api('/session', { method: 'DELETE' }); onLogout(); } catch (e) { notify(errorMessage(e), true); } finally { setBusy(false); } };
  const copy = async (text: string) => { try { await navigator.clipboard.writeText(text); notify('Скопировано.'); } catch { notify('Не удалось скопировать. Выделите значение вручную.', true); } };
  useEffect(() => {
    if (!mobileOpen) return;
    const close = (event: KeyboardEvent) => {
      if (event.key === 'Escape') { setMobileOpen(false); document.getElementById('account-menu-toggle')?.focus(); }
    };
    document.addEventListener('keydown', close);
    return () => document.removeEventListener('keydown', close);
  }, [mobileOpen]);
  return <div className="console-shell account-shell">
    {mobileOpen && <button className="sidebar-scrim" aria-label="Закрыть меню" onClick={() => setMobileOpen(false)} />}
    <aside id="account-sidebar" className={'sidebar account-sidebar ' + (mobileOpen ? 'sidebar-open' : '')}>
      <a className="brand" href="/account" aria-label="CLEX MQTT – MQTT-доступ" onClick={(event) => { if (event.button !== 0 || event.metaKey || event.ctrlKey || event.shiftKey || event.altKey) return; event.preventDefault(); navigate('users'); }}><span className="brand-mark"><ClexMark size={39} /></span><span>CLEX <strong>MQTT</strong><small>Личный кабинет</small></span></a>
      <nav aria-label="Разделы кабинета"><div className="nav-group"><p className="nav-label">Моё пространство</p>{accountPages.map(({ id, title, icon: Icon }) => <button key={id} className={'nav-link ' + (page === id ? 'active' : '')} aria-current={page === id ? 'page' : undefined} onClick={() => navigate(id)}><Icon size={19} strokeWidth={1.7} /><span>{title}</span>{page === id && <ChevronRight className="nav-chevron" size={14} />}</button>)}</div></nav>
      <div className="sidebar-bottom"><div className="account"><span className="avatar">{data.username.slice(0, 1).toUpperCase()}</span><div><strong title={data.username}>{data.username}</strong><small>Пользователь</small></div><button className="icon-button" title="Выйти" aria-label="Выйти из кабинета" onClick={() => void logout()} disabled={busy}>{busy ? <LoaderCircle size={17} className="spin" /> : <LogOut size={17} />}</button></div></div>
    </aside>
    <div className="main-shell">
      <header className="topbar"><div className="breadcrumb"><button id="account-menu-toggle" className="icon-button mobile-menu" aria-label="Открыть меню" aria-expanded={mobileOpen} aria-controls="account-sidebar" onClick={() => setMobileOpen(true)}><Menu size={21} /></button><span>CLEX MQTT</span><ChevronRight size={13} /><strong>{activePage.title}</strong></div><div className="topbar-actions"><button className="icon-button refresh-button" onClick={refresh} aria-label="Обновить данные" title="Загрузить актуальные данные раздела"><RefreshCw size={17} className={profile.refreshing ? 'spin' : ''} /></button></div></header>
      <main className="main-content account-main" id="main-content"><div className="page-heading"><div><p className="eyebrow">Личный кабинет</p><h1>{activePage.title}</h1><p>{activePage.description}</p></div></div>
        {profile.error && <ErrorNotice>{profile.error}</ErrorNotice>}
        {page === 'users' && <>
          <div className="account-usage" aria-label="Использование лимитов">
            <div><span>MQTT-учётные записи</span><strong>{data.usage.mqttUsers} / {data.limits.maxMqttUsers}</strong></div>
            <div><span>Подключения</span><strong>{data.usage.connections} / {data.limits.maxConnections}</strong></div>
            <div><span>Сохранённые сессии</span><strong>{data.usage.storedSessions} / {data.limits.maxSessions}</strong></div>
            <div><span>Хранимые сообщения</span><strong>{data.usage.storedMessages} / {data.limits.maxQueuedMessages}</strong></div>
            <div><span>Хранилище</span><strong>{bytes(data.usage.storedBytes)} / {bytes(data.limits.maxQueuedBytes)}</strong></div>
            <div><span>Публикации</span><strong>до {data.limits.maxPublishRate} / с</strong></div>
          </div>
          <ConnectionDetails data={data} copy={copy} />
          <MqttUsers api={api} data={data} revision={revision} refresh={refresh} notify={notify} copy={copy} />
        </>}
        {page === 'clients' && <Connections api={api} revision={revision} refresh={refresh} notify={notify} />}
        {page === 'messages' && <Messages api={api} revision={revision} refresh={refresh} notify={notify} />}
        {page === 'settings' && <PasswordSettings api={api} notify={notify} onLogout={onLogout} />}
        <p className="account-footer">Лимиты задаёт администратор сервера. Хранилище учитывает очереди доставки, входящий QoS 2, Retain и Will.</p>
      </main>
    </div>
  </div>;
}

function ConnectionValue({ label, value, copyLabel, copy, emptyLabel = 'Уточните у администратора' }: { label: string; value: string | null; copyLabel: string; copy: (text: string) => Promise<void>; emptyLabel?: string }) {
  return <div className="account-connection-value"><span>{label}</span><div className="account-copy"><code>{value ?? emptyLabel}</code>{value !== null && <button type="button" className="icon-button" aria-label={copyLabel} title={copyLabel} onClick={() => void copy(value)}><Copy size={17} /></button>}</div></div>;
}

function ConnectionDetails({ data, copy }: { data: AccountSession; copy: (text: string) => Promise<void> }) {
  return <section className="card account-connection">
    <h2>Подключение к брокеру</h2>
    <div className="account-connection-grid">
      <ConnectionValue label="Адрес брокера" value={data.connection.host} copyLabel="Скопировать адрес брокера" copy={copy} />
      <ConnectionValue label="Порт MQTT TLS" value={data.connection.tlsPort === null ? null : String(data.connection.tlsPort)} copyLabel="Скопировать порт MQTT TLS" copy={copy} />
    </div>
    <div className="account-connection-section">
      <h3>Ваши топики</h3>
      <div className="account-topic-values">
        <ConnectionValue label="Префикс топиков" value={data.topicPrefix} copyLabel="Скопировать префикс топиков" copy={copy} />
        <ConnectionValue label="Пример полного топика" value={data.topicPrefix + 'esp32/ask'} copyLabel="Скопировать пример топика" copy={copy} />
      </div>
      <p className="muted account-connection-note">Все ваши топики начинаются с указанного префикса. Используйте полный топик в настройках устройства и в разрешениях MQTT-учётной записи.</p>
    </div>
    <div className="account-connection-section">
      <h3>Данные MQTT-клиента</h3>
      <dl className="account-connection-help">
        <div><dt>MQTT-логин</dt><dd>Скопируйте MQTT-логин нужной учётной записи из таблицы ниже, например <code>{data.username}.home</code>.</dd></div>
        <div><dt>Пароль</dt><dd>Укажите пароль этой MQTT-учётной записи. Пароль аккаунта используется для входа в кабинет.</dd></div>
        <div><dt>Client ID</dt><dd>Укажите Client ID в настройках устройства или приложения, например <code>esp32-01</code>. Для каждого одновременного подключения под одним MQTT-логином используйте свой постоянный Client ID.</dd></div>
      </dl>
    </div>
  </section>;
}

function MqttUsers({ api, data, revision, refresh, notify, copy }: { api: ApiClient; data: AccountSession; revision: number; refresh: () => void; notify: Notify; copy: (text: string) => Promise<void> }) {
  const resource = useResource<PersonalMqttUser[]>(api, '/users', revision);
  const [form, setForm] = useState<{ user?: PersonalMqttUser } | null>(null); const [deleting, setDeleting] = useState<PersonalMqttUser | null>(null);
  const [busy, setBusy] = useState(false); const [error, setError] = useState<string | null>(null);
  const remove = async () => { if (!deleting) return; setBusy(true); setError(null); try { await api('/users/' + deleting.id, { method: 'DELETE' }); setDeleting(null); refresh(); notify('MQTT-учётная запись удалена.'); } catch (e) { setError(errorMessage(e)); } finally { setBusy(false); } };
  const toggle = async (user: PersonalMqttUser) => { setBusy(true); try { await api('/users/' + user.id, { method: 'PATCH', body: { enabled: !user.enabled } }); refresh(); notify('Состояние учётной записи изменено.'); } catch (e) { notify(errorMessage(e), true); } finally { setBusy(false); } };
  return <>
    <div className="page-actions"><p className="muted">Одну учётную запись могут использовать несколько устройств или приложений. Для раздельного управления доступом создавайте отдельные записи.</p><button className="button button-primary" disabled={data.usage.mqttUsers >= data.limits.maxMqttUsers} onClick={() => setForm({})}><Plus size={16} />Добавить учётную запись</button></div>
    <ResourceView resource={resource}>{(users) => users.length ? <section className="card"><div className="table-scroll"><table className="account-mqtt-users-table"><thead><tr><th>Учётная запись</th><th>MQTT-логин</th><th>Состояние</th><th>Публикации</th><th>Подписки</th><th>Действия</th></tr></thead><tbody>{users.map((user) => <tr key={user.id}><td><strong>{user.label}</strong><small className="muted">{dateTime(user.createdAt)}</small></td><td><div className="account-copy"><code className="mqtt-login">{user.username}</code><button className="icon-button" aria-label={'Скопировать MQTT-логин ' + user.label} onClick={() => void copy(user.username)}><Copy size={15} /></button></div></td><td><Badge tone={user.enabled ? 'green' : 'neutral'}>{user.enabled ? 'Включено' : 'Отключено'}</Badge></td><td><MqttTopicFilters values={user.publishFilters} kind="публикации" label={user.label} copy={copy} /></td><td><MqttTopicFilters values={user.subscribeFilters} kind="подписки" label={user.label} copy={copy} /></td><td className="row-actions"><div className="action-group"><button className="icon-button" title="Редактировать учётную запись" aria-label={'Изменить учётную запись ' + user.label} disabled={busy} onClick={() => setForm({ user })}><Pencil size={16} /></button><button className={'icon-button ' + (user.enabled ? '' : 'action-enable')} title={user.enabled ? 'Отключить учётную запись' : 'Включить учётную запись'} aria-label={(user.enabled ? 'Отключить учётную запись ' : 'Включить учётную запись ') + user.label} disabled={busy} onClick={() => void toggle(user)}><Power size={16} /></button><button className="icon-button" title="Удалить учётную запись" aria-label={'Удалить учётную запись ' + user.label} disabled={busy} onClick={() => { setDeleting(user); setError(null); }}><Trash2 size={16} /></button></div></td></tr>)}</tbody></table></div></section>
      : <EmptyState icon={KeyRound} title="Учётных записей пока нет" description="Создайте MQTT-логин и пароль для подключения устройств или приложений." />}</ResourceView>
    {form && <MqttUserForm api={api} accountName={data.username} copy={copy} prefix={data.topicPrefix} user={form.user} onClose={() => setForm(null)} onSaved={() => { setForm(null); refresh(); notify('MQTT-учётная запись сохранена.'); }} />}
    {deleting && <ConfirmDialog title="Удалить MQTT-учётную запись?" description={<>Устройства и приложения, использующие запись «{deleting.label}», потеряют доступ. Её подключения, сохранённые сессии и очереди будут удалены. Сообщения Retain останутся в вашем пространстве.</>} label="Удалить" busy={busy} error={error} onConfirm={() => void remove()} onClose={() => { if (!busy) setDeleting(null); }} />}
  </>;
}
function MqttUserForm({ api, accountName, prefix, user, copy, onClose, onSaved }: { api: ApiClient; accountName: string; prefix: string; user?: PersonalMqttUser; copy: (text: string) => Promise<void>; onClose: () => void; onSaved: () => void }) {
  const [label, setLabel] = useState(user?.label ?? ''); const [password, setPassword] = useState('');
  const [publish, setPublish] = useState(user ? user.publishFilters.join('\n') : prefix + '#');
  const [subscribe, setSubscribe] = useState(user ? user.subscribeFilters.join('\n') : prefix + '#');
  const [busy, setBusy] = useState(false); const [error, setError] = useState<string | null>(null);
  const submit = async (event: FormEvent) => {
    event.preventDefault(); setError(null); const split = (value: string) => value.split('\n').filter((v) => v.length > 0);
    const publishFilters = split(publish), subscribeFilters = split(subscribe);
    if ([...publishFilters, ...subscribeFilters].some((filter) => !filter.startsWith(prefix))) { setError('Начните каждый MQTT-фильтр с ' + prefix); return; }
    setBusy(true);
    const body = { ...(user ? {} : { label }), ...(password ? { password } : {}), publishFilters, subscribeFilters };
    try { await api(user ? '/users/' + user.id : '/users', { method: user ? 'PATCH' : 'POST', body }); setPassword(''); onSaved(); }
    catch (e) { setError(errorMessage(e)); } finally { setBusy(false); }
  };
  return <Modal title={user ? 'Учётная запись ' + user.label : 'Добавить учётную запись'} subtitle="MQTT-логин и пароль для одного или нескольких устройств и приложений." onClose={() => { if (!busy) onClose(); }}>
    <form onSubmit={(event) => void submit(event)}><div className="modal-body form-stack">
      <label className="field"><span>Название учётной записи</span><input value={label} onChange={(e) => setLabel(e.target.value)} required maxLength={24} pattern="[a-zA-Z0-9_-]+" disabled={busy || !!user} placeholder="home" aria-label="Название учётной записи" /><small>Название закрепляется за учётной записью при создании.</small></label>
      <ConnectionValue label="MQTT-логин" value={user?.username ?? (/^[a-zA-Z0-9_-]{1,24}$/.test(label) ? accountName + '.' + label : null)} emptyLabel="Введите название учётной записи" copyLabel="Скопировать MQTT-логин" copy={copy} />
      <label className="field"><span>{user ? 'Новый MQTT-пароль' : 'MQTT-пароль'}</span><input type="password" autoComplete="new-password" value={password} onChange={(e) => setPassword(e.target.value)} required={!user} minLength={12} maxLength={256} disabled={busy} /><small>{user ? 'Оставьте пустым, чтобы сохранить текущий пароль.' : 'От 12 символов.'}</small></label>
      <p className="muted">Укажите полные MQTT-фильтры, по одному на строку. Например: <code>{prefix}esp32/ask</code>. Фильтр <code>{prefix}#</code> разрешает все ваши топики. Пустое поле запрещает действие.</p>
      <label className="field"><span>Публикация</span><textarea value={publish} onChange={(e) => setPublish(e.target.value)} rows={3} disabled={busy} /></label>
      <label className="field"><span>Подписка</span><textarea value={subscribe} onChange={(e) => setSubscribe(e.target.value)} rows={3} disabled={busy} /></label>
      {user && <p className="muted">После сохранения активные клиенты этой учётной записи будут переподключены для применения прав.</p>}
      {error && <ErrorNotice>{error}</ErrorNotice>}
    </div><div className="modal-footer"><button type="button" className="button button-secondary" disabled={busy} onClick={onClose}>Отмена</button><button className="button button-primary" disabled={busy}>{busy && <LoaderCircle size={16} className="spin" />}{user ? 'Сохранить' : 'Создать учётную запись'}</button></div></form>
  </Modal>;
}

interface Connection { sessionId: string; username: string; clientId: string; connected: boolean; protocol: number; lastSeen: string; subscriptions: number; queued: number }
function Connections({ api, revision, refresh, notify }: { api: ApiClient; revision: number; refresh: () => void; notify: Notify }) {
  const resource = useResource<Connection[]>(api, '/clients', revision, 2000); const [busy, setBusy] = useState<string | null>(null);
  const disconnect = async (connection: Connection) => { setBusy(connection.sessionId); try { await api('/clients/disconnect', { method: 'POST', body: { username: connection.username, clientId: connection.clientId } }); refresh(); notify('Запрошено отключение клиента.'); } catch (e) { notify(errorMessage(e), true); } finally { setBusy(null); } };
  return <ResourceView resource={resource}>{(connections) => connections.length ? <section className="card"><div className="table-scroll"><table><thead><tr><th>Client ID</th><th>MQTT-логин</th><th>Состояние</th><th>Подписки / очередь</th><th>Действия</th></tr></thead><tbody>{connections.map((c) => <tr key={c.sessionId}><td><strong>{c.clientId}</strong><small className="muted">{dateTime(c.lastSeen)}</small></td><td><code>{c.username}</code></td><td><Badge tone={c.connected ? 'green' : 'neutral'}>{c.connected ? 'Подключено' : 'Сохранённая сессия'}</Badge></td><td>{c.subscriptions} / {c.queued}</td><td className="row-actions">{c.connected && <button type="button" className="icon-button" title="Отключить клиента" aria-label={'Отключить клиента ' + c.clientId} disabled={busy === c.sessionId} onClick={() => void disconnect(c)}><Power size={16} /></button>}</td></tr>)}</tbody></table></div></section> : <EmptyState icon={Network} title="Подключений пока нет" description="После подключения устройства или приложения здесь появится его MQTT-сессия." />}</ResourceView>;
}
function Messages({ api, revision, refresh, notify }: { api: ApiClient; revision: number; refresh: () => void; notify: Notify }) {
  const [tab, setTab] = useState<'stream' | 'retain'>('stream');
  const [paused, setPaused] = useState(false);
  const stream = useAccountMessageStream(api, revision, paused || tab !== 'stream');
  return <>
    <div className="account-tabs"><button className={'button ' + (tab === 'stream' ? 'button-primary' : 'button-secondary')} onClick={() => setTab('stream')}>Поток</button><button className={'button ' + (tab === 'retain' ? 'button-primary' : 'button-secondary')} onClick={() => setTab('retain')}>Retain</button></div>
    {tab === 'stream' ? <MessageViewer stream={stream} paused={paused} onPausedChange={setPaused} notify={notify} windowLimit={100} bufferNote="В потоке – до 100 последних публикаций вашего аккаунта, предпросмотр содержимого – до 4 КБ. Пауза останавливает обновление, очистка убирает сообщения из списка на экране. Буфер обновляется по мере обмена и при перезапуске брокера." /> : <RetainedPage api={api} revision={revision} notify={notify} onChange={refresh} />}
  </>;
}
function PasswordSettings({ api, notify, onLogout }: { api: ApiClient; notify: Notify; onLogout: () => void }) {
  const [current, setCurrent] = useState(''); const [next, setNext] = useState(''); const [confirm, setConfirm] = useState('');
  const [busy, setBusy] = useState(false); const [error, setError] = useState<string | null>(null);
  const submit = async (event: FormEvent) => { event.preventDefault(); setError(null); if (next !== confirm) { setError('Пароли не совпадают.'); return; } setBusy(true); try { await api('/password', { method: 'POST', body: { currentPassword: current, newPassword: next } }); setCurrent(''); setNext(''); setConfirm(''); notify('Пароль изменён. Войдите снова.'); onLogout(); } catch (e) { setError(errorMessage(e)); } finally { setBusy(false); } };
  return <section className="card account-password"><h2><KeyRound size={18} />Пароль аккаунта</h2><p className="muted">Смена пароля завершит все сеансы аккаунта. Пароли MQTT-учётных записей сохранятся.</p><form onSubmit={(event) => void submit(event)}><label className="field"><span>Текущий пароль</span><input type="password" autoComplete="current-password" maxLength={256} value={current} onChange={(e) => setCurrent(e.target.value)} required disabled={busy} /></label><label className="field"><span>Новый пароль</span><input type="password" autoComplete="new-password" minLength={12} maxLength={256} value={next} onChange={(e) => setNext(e.target.value)} required disabled={busy} /></label><label className="field"><span>Повторите новый пароль</span><input type="password" autoComplete="new-password" minLength={12} maxLength={256} value={confirm} onChange={(e) => setConfirm(e.target.value)} required disabled={busy} /></label>{error && <ErrorNotice>{error}</ErrorNotice>}<button className="button button-primary" disabled={busy}>Изменить пароль</button></form></section>;
}
