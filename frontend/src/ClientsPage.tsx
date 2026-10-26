import { useState } from 'react';
import { ChevronRight, Network, Power } from 'lucide-react';
import { errorMessage } from './api';
import type { ApiClient } from './api';
import type { Notify } from './App';
import { Badge, ConfirmDialog, DetailRow, EmptyState, Modal, ResourceView, SearchField } from './components';
import { dateTime, fullDateTime, number } from './format';
import type { Client } from './types';
import { useResource } from './useResource';

interface Props { api: ApiClient; revision: number; notify: Notify; onChange: () => void }

export function ClientsPage({ api, revision, notify, onChange }: Props) {
  const resource = useResource<Client[]>(api, '/clients', revision, 2000);
  const [search, setSearch] = useState('');
  const [filter, setFilter] = useState<'all' | 'connected' | 'stored'>('all');
  const [selectedId, setSelectedId] = useState<string | null>(null);
  const [disconnecting, setDisconnecting] = useState<Client | null>(null);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const selected = resource.data?.find((client) => client.sessionId === selectedId);
  const disconnect = async () => {
    if (!disconnecting) return;
    setBusy(true); setError(null);
    try {
      await api('/clients/disconnect', { method: 'POST', body: { clientId: disconnecting.clientId, username: disconnecting.username } });
      setDisconnecting(null); notify('Соединение клиента закрыто.'); onChange();
    } catch (failure) { setError(errorMessage(failure)); }
    finally { setBusy(false); }
  };
  return <>
    <ResourceView resource={resource}>{(clients) => {
      const term = search.trim().toLowerCase();
      const filtered = clients.filter((client) => (filter === 'all' || (filter === 'connected' ? client.connected : !client.connected)) && (!term || `${client.clientId} ${client.username} ${client.remoteAddress ?? ''}`.toLowerCase().includes(term)));
      const connected = clients.filter((client) => client.connected).length;
      return <section className="card"><div className="table-toolbar"><div className="segmented" aria-label="Состояние клиентов">{([{ id: 'all', label: 'Все', count: clients.length }, { id: 'connected', label: 'Подключены', count: connected }, { id: 'stored', label: 'Отключены', count: clients.length - connected }] as const).map((item) => <button key={item.id} aria-pressed={filter === item.id} className={filter === item.id ? 'selected' : ''} onClick={() => setFilter(item.id)}>{item.label}<span>{number(item.count)}</span></button>)}</div><SearchField value={search} onChange={setSearch} placeholder="Client ID, MQTT-логин или IP" /></div>
        {filtered.length ? <div className="table-scroll"><table className="clients-table"><thead><tr><th>Клиент</th><th>Состояние</th><th className="numeric">Подписки</th><th className="numeric">В очереди</th><th>Последняя активность</th><th><span className="sr-only">Действия</span></th></tr></thead><tbody>{filtered.map((client) => <tr key={client.sessionId}><td><button className="cell-link client-identity" onClick={() => setSelectedId(client.sessionId)}><span className={`table-avatar ${client.connected ? 'table-avatar-blue' : ''}`}><Network size={17} /></span><span><strong className="mono">{client.clientId}</strong><small>{client.username || 'Без MQTT-логина'}</small></span></button></td><td><Badge tone={client.connected ? 'green' : 'neutral'}>{client.connected ? 'Подключён' : 'Отключён'}</Badge></td><td className="numeric tabular">{number(client.subscriptions)}</td><td className="numeric tabular">{number(client.queued)}</td><td className="muted tabular">{dateTime(client.lastSeen)}</td><td className="row-actions"><button className="icon-button" title="Подробнее о клиенте" aria-label={`Подробнее о ${client.clientId}`} onClick={() => setSelectedId(client.sessionId)}><ChevronRight size={18} /></button></td></tr>)}</tbody></table></div> : <EmptyState icon={Network} title={clients.length ? 'Измените фильтр клиентов' : 'Подключите MQTT-клиент'} description={clients.length ? 'Укажите другой Client ID, MQTT-логин или IP либо выберите другое состояние.' : 'Укажите в MQTT-клиенте адрес брокера и учётные данные. После подключения здесь появится его сессия.'} />}
        <div className="table-footer"><span>Показано {number(filtered.length)} из {number(clients.length)} загруженных · до 1 000 клиентов</span><span>{resource.refreshing ? 'Загружаем список…' : resource.updatedAt ? `Список обновлён в ${resource.updatedAt.toLocaleTimeString('ru-RU')}` : 'Загружаем список'}</span></div>
        <div className="card-bottom-note"><Network size={16} /><span>Состояние соединений определяется по MQTT и Keep Alive. Интервал ожидания указан в карточке клиента.</span></div>
      </section>;
    }}</ResourceView>
    {selected && <Modal title="MQTT-клиент" subtitle={selected.clientId} onClose={() => setSelectedId(null)}>
      <div className="modal-body"><div className="client-detail-status"><span className="table-avatar table-avatar-blue"><Network size={22} /></span><Badge tone={selected.connected ? 'green' : 'neutral'}>{selected.connected ? 'Подключён' : 'Отключён'}</Badge></div><dl className="detail-list"><DetailRow label="Client ID"><code>{selected.clientId}</code></DetailRow><DetailRow label="MQTT-логин">{selected.username || 'Не задан'}</DetailRow><DetailRow label="Протокол">{selected.protocol}</DetailRow>{selected.certificateFingerprint && <DetailRow label="Сертификат клиента"><code className="break-word">{selected.certificateFingerprint}</code></DetailRow>}<DetailRow label="Keep Alive">{selected.keepAliveSeconds == null ? '–' : selected.keepAliveSeconds === 0 ? 'Выключен' : `${selected.keepAliveSeconds} с · ожидание пакета до ${selected.keepAliveSeconds * 1.5} с`}</DetailRow><DetailRow label="Адрес клиента"><code>{selected.remoteAddress || '–'}</code></DetailRow><DetailRow label="Подключён с">{fullDateTime(selected.connectedAt)}</DetailRow><DetailRow label="Последняя активность">{fullDateTime(selected.lastSeen)}</DetailRow><DetailRow label="Срок сессии">{selected.expiresAt ? fullDateTime(selected.expiresAt) : 'Не задан'}</DetailRow></dl><div className="detail-metrics"><div><strong>{number(selected.subscriptions)}</strong><span>Подписки</span></div><div><strong>{number(selected.queued)}</strong><span>В очереди</span></div><div><strong>{number(selected.inflight)}</strong><span>Inflight</span></div></div></div>
      <footer className="modal-footer"><button className="button button-secondary" onClick={() => setSelectedId(null)}>Закрыть</button><a className="button button-secondary" href={`#messages?${new URLSearchParams({ clientId: selected.clientId, username: selected.username })}`}>Сообщения клиента</a>{selected.connected && <button className="button button-danger-soft" onClick={() => { setError(null); setDisconnecting(selected); setSelectedId(null); }}><Power size={16} />Отключить клиента</button>}</footer>
    </Modal>}
    {disconnecting && <ConfirmDialog title="Отключить клиента?" description={<>Соединение <strong className="mono break-word">{disconnecting.clientId}</strong> с MQTT-логином <strong>{disconnecting.username}</strong> будет закрыто. Клиент сможет подключиться снова с действующими учётными данными.</>} label="Отключить" busy={busy} error={error} onConfirm={() => void disconnect()} onClose={() => setDisconnecting(null)} />}
  </>;
}
