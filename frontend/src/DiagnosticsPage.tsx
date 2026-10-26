import { useEffect, useState } from 'react';
import { Activity, ChevronRight, Play, Square } from 'lucide-react';
import { errorMessage, type ApiClient } from './api';
import type { Notify } from './App';
import { Badge, DetailRow, EmptyState, ErrorNotice, Modal } from './components';
import { bytes, fullDateTime, number } from './format';
import { useResource } from './useResource';

interface Filter { clientId: string; username: string; topic: string; seconds: number }
interface Summary { id: string; receivedAt: string; clientId: string; username: string; topic: string; qos: number; status: string; queued: number; truncated: number }
interface TraceEvent { at: string; elapsedMs: number; code: string; deliveryId: string | null; sessionId: string | null; clientId: string | null; username: string | null; packetId: number | null; detail: string | null }
interface Trace extends Summary { retain: boolean; payloadBytes: number; metadataTruncated: boolean; events: TraceEvent[] }
interface Page { instanceId: string; id: string | null; active: boolean; expiresAt: string | null; filter: Filter | null; capacity: number; eventCapacity: number; bytes: number; evicted: number; messages: Summary[] }
const labels: Record<string, string> = {
  received: 'Начало обработки публикации', session_locked: 'Сессия проверена · блокировка хранилища получена', will: 'Публикация LWT', subscription_match: 'Подписка совпала',
  no_local: 'Применён No Local', offline_qos0: 'QoS 0: получатель отключён', offline_queued: 'Ожидание подключения получателя',
  shared_acl_allowed: 'Права участника группы подтверждены', shared_acl_denied: 'Доступ участника группы закрыт',
  shared_quota: 'Очередь участника группы заполнена', quota_exceeded: 'Достигнута квота очереди',
  queue_prepared: 'Доставка подготовлена в транзакции', committed: 'Транзакция очередей зафиксирована',
  await_pubrel: 'QoS 2 сохранён · ожидание PUBREL издателя', pubrel_received: 'Получен PUBREL издателя',
  publish_acl_denied: 'Публикация отклонена по ACL', delivery_acl_denied: 'Доставка отклонена по актуальным ACL',
  acl_allowed: 'Права получателя проверены перед отправкой', no_recipients: 'Маршрутизация завершена: 0 получателей',
  retain_updated: 'Подготовлено обновление Retain', retain_deleted: 'Подготовлено удаление Retain',
  bridge_routing: 'Обработаны правила исходящих мостов', expired: 'Истёк срок жизни сообщения',
  receive_maximum: 'Ожидание свободного окна Receive Maximum', packet_too_large: 'Размер превышает лимит получателя',
  publish_sent: 'PUBLISH записан в соединение · ожидание ACK', publish_retried: 'Повторный PUBLISH с DUP=1',
  qos0_written: 'QoS 0 записан в соединение', write_failed: 'Ошибка записи · обмен прерван', connection_closed: 'Соединение получателя закрыто',
  puback: 'PUBACK получателя · QoS 1 завершён', pubrec: 'PUBREC получателя · фаза сохранена',
  pubrel_sent: 'PUBREL записан в соединение', pubcomp: 'PUBCOMP получателя · QoS 2 завершён',
  puback_rejected: 'PUBACK с кодом отказа', pubrec_rejected: 'PUBREC с кодом отказа', pubcomp_rejected: 'PUBCOMP с кодом отказа',
  interrupted: 'Обработка прервана', commit_unknown: 'Исход COMMIT требует сверки с БД',
};
const status = (value: string) => ({ processing: 'Обработка', committed: 'Зафиксировано', await_pubrel: 'Ожидание PUBREL', interrupted: 'Прервано', commit_unknown: 'Сверка COMMIT', publish_acl_denied: 'Отказ ACL' })[value] ?? value;

