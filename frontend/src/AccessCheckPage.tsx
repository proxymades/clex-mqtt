import { useEffect, useState } from 'react';
import type { FormEvent } from 'react';
import { LoaderCircle, ShieldCheck } from 'lucide-react';
import type { ApiClient } from './api';
import { errorMessage } from './api';
import { Badge, ErrorNotice } from './components';
import { number } from './format';
import type { AccessCheckResult, BrokerUser } from './types';
import { useResource } from './useResource';

const reasons: Record<string, string> = {
  account_scope: 'Топик или фильтр находится вне пространства аккаунта.',
  rule_allowed: 'Доступ предоставлен подходящим правилом.',
  explicit_deny: 'Действует явный запрет. Для подписки учитывается пересечение фильтра с запрещёнными топиками.',
  no_matching_rule: 'Доступ закрыт по умолчанию. Добавьте подходящее разрешение учётной записи или её группе.',
  user_disabled: 'Учётная запись отключена.', user_missing: 'Выберите существующую MQTT-учётную запись.',
  superuser: 'Учётная запись имеет полный MQTT-доступ.', invalid_request: 'Проверьте MQTT-топик или фильтр.',
};

export function AccessCheckPage({ api, revision, username: initialUsername }: { api: ApiClient; revision: number; username: string }) {
  const users = useResource<BrokerUser[]>(api, '/users', revision);
  const [username, setUsername] = useState(initialUsername);
  const [action, setAction] = useState<AccessCheckResult['action']>('publish');
  const [topic, setTopic] = useState('');
  const [result, setResult] = useState<AccessCheckResult | null>(null);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  useEffect(() => { setUsername(initialUsername); setResult(null); }, [initialUsername]);
  useEffect(() => { setResult(null); }, [revision]);
  const submit = async (event: FormEvent) => {
    event.preventDefault(); setBusy(true); setError(null); setResult(null);
    try { setResult(await api<AccessCheckResult>('/access/check', { method: 'POST', body: { username, action, topic } })); }
    catch (failure) { setError(errorMessage(failure)); }
    finally { setBusy(false); }
  };
  return <>
    <section className="card"><header className="card-header"><div><h2>Проверка доступа</h2><p>Проверяет текущие правила брокера для выбранной MQTT-учётной записи.</p></div><ShieldCheck size={21} className="soft-icon" /></header>
      <form className="access-check-form" onSubmit={(event) => void submit(event)}>
        <label className="field"><span>MQTT-логин</span><input value={username} list="acl-usernames" onChange={(event) => { setUsername(event.target.value); setResult(null); }} required maxLength={128} placeholder="Имя учётной записи" /><datalist id="acl-usernames">{users.data?.map((user) => <option key={user.username} value={user.username} />)}</datalist></label>
        <label className="field"><span>Действие</span><select value={action} onChange={(event) => { setAction(event.target.value as AccessCheckResult['action']); setResult(null); }}><option value="publish">Публикация</option><option value="subscribe">Подписка на фильтр</option><option value="receive">Получение из топика</option></select></label>
        <label className="field"><span>{action === 'subscribe' ? 'MQTT-фильтр' : 'Топик'}</span><input className="mono" value={topic} onChange={(event) => { setTopic(event.target.value); setResult(null); }} required maxLength={65535} placeholder={action === 'subscribe' ? 'building/+/state/#' : 'building/room-1/state'} /></label>
        <button className="button button-primary" disabled={busy}>{busy ? <LoaderCircle size={16} className="spin" /> : <ShieldCheck size={16} />}Проверить доступ</button>
      </form>
    </section>
    {(error || users.error) && <ErrorNotice>{error || users.error}</ErrorNotice>}
    {result && <section className="card access-check-result"><header className="card-header"><div><h2>{result.decision.allowed ? 'Доступ разрешён' : 'Доступ закрыт'}</h2><p>{reasons[result.decision.reason] ?? result.decision.reason}</p></div><Badge tone={result.decision.allowed ? 'green' : 'amber'}>{result.decision.allowed ? 'Разрешено' : 'Закрыто'}</Badge></header>
      <div className="access-check-subject"><strong>{result.username}</strong><code>{result.topic}</code></div>
      {result.decision.matchingRules.length > 0 && <div className="table-scroll"><table className="access-rules-table"><thead><tr><th>Источник правила</th><th>MQTT-фильтр</th><th>Решение</th><th>Совпадение</th></tr></thead><tbody>{result.decision.matchingRules.map((rule) => <tr key={rule.id}><td>{rule.source === 'group' ? `Группа: ${rule.groupName}` : rule.source === 'personal' ? 'Личное правило' : 'Общее правило'}</td><td><code>{rule.filter}</code></td><td><Badge tone={rule.allow ? 'green' : 'amber'}>{rule.allow ? 'Разрешить' : 'Запретить'}</Badge></td><td className="muted">{rule.coversRequest ? 'Покрывает запрос' : 'Пересекает фильтр'}</td></tr>)}</tbody></table></div>}
      <div className="table-footer"><span>Подходящих правил: {number(result.decision.matchingTotal)}</span><span>Показано до 100 правил</span></div>
    </section>}
  </>;
}
