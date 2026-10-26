import { useCallback, useEffect, useState } from 'react';
import { ChevronRight, MessagesSquare, LayoutDashboard, KeyRound, LoaderCircle, LogOut, Menu, Network, RefreshCw, ScrollText, Settings2, UsersRound } from 'lucide-react';
import type { LucideIcon } from 'lucide-react';
import { errorMessage } from './api';
import type { ApiClient } from './api';
import type { Notify } from './App';
import { useResource } from './useResource';
import type { Overview, PageId, Session } from './types';
import { OverviewPage, SettingsPage } from './OverviewPage';
import { ClientsPage } from './ClientsPage';
import { MessagesPage } from './MessagesPage';
import { AccountsPage } from './AccountsPage';
import { AccessPage } from './AccessPage';
import { AuditPage } from './AuditPage';
import { ClexMark } from './ClexMark';
import { BridgesPage } from './BridgesPage';

const pages: { id: PageId; title: string; description: string; icon: LucideIcon; group: string }[] = [
  { id: 'overview', title: 'Обзор', description: 'Подключения, сообщения и состояние сервера.', icon: LayoutDashboard, group: 'Брокер' },
  { id: 'clients', title: 'Подключения', description: 'Активные клиенты и сохранённые MQTT-сессии.', icon: Network, group: 'Брокер' },
  { id: 'messages', title: 'Сообщения', description: 'Публикации, значения Retain и диагностика пути доставки.', icon: MessagesSquare, group: 'Брокер' },
  { id: 'bridges', title: 'Мосты', description: 'Подключения к другим MQTT-брокерам и правила обмена топиками.', icon: Network, group: 'Брокер' },
  { id: 'users', title: 'MQTT-доступ', description: 'Учётные записи для подключения устройств и приложений к брокеру.', icon: KeyRound, group: 'Управление' },
  { id: 'accounts', title: 'Аккаунты', description: 'Пользовательские аккаунты, ограничения и доступ к серверу.', icon: UsersRound, group: 'Управление' },
  { id: 'audit', title: 'Журнал действий', description: 'Попытки входа и изменения, выполненные через панель управления.', icon: ScrollText, group: 'Управление' },
  { id: 'settings', title: 'Система', description: 'Сетевые интерфейсы и возможности этой версии.', icon: Settings2, group: 'Управление' },
];

function getPage(): PageId {
  const hash = window.location.hash.slice(1).split('?')[0];
  if (hash === 'retained') return 'messages';
  return pages.some((page) => page.id === hash) ? hash as PageId : 'overview';
}

export function Console({ session, api, notify, onLogout }: { session: Session; api: ApiClient; notify: Notify; onLogout: () => void }) {
  const [pageId, setPageId] = useState<PageId>(getPage);
  const [routeHash, setRouteHash] = useState(() => window.location.hash);
  const [mobileOpen, setMobileOpen] = useState(false);
  const [revision, setRevision] = useState(0);
  const [logoutBusy, setLogoutBusy] = useState(false);
  const overview = useResource<Overview>(api, '/overview', revision);
  const page = pages.find((entry) => entry.id === pageId)!;
  useEffect(() => {
    const onHashChange = () => { setPageId(getPage()); setRouteHash(window.location.hash); setMobileOpen(false); };
    window.addEventListener('hashchange', onHashChange);
    return () => window.removeEventListener('hashchange', onHashChange);
  }, []);
  const refresh = useCallback(() => setRevision((value) => value + 1), []);
  const logout = async () => {
    setLogoutBusy(true);
    try { await api('/session', { method: 'DELETE' }); onLogout(); }
    catch (error) { notify(errorMessage(error), true); }
    finally { setLogoutBusy(false); }
  };
  return <div className="console-shell">
    {mobileOpen && <button className="sidebar-scrim" aria-label="Закрыть меню" onClick={() => setMobileOpen(false)} />}
    <aside className={`sidebar ${mobileOpen ? 'sidebar-open' : ''}`}>
      <a className="brand" href="#overview" aria-label="CLEX MQTT – Обзор" onClick={() => setMobileOpen(false)}><span className="brand-mark"><ClexMark size={39} /></span><span>CLEX <strong>MQTT</strong><small>Панель управления</small></span></a>
      <nav aria-label="Основная навигация">{['Брокер', 'Управление'].map((group) => <div className="nav-group" key={group}><p className="nav-label">{group}</p>{pages.filter((entry) => entry.group === group).map(({ id, title, icon: Icon }) => <a key={id} href={`#${id}`} className={`nav-link ${pageId === id ? 'active' : ''}`} aria-current={pageId === id ? 'page' : undefined}><Icon size={19} strokeWidth={1.7} /><span>{title}</span>{pageId === id && <ChevronRight className="nav-chevron" size={14} />}</a>)}</div>)}</nav>
      <div className="sidebar-bottom">
        <div className="account"><span className="avatar">{session.username.slice(0, 1).toUpperCase()}</span><div><strong>{session.username}</strong><small>Администратор</small></div><button className="icon-button" title="Выйти" aria-label="Выйти из панели" disabled={logoutBusy} onClick={() => void logout()}>{logoutBusy ? <LoaderCircle size={17} className="spin" /> : <LogOut size={17} />}</button></div>
      </div>
    </aside>
    <div className="main-shell">
      <header className="topbar"><div className="breadcrumb"><button className="icon-button mobile-menu" aria-label="Открыть меню" onClick={() => setMobileOpen(true)}><Menu size={21} /></button><span>CLEX MQTT</span><ChevronRight size={13} /><strong>{page.title}</strong></div><div className="topbar-actions"><span className="live-label" title="Период загрузки данных с сервера"><span />Данные каждые {pageId === 'clients' || (pageId === 'messages' && routeHash !== '#retained' && !routeHash.includes('tab=retain')) ? 2 : 10} с</span><button className="icon-button refresh-button" onClick={refresh} aria-label="Обновить данные" title={pageId === 'clients' ? 'Загрузить список подключений с сервера' : 'Загрузить актуальные данные раздела с сервера'}><RefreshCw size={17} className={overview.refreshing ? 'spin' : ''} /></button></div></header>
      <main className="main-content" id="main-content">
        <div className="page-heading"><div><p className="eyebrow">{page.group}</p><h1>{page.title}</h1><p>{page.description}</p></div>{pageId === 'overview' && overview.data && <span className="version-pill">v{overview.data.version}</span>}</div>
        {pageId === 'overview' && <OverviewPage resource={overview} navigate={(id) => { window.location.hash = id; }} />}
        {pageId === 'clients' && <ClientsPage api={api} revision={revision} notify={notify} onChange={refresh} />}
        {pageId === 'messages' && <MessagesPage api={api} revision={revision} notify={notify} onChange={refresh} />}
        {pageId === 'users' && <AccessPage api={api} revision={revision} notify={notify} onChange={refresh} />}
        {pageId === 'bridges' && <BridgesPage api={api} revision={revision} notify={notify} onChange={refresh} />}
        {pageId === 'accounts' && <AccountsPage api={api} revision={revision} notify={notify} onChange={refresh} />}
        {pageId === 'audit' && <AuditPage api={api} revision={revision} />}
        {pageId === 'settings' && <SettingsPage resource={overview} />}
        <footer className="content-footer"><span>CLEX MQTT</span><span>{overview.updatedAt ? `Обзор обновлён в ${overview.updatedAt.toLocaleTimeString('ru-RU', { hour: '2-digit', minute: '2-digit', second: '2-digit' })}` : 'Панель управления брокером'}</span></footer>
      </main>
    </div>
  </div>;
}