export function DiagnosticsPage({ api, revision, notify, filters }: { api: ApiClient; revision: number; notify: Notify; filters: Omit<Filter, 'seconds'> }) {
  const [draft, setDraft] = useState<Filter>({ ...filters, seconds: 60 });
  const [busy, setBusy] = useState(false);
  const [selected, setSelected] = useState<string | null>(null);
  const resource = useResource<Page>(api, '/diagnostics', revision, 2000);
  const page = resource.data;
  const activeFilter = page?.active ? page.filter : null;
  useEffect(() => { if (activeFilter) setDraft(activeFilter); }, [activeFilter]);
  useEffect(() => { if (page && selected && !page.messages.some((m) => m.id === selected)) setSelected(null); }, [page, selected]);
  const change = async (start: boolean) => {
    setBusy(true);
    try {
      if (start) await api('/diagnostics', { method: 'POST', body: draft });
      else if (page?.id) await api(`/diagnostics/${page.id}`, { method: 'DELETE' });
      await resource.refresh();
      setSelected(null);
      notify(start ? 'Диагностика запущена.' : 'Сбор событий завершён. Результаты доступны для просмотра.');
    } catch (failure) { notify(errorMessage(failure), true); }
    finally { setBusy(false); }
  };
  return <>
    <section className="card message-filter-card">
      <div className="diagnostics-intro"><div><h2>Диагностика сообщения</h2><p className="muted">Выберите источник или топик и запустите сбор. Затем отправьте публикацию, чтобы увидеть её путь до MQTT-подтверждений получателя.</p></div><Badge tone={page?.active ? 'green' : 'neutral'}>{page?.active ? 'Сбор событий' : 'По запросу'}</Badge></div>
      <form className="message-filters diagnostics-filters" onSubmit={(e) => { e.preventDefault(); void change(true); }}>
        <label>Client ID<input value={draft.clientId} disabled={busy || page?.active} onChange={(e) => setDraft({ ...draft, clientId: e.target.value })} maxLength={1024} placeholder="Источник публикаций" /></label>
        <label>MQTT-логин<input value={draft.username} disabled={busy || page?.active} onChange={(e) => setDraft({ ...draft, username: e.target.value })} maxLength={128} placeholder="Все учётные записи" /></label>
        <label>Топик или фильтр<input value={draft.topic} disabled={busy || page?.active} onChange={(e) => setDraft({ ...draft, topic: e.target.value })} maxLength={1024} placeholder="Например, building/+/status" /></label>
        <label className="field">Длительность<select aria-label="Длительность" value={draft.seconds} disabled={busy || page?.active} onChange={(e) => setDraft({ ...draft, seconds: Number(e.target.value) })}><option value={30}>30 секунд</option><option value={60}>1 минута</option><option value={180}>3 минуты</option><option value={300}>5 минут</option></select></label>
        {page?.active ? <button className="button button-secondary" type="button" disabled={busy} onClick={() => void change(false)}><Square size={14} />Завершить сбор</button> : <button className="button button-primary" type="submit" disabled={busy || ![draft.clientId, draft.username, draft.topic].some(Boolean)}><Play size={14} />Запустить</button>}
      </form>
      {page?.filter && <p className="field-hint diagnostics-scope">{page.active ? 'Сбор до' : 'Последний сеанс до'} {page.expiresAt ? new Date(page.expiresAt).toLocaleTimeString('ru-RU') : '–'} · Источник: <strong>{page.filter.username || 'все учётные записи'} / {page.filter.clientId || 'все клиенты'}</strong> · Топик: <code>{page.filter.topic || '#'}</code></p>}
    </section>
    {resource.error && <ErrorNotice>{resource.error}</ErrorNotice>}
    {!!page?.evicted && <div className="notice notice-info">Буфер обновился: {number(page.evicted)} ранних записей вышли за его пределы. Уточните источник или топик для следующего сеанса.</div>}
    <section className="card">
      <div className="table-toolbar"><h2>Путь публикаций</h2><span className="muted">{number(page?.messages.length ?? 0)} / {page?.capacity ?? 100}</span></div>
      {page?.messages.length ? <div className="table-scroll"><table className="messages-table diagnostics-table"><thead><tr><th>Время / источник</th><th>Топик</th><th>QoS</th><th>Получатели</th><th>Транзакция</th><th><span className="sr-only">Подробнее</span></th></tr></thead><tbody>{page.messages.map((m) => <tr key={m.id}><td><strong className="mono">{m.clientId}</strong><small className="diagnostics-secondary">{m.username} · {new Date(m.receivedAt).toLocaleTimeString('ru-RU')}</small></td><td><button className="cell-link" onClick={() => setSelected(m.id)}><strong className="mono">{m.topic}</strong></button></td><td>{m.qos}</td><td className="tabular">{m.queued}</td><td><Badge tone={m.status === 'committed' ? 'green' : 'neutral'}>{status(m.status)}</Badge></td><td><button className="icon-button" aria-label={`Путь сообщения ${m.id}`} onClick={() => setSelected(m.id)}><ChevronRight size={18} /></button></td></tr>)}</tbody></table></div> : <EmptyState icon={Activity} title={page?.active ? 'Ожидаем выбранные публикации' : 'Запустите диагностику'} description="События соединятся в одну последовательность: приём, подписки, очередь, отправка и подтверждения QoS." />}
      <div className="table-footer"><span>Буфер: {bytes(page?.bytes ?? 0)} / 8 МБ</span><span>Обновление каждые 2 с</span></div>
    </section>
    <p className="field-hint message-buffer-note">Один общий сеанс для администраторов, до 100 публикаций и 128 событий на каждую. Сбор охватывает новые публикации MQTT-клиентов и LWT в выбранный период. Для QoS 0 фиксируется запись в соединение; для QoS 1/2 – подтверждения MQTT. При завершении сбора история остаётся до следующего запуска диагностики или перезапуска брокера. Содержимое сообщений хранится в разделе «Поток». Для мостов здесь показана граница локальной маршрутизации.</p>
    {selected && <TraceDetail key={`${page?.instanceId}:${selected}`} api={api} id={selected} onClose={() => setSelected(null)} />}
  </>;
}

