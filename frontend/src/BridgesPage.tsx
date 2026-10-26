import { useState } from 'react';
import type { FormEvent } from 'react';
import { ArrowDownLeft, ArrowUpRight, Cable, LoaderCircle, Pencil, Plus, Power, Trash2 } from 'lucide-react';
import type { ApiClient } from './api';
import { errorMessage } from './api';
import type { Notify } from './App';
import { Badge, ConfirmDialog, EmptyState, ErrorNotice, Modal, ResourceView, SearchField } from './components';
import type { Bridge, BridgeList, BridgeRule } from './types';
import { number } from './format';
import { useResource } from './useResource';

const statuses: Record<string, string> = { connected: 'На связи', connecting: 'Подключение', retrying: 'Повторная попытка', disabled: 'Отключён', stopped: 'Остановлен' };
const newRule = (): BridgeRule => ({ direction: 'out', filter: '#', localPrefix: '', remotePrefix: '' });
const settings = (bridge: Bridge) => ({ name: bridge.name, endpoint: bridge.endpoint, username: bridge.username, caPem: bridge.caPem, rules: bridge.rules, enabled: bridge.enabled, revision: bridge.revision });

export function BridgesPage({ api, revision, notify, onChange }: { api: ApiClient; revision: number; notify: Notify; onChange: () => void }) {
  const resource = useResource<BridgeList>(api, '/bridges', revision);
  const [search, setSearch] = useState('');
  const [form, setForm] = useState<{ bridge?: Bridge } | null>(null);
  const [action, setAction] = useState<{ bridge: Bridge; kind: 'toggle' | 'delete' } | null>(null);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const commit = async () => {
    if (!action) return; setBusy(true); setError(null);
    try {
      await api(`/bridges/${action.bridge.id}`, action.kind === 'delete' ? { method: 'DELETE' } : { method: 'PATCH', body: { ...settings(action.bridge), enabled: !action.bridge.enabled } });
      notify(action.kind === 'delete' ? 'Мост удалён.' : action.bridge.enabled ? 'Мост отключён.' : 'Мост включён.');
      setAction(null); onChange();
    } catch (failure) { setError(errorMessage(failure)); } finally { setBusy(false); }
  };
  return <>
    <div className="page-actions"><p className="subtle-note"><Cable size={17} />Обмен между площадками по MQTT 5 · QoS 1</p><button className="button button-primary" onClick={() => setForm({})} disabled={!!resource.data && resource.data.bridges.length >= 16}><Plus size={17} />Добавить мост</button></div>
    <ResourceView resource={resource}>{(data) => {
      const term = search.trim().toLowerCase();
      const bridges = data.bridges.filter((bridge) => `${bridge.name} ${bridge.endpoint}`.toLowerCase().includes(term));
      return <>
        <section className="card"><div className="table-toolbar"><div className="toolbar-title"><h2>Мосты MQTT</h2><span className="count-pill">{number(data.bridges.length)}</span></div><SearchField value={search} onChange={setSearch} placeholder="Найти мост" /></div>
          {bridges.length ? <div className="bridge-list">{bridges.map((bridge) => <article className="bridge-card" key={bridge.id}>
            <div className="bridge-card-top"><div className="bridge-identity"><button className="cell-link" onClick={() => setForm({ bridge })}><strong>{bridge.name}</strong></button><span className="mono">{bridge.endpoint}</span></div><Badge tone={bridge.status === 'connected' ? 'green' : bridge.enabled ? 'amber' : 'neutral'}>{statuses[bridge.status] ?? bridge.status}</Badge></div>
            <div className="bridge-rules">{bridge.rules.map((rule, index) => <div className="bridge-rule-summary" key={index}>{rule.direction === 'out' ? <ArrowUpRight size={17} /> : <ArrowDownLeft size={17} />}<span>{rule.direction === 'out' ? 'На удалённый брокер' : 'На этот брокер'}<code>{rule.direction === 'out' ? `${rule.localPrefix}${rule.filter} → ${rule.remotePrefix}${rule.filter}` : `${rule.remotePrefix}${rule.filter} → ${rule.localPrefix}${rule.filter}`}</code></span></div>)}</div>
            {bridge.lastError && bridge.enabled && <p className="bridge-error">{bridge.lastError}</p>}
            <div className="bridge-card-bottom"><div className="bridge-stats"><span>В очереди <strong>{number(bridge.queued)}</strong></span><span>Отправлено <strong>{number(bridge.sent)}</strong></span><span>Принято <strong>{number(bridge.received)}</strong></span></div><div className="action-group"><button className="icon-button" title="Настройки моста" aria-label={`Изменить мост ${bridge.name}`} onClick={() => setForm({ bridge })}><Pencil size={16} /></button><button className="icon-button" title={bridge.enabled ? 'Отключить мост' : 'Включить мост'} aria-label={`${bridge.enabled ? 'Отключить' : 'Включить'} мост ${bridge.name}`} onClick={() => { setError(null); setAction({ bridge, kind: 'toggle' }); }}><Power size={16} /></button><button className="icon-button" title="Удалить мост" aria-label={`Удалить мост ${bridge.name}`} onClick={() => { setError(null); setAction({ bridge, kind: 'delete' }); }}><Trash2 size={16} /></button></div></div>
          </article>)}</div> : <EmptyState icon={Cable} title={data.bridges.length ? 'Уточните поиск' : 'Соедините MQTT-брокеры'} description="Добавьте сервер и выберите топики для отправки или получения. Для каждой площадки можно задать собственный префикс." />}
          <div className="table-footer"><span>До 16 мостов · до 16 правил в каждом</span><span>Очередь каждого: {number(data.maxQueuedMessages)} сообщений · {number(data.maxQueuedBytes / 1024 / 1024)} МиБ</span></div>
        </section>
        <p className="subtle-note bridge-footnote">Очередь отправки хранится в PostgreSQL и продолжается после восстановления связи. QoS 1 допускает повторную доставку; обработчики команд могут использовать идентификатор операции.</p>
      </>;
    }}</ResourceView>
    {form && resource.data && <BridgeForm bridge={form.bridge} secretKeyReady={resource.data.secretKeyReady} api={api} onClose={() => setForm(null)} onSaved={() => { setForm(null); notify('Мост сохранён.'); onChange(); }} />}
    {action && <ConfirmDialog title={action.kind === 'delete' ? 'Удалить мост?' : action.bridge.enabled ? 'Отключить мост?' : 'Включить мост?'} description={<><strong>{action.bridge.name}</strong>: {action.kind === 'delete' ? `настройки и очередь из ${number(action.bridge.queued)} сообщений будут удалены.` : action.bridge.enabled ? 'передача приостановится. Текущая очередь сохранится до следующего включения; новые публикации будут обрабатываться остальными маршрутами брокера.' : 'начнётся подключение и отправка сохранённой очереди.'}</>} label={action.kind === 'delete' ? 'Удалить мост' : action.bridge.enabled ? 'Отключить мост' : 'Включить мост'} danger={action.kind === 'delete'} busy={busy} error={error} onConfirm={() => void commit()} onClose={() => setAction(null)} />}
  </>;
}

