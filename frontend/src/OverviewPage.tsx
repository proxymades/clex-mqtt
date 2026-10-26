import { ArrowDownLeft, ArrowUpRight, ChevronRight, Clock3, Database, Layers3, Network, Radio, Server } from 'lucide-react';
import type { LucideIcon } from 'lucide-react';
import { Badge, ResourceView } from './components';
import { duration, number } from './format';
import type { Overview, PageId } from './types';
import type { Resource } from './useResource';
import { ClexMark } from './ClexMark';

function Stat({ label, value, hint, icon: Icon, onClick }: { label: string; value: number; hint: string; icon: LucideIcon; onClick: () => void }) {
  return <button className="stat-card card" onClick={onClick}><span className="stat-top"><span>{label}</span><Icon size={19} strokeWidth={1.7} /></span><strong>{number(value)}</strong><span className="stat-bottom">{hint}<ChevronRight size={14} /></span></button>;
}

export function OverviewPage({ resource, navigate }: { resource: Resource<Overview>; navigate: (page: PageId) => void }) {
  return <ResourceView resource={resource}>{(data) => <>
    <section className="status-banner card">
      <span className={`status-orb ${data.database.healthy ? '' : 'status-orb-error'}`}><Radio size={28} strokeWidth={1.6} /></span>
      <div className="status-banner-copy"><p className="eyebrow">Состояние сервера</p><h2>{data.database.healthy ? 'База данных доступна' : 'База данных недоступна'}</h2><p>{data.listeners.filter((listener) => listener.enabled).length > 0 ? `Включено сетевых интерфейсов: ${number(data.listeners.filter((listener) => listener.enabled).length)}` : 'Включите сетевой интерфейс в конфигурации сервера.'}</p></div>
      <div className="uptime"><Clock3 size={17} /><div><span>Время работы</span><strong>{duration(data.uptimeSeconds)}</strong></div></div>
    </section>
    <section className="stats-grid" aria-label="Основные показатели">
      <Stat label="Подключено" value={data.activeConnections} hint="Активные клиенты" icon={Network} onClick={() => navigate('clients')} />
      <Stat label="Подписки" value={data.subscriptions} hint="В MQTT-сессиях" icon={Radio} onClick={() => navigate('clients')} />
      <Stat label="Сессии" value={data.storedSessions} hint="Сохранённые состояния" icon={Layers3} onClick={() => navigate('clients')} />
      <Stat label="Retain" value={data.retainedMessages} hint="По одному на топик" icon={Database} onClick={() => { window.location.hash = 'messages?tab=retain'; }} />
    </section>
    <div className="overview-columns">
      <section className="card traffic-card"><header className="card-header"><div><h2>Пакеты MQTT</h2><p>Счётчики с момента запуска брокера</p></div><span className="soft-icon"><Radio size={19} /></span></header>
        <div className="traffic-grid"><div><span className="traffic-icon incoming"><ArrowDownLeft size={22} /></span><p>Входящие пакеты</p><strong>{number(data.incomingMessages)}</strong><small>От MQTT-клиентов</small></div><div><span className="traffic-icon outgoing"><ArrowUpRight size={22} /></span><p>Исходящие пакеты</p><strong>{number(data.outgoingMessages)}</strong><small>MQTT-клиентам</small></div></div>
        <div className="metric-line"><span>В очередях доставки</span><strong>{number(data.queuedMessages)}</strong></div><div className="metric-line"><span>Отклонённые публикации</span><strong>{number(data.rejectedPublications ?? data.rejectedConnections)}</strong></div>
      </section>
      <section className="card"><header className="card-header"><div><h2>Сетевые интерфейсы</h2><p>Настроенные точки подключения</p></div><button className="text-button" onClick={() => navigate('settings')}>Все<ChevronRight size={14} /></button></header>
        <div className="listener-list">{data.listeners.length ? data.listeners.map((listener) => <div className="listener-item" key={`${listener.name}-${listener.address}`}><span className="listener-icon"><Server size={19} strokeWidth={1.6} /></span><div><strong>{listener.name}</strong><code>{listener.address}</code></div><Badge tone={listener.enabled ? 'green' : 'neutral'}>{listener.enabled ? 'Включён' : 'Выключен'}</Badge></div>) : <p className="inline-empty">Добавьте сетевые интерфейсы в конфигурацию сервера.</p>}</div>
        <div className="card-bottom-note"><Network size={16} /><span>Адреса и порты для подключения MQTT-клиентов и доступа к панели управления.</span></div>
      </section>
    </div>
  </>}</ResourceView>;
}