function TraceDetail({ api, id, onClose }: { api: ApiClient; id: string; onClose: () => void }) {
  const resource = useResource<Trace>(api, `/diagnostics/messages/${id}`, 0, 2000);
  const trace = resource.data;
  return <Modal title="Путь сообщения" subtitle={trace?.topic ?? 'Загрузка событий'} onClose={onClose} wide>
    <div className="modal-body">{resource.error && <ErrorNotice>{resource.error}</ErrorNotice>}{trace && <>
      <dl className="detail-list detail-list-compact"><DetailRow label="Источник"><code>{trace.username} / {trace.clientId}</code></DetailRow><DetailRow label="Получено">{fullDateTime(trace.receivedAt)}</DetailRow><DetailRow label="Публикация">QoS {trace.qos} · {bytes(trace.payloadBytes)} · Retain {trace.retain ? 'включён' : 'выключен'}</DetailRow><DetailRow label="Внутренний ID"><code>{trace.id}</code></DetailRow></dl>
      {(trace.truncated > 0 || trace.metadataTruncated) && <div className="notice notice-info">Показан ограниченный фрагмент диагностики. Для более подробного просмотра выберите более узкий топик или источник.</div>}
      <ol className="trace-timeline">{trace.events.map((event, index) => <li key={index}><span className="trace-time tabular">+{event.elapsedMs.toLocaleString('ru-RU', { maximumFractionDigits: 2 })} мс</span><div><strong>{labels[event.code] ?? event.code}</strong>{event.clientId && <span className="diagnostics-secondary mono">{event.username} / {event.clientId}</span>}{event.detail && <span className="diagnostics-secondary">{event.detail}</span>}{(event.packetId || event.sessionId || event.deliveryId) && <small className="trace-identifiers mono">{event.packetId ? `Packet ID ${event.packetId} · ` : ''}{event.deliveryId ? `Доставка ${event.deliveryId.slice(0, 8)}` : event.sessionId ? `Сессия ${event.sessionId.slice(0, 8)}` : ''}</small>}</div></li>)}</ol>
    </>}</div><footer className="modal-footer"><button className="button button-secondary" onClick={onClose}>Закрыть</button></footer>
  </Modal>;
}