function BridgeForm({ bridge, secretKeyReady, api, onClose, onSaved }: { bridge?: Bridge; secretKeyReady: boolean; api: ApiClient; onClose: () => void; onSaved: () => void }) {
  const [name, setName] = useState(bridge?.name ?? '');
  const [endpoint, setEndpoint] = useState(bridge?.endpoint ?? 'mqtt://');
  const [username, setUsername] = useState(bridge?.username ?? '');
  const [password, setPassword] = useState('');
  const [clearPassword, setClearPassword] = useState(false);
  const [caPem, setCaPem] = useState(bridge?.caPem ?? '');
  const [rules, setRules] = useState<BridgeRule[]>(bridge?.rules ?? [newRule()]);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const updateRule = (index: number, patch: Partial<BridgeRule>) => setRules((values) => values.map((value, position) => position === index ? { ...value, ...patch } : value));
  const submit = async (event: FormEvent) => {
    event.preventDefault(); setBusy(true); setError(null);
    try {
      await api(bridge ? `/bridges/${bridge.id}` : '/bridges', { method: bridge ? 'PATCH' : 'POST', body: { name, endpoint, username, ...(password ? { password } : {}), clearPassword, caPem: endpoint.startsWith('mqtts://') && caPem.trim() ? caPem : null, rules, enabled: bridge?.enabled ?? true, ...(bridge ? { revision: bridge.revision } : {}) } });
      setPassword(''); onSaved();
    } catch (failure) { setError(errorMessage(failure)); } finally { setBusy(false); }
  };
  return <Modal title={bridge ? 'Настройки моста' : 'Новый мост MQTT'} subtitle="Удалённый брокер с поддержкой MQTT 5" wide onClose={() => { if (!busy) onClose(); }}>
    <form onSubmit={(event) => void submit(event)}><div className="modal-body form-stack">
      <label className="field"><span>Название моста</span><input required maxLength={80} placeholder="Например, мастерская" value={name} onChange={(event) => setName(event.target.value)} disabled={busy} /></label>
      <label className="field"><span>Адрес MQTT-сервера</span><input aria-label="Адрес MQTT-сервера" required maxLength={512} placeholder="mqtts://broker.example.com:8883" value={endpoint} onChange={(event) => setEndpoint(event.target.value)} disabled={busy} autoCapitalize="none" spellCheck={false} /><small>mqtt:// – TCP; mqtts:// – TLS с проверкой сертификата сервера.</small></label>
      <div className="acl-fields"><label className="field"><span>MQTT-логин</span><input value={username} onChange={(event) => setUsername(event.target.value)} maxLength={128} disabled={busy} autoComplete="off" /></label><label className="field"><span>{bridge?.hasPassword ? 'Новый пароль' : 'MQTT-пароль'}</span><input type="password" value={password} onChange={(event) => { setPassword(event.target.value); setClearPassword(false); }} disabled={busy || !secretKeyReady} maxLength={1024} autoComplete="new-password" placeholder={bridge?.hasPassword ? 'Сохранённый пароль используется' : 'Пароль удалённого брокера'} /></label></div>
      {bridge?.hasPassword && <label className="checkbox-field"><input type="checkbox" checked={clearPassword} onChange={(event) => { setClearPassword(event.target.checked); setPassword(''); }} disabled={busy} /><span>Удалить сохранённый пароль</span></label>}
      {!secretKeyReady && <p className="field-hint">Для сохранения паролей задайте CLEX_BRIDGE_SECRET_KEY в окружении сервера. Команда подготовки: python3 scripts/bridge_key.py.</p>}
      {endpoint.startsWith('mqtts://') && <label className="field"><span>Открытые CA-сертификаты PEM</span><textarea className="mono" rows={3} value={caPem} onChange={(event) => setCaPem(event.target.value)} maxLength={12288} disabled={busy} placeholder="Системные доверенные CA используются по умолчанию" /><small>Для собственного CA вставьте открытый сертификат. Имя сервера и срок действия проверяются при подключении.</small></label>}
      <div className="form-section-heading"><Cable size={17} /><div><h3>Правила передачи</h3><p>Фильтр выбирает топики; префиксы задают пространство каждой площадки.</p></div></div>
      {rules.map((rule, index) => <fieldset className="bridge-rule-editor" key={index} disabled={busy}><legend>Правило {index + 1}</legend>
        <div className="bridge-rule-editor-top"><label className="field"><span>Направление</span><select aria-label="Направление" value={rule.direction} onChange={(event) => updateRule(index, { direction: event.target.value as BridgeRule['direction'] })}><option value="out">На удалённый брокер</option><option value="in">На этот брокер</option></select></label><button className="icon-button" type="button" aria-label={`Удалить правило ${index + 1}`} disabled={rules.length === 1} onClick={() => setRules((values) => values.filter((_, position) => position !== index))}><Trash2 size={16} /></button></div>
        <label className="field"><span>Фильтр топиков</span><input required value={rule.filter} onChange={(event) => updateRule(index, { filter: event.target.value })} placeholder="sensors/#" maxLength={1024} /></label>
        <div className="acl-fields"><label className="field"><span>Префикс на этом брокере</span><input value={rule.localPrefix} onChange={(event) => updateRule(index, { localPrefix: event.target.value })} placeholder="workshop/" maxLength={512} /></label><label className="field"><span>Префикс удалённого брокера</span><input value={rule.remotePrefix} onChange={(event) => updateRule(index, { remotePrefix: event.target.value })} placeholder="sites/workshop/" maxLength={512} /></label></div>
        <p className="bridge-mapping mono">{rule.direction === 'out' ? `${rule.localPrefix}${rule.filter} → ${rule.remotePrefix}${rule.filter}` : `${rule.remotePrefix}${rule.filter} → ${rule.localPrefix}${rule.filter}`}</p>
      </fieldset>)}
      <button type="button" className="button button-secondary" disabled={busy || rules.length >= 16} onClick={() => setRules((values) => [...values, newRule()])}><Plus size={16} />Добавить правило</button>
      <p className="field-hint">Префиксы завершаются символом /. Пустой префикс сохраняет исходное начало топика. Retain и свойства MQTT 5 передаются вместе с сообщением. Начальные значения Retain отправляются при включении моста.</p>
      {bridge && <p className="field-hint">Сохранение настроек обновит подключение. Сообщения текущей очереди сохраняют свои топики назначения.</p>}
      {error && <ErrorNotice>{error}</ErrorNotice>}
    </div><footer className="modal-footer"><button type="button" className="button button-secondary" disabled={busy} onClick={onClose}>Отмена</button><button type="submit" className="button button-primary" disabled={busy}>{busy && <LoaderCircle size={16} className="spin" />}{bridge ? 'Сохранить мост' : 'Создать мост'}</button></footer></form>
  </Modal>;
}