const capabilityStatus = { available: { label: 'Доступно', tone: 'green' }, partial: { label: 'Частично', tone: 'amber' }, planned: { label: 'Запланировано', tone: 'neutral' } } as const;

export function SettingsPage({ resource }: { resource: Resource<Overview> }) {
  return <ResourceView resource={resource}>{(data) => <>
    <section className="system-summary card"><div className="system-summary-brand"><span className="brand-mark"><ClexMark size={42} /></span><div><h2>CLEX MQTT</h2><p>Версия {data.version}</p></div></div><div><span>Время работы</span><strong>{duration(data.uptimeSeconds)}</strong></div><div><span>База данных</span><Badge tone={data.database.healthy ? 'green' : 'red'}>{data.database.healthy ? 'Доступна' : 'Недоступна'}</Badge></div></section>
    <section className="card"><header className="card-header"><div><h2>Сетевые интерфейсы</h2><p>Параметры, с которыми запущен сервер</p></div></header><div className="table-scroll"><table><thead><tr><th>Интерфейс</th><th>Адрес</th><th>Протокол</th><th>Состояние</th></tr></thead><tbody>{data.listeners.map((listener) => <tr key={`${listener.name}-${listener.address}`}><td className="cell-strong">{listener.name}</td><td><code>{listener.address}</code></td><td>{listener.protocol}</td><td><Badge tone={listener.enabled ? 'green' : 'neutral'}>{listener.enabled ? 'Включён' : 'Выключен'}</Badge></td></tr>)}</tbody></table></div>{!data.listeners.length && <p className="inline-empty">Добавьте сетевые интерфейсы в конфигурацию сервера.</p>}<div className="card-bottom-note"><Server size={16} /><span>Сетевые параметры задаются в конфигурации сервера и применяются при запуске.</span></div></section>
    {data.monitoring && <section className="card"><header className="card-header"><div><h2>Мониторинг</h2><p>Метрики сервера для Prometheus и Grafana</p></div><Badge tone={data.monitoring.enabled ? 'green' : 'neutral'}>{data.monitoring.enabled ? 'Включён' : 'Настройка'}</Badge></header><div className="monitoring-details"><div><span>Путь метрик</span><code>{data.monitoring.path}</code></div><div><span>Доступ</span><strong>Отдельный Bearer-токен</strong></div><div><span>Снимок состояния БД</span><strong>Каждые {data.monitoring.databaseSnapshotSeconds} с при запросе</strong></div></div><div className="card-bottom-note"><Database size={16} /><span>{data.monitoring.enabled ? 'Подключения, публикации, очереди и время операций PostgreSQL доступны системе мониторинга.' : 'Для включения задайте CLEX_METRICS_TOKEN в окружении сервера. Локальная подготовка: python3 scripts/metrics_token.py.'}</span></div></section>}
    <section className="card"><header className="card-header"><div><h2>Возможности версии</h2><p>Протоколы и функции этой версии</p></div></header><div className="capability-list">{data.capabilities.map((capability) => <div className="capability-item" key={capability.name}><div><h3>{capability.name}</h3><p>{capability.detail}</p></div><Badge tone={capabilityStatus[capability.status].tone}>{capabilityStatus[capability.status].label}</Badge></div>)}</div>{!data.capabilities.length && <p className="inline-empty">Обновите данные, чтобы загрузить список возможностей.</p>}</section>
  </>}</ResourceView>;
}
