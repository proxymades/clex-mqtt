import { useState } from 'react';
import type { FormEvent } from 'react';
import { ScrollText } from 'lucide-react';
import type { ApiClient } from './api';
import type { AuditEntry } from './types';
import { Badge, EmptyState, ResourceView } from './components';
import { dateTime, number } from './format';
import { useResource } from './useResource';

const actionLabels: Record<string, string> = {
  'client_certificate.create': 'Добавлен сертификат клиента', 'client_certificate.update': 'Изменён сертификат клиента', 'client_certificate.delete': 'Удалён сертификат клиента',
  'access_group.create': 'Создана группа доступа', 'access_group.update': 'Изменена группа доступа', 'access_group.delete': 'Удалена группа доступа',
  'bridge.create': 'Создан мост MQTT', 'bridge.update': 'Изменён мост MQTT', 'bridge.delete': 'Удалён мост MQTT',
  'user.create': 'Создана MQTT-учётная запись', 'user.update': 'Изменена MQTT-учётная запись',
  'client.disconnect': 'Отключён клиент', 'retained.delete': 'Удалено сообщение Retain',
  'account.register': 'Регистрация аккаунта', 'account.login': 'Вход в аккаунт', 'account.login_failed': 'Неудачный вход в аккаунт', 'account.login_limited': 'Вход в аккаунт ограничен', 'account.update': 'Изменение аккаунта', 'account.password': 'Смена пароля аккаунта',
  'account.mqtt_user.create': 'Создание MQTT-учётной записи', 'account.mqtt_user.update': 'Изменение MQTT-учётной записи', 'account.mqtt_user.delete': 'Удаление MQTT-учётной записи', 'account.client.disconnect': 'Отключение своего клиента', 'account.retained.delete': 'Удаление своего Retain',
  'admin.login': 'Вход в панель', 'admin.login_failed': 'Неудачный вход', 'admin.login_limited': 'Вход ограничен',
  'session.login': 'Вход в панель', 'session.logout': 'Выход из панели',
  'user.created': 'Создана MQTT-учётная запись', 'user.updated': 'Изменена MQTT-учётная запись',
  'client.disconnected': 'Отключён клиент', 'retained.deleted': 'Удалено сообщение Retain',
};
const resultLabels: Record<string, string> = { success: 'Успешно', failed: 'Отказ', limited: 'Ограничено' };
const reasons: Record<string, string> = {
  invalid_credentials: 'Неверные учётные данные', invalid_input: 'Некорректные данные',
  origin_denied: 'Источник запроса не разрешён', rate_limit: 'Превышен лимит попыток',
  password_busy: 'Проверка паролей занята',
};
const pageSize = 50;

export function AuditPage({ api, revision }: { api: ApiClient; revision: number }) {
  const [search, setSearch] = useState('');
  const [ip, setIp] = useState('');
  const [result, setResult] = useState('');
  const [filters, setFilters] = useState('');
  const [cursors, setCursors] = useState<string[]>([]);
  const query = new URLSearchParams(filters);
  query.set('limit', String(pageSize + 1));
  if (cursors.length) query.set('before', cursors[cursors.length - 1]);
  const path = '/audit?' + query.toString();
  const apply = (event: FormEvent) => {
    event.preventDefault();
    const next = new URLSearchParams();
    if (search.trim()) next.set('search', search.trim());
    if (ip.trim()) next.set('ip', ip.trim());
    if (result) next.set('result', result);
    setFilters(next.toString()); setCursors([]);
  };
  return <>
    <form className="card audit-filters" onSubmit={apply}>
      <label className="field"><span>Поиск по журналу</span><input type="search" maxLength={128} value={search} onChange={(event) => setSearch(event.target.value)} placeholder="Имя, код действия или объект" /></label>
      <label className="field"><span>IP-адрес</span><input value={ip} maxLength={45} onChange={(event) => setIp(event.target.value)} placeholder="Все адреса" /></label>
      <label className="field"><span>Результат входа</span><select aria-label="Результат входа" value={result} onChange={(event) => setResult(event.target.value)}><option value="">Все действия</option><option value="success">Успешный вход</option><option value="failed">Неудачный вход</option><option value="limited">Ограничение входа</option></select></label>
      <button className="button button-primary" type="submit">Применить</button>
      <button className="button button-secondary" type="button" onClick={() => { setSearch(''); setIp(''); setResult(''); setFilters(''); setCursors([]); }}>Сбросить</button>
    </form>
    <AuditResults key={path} api={api} path={path} revision={revision} page={cursors.length + 1}
      onNext={(cursor) => setCursors([...cursors, cursor])} onPrevious={() => setCursors(cursors.slice(0, -1))} />
    <p className="audit-note">История входов хранится до 30 дней, до 10 000 записей. При массовых попытках запись ограничена: повторные срабатывания блокировки могут отсутствовать.</p>
  </>;
}

function AuditResults({ api, path, revision, page, onNext, onPrevious }: {
  api: ApiClient; path: string; revision: number; page: number;
  onNext: (cursor: string) => void; onPrevious: () => void;
}) {
  const resource = useResource<AuditEntry[]>(api, path, revision);
  return <ResourceView resource={resource}>{(entries) => {
    const rows = entries.slice(0, pageSize);
    return <section className="card">
      <div className="table-toolbar"><div className="toolbar-title"><h2>Последние действия</h2><span className="count-pill">{number(rows.length)}</span></div></div>
      {rows.length ? <div className="table-scroll"><table className="audit-table"><thead><tr><th>Время</th><th>Действие</th><th>Результат</th><th>IP</th><th>Объект</th><th>Автор / имя входа</th></tr></thead><tbody>{rows.map((entry) => <tr key={entry.id}>
        <td className="muted tabular">{dateTime(entry.createdAt)}</td>
        <td><span className="cell-strong">{actionLabels[entry.action] || entry.action}</span><small className="cell-subtitle mono">{entry.action}</small>{entry.reason && <small className="cell-subtitle">{reasons[entry.reason] || entry.reason}</small>}</td>
        <td>{entry.result ? <Badge tone={entry.result === 'success' ? 'green' : entry.result === 'limited' ? 'amber' : 'red'}>{resultLabels[entry.result] || entry.result}</Badge> : '–'}</td>
        <td><code>{entry.ip || '–'}</code></td><td><code className="break-word">{entry.target || '–'}</code></td><td className="break-word">{entry.actor || '–'}</td>
      </tr>)}</tbody></table></div> : <EmptyState icon={ScrollText} title="Нет подходящих событий" description="Измените фильтры или дождитесь новых действий и попыток входа." />}
      <div className="table-footer"><span>Страница {page} · {number(rows.length)} записей</span><div className="audit-pagination">
        <button className="button button-secondary" disabled={page === 1} onClick={onPrevious}>Назад</button>
        <button className="button button-secondary" disabled={entries.length <= pageSize || resource.refreshing} onClick={() => onNext(String(rows[rows.length - 1].id))}>Далее</button>
      </div></div>
    </section>;
  }}</ResourceView>;
}
