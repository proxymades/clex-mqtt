import { useState } from 'react';
import { FolderKey, Pencil, Plus, Power, Trash2 } from 'lucide-react';
import type { ApiClient } from './api';
import { errorMessage } from './api';
import type { Notify } from './App';
import { Badge, ConfirmDialog, EmptyState, ResourceView, SearchField } from './components';
import { number } from './format';
import { GroupForm } from './GroupForm';
import type { AccessGroup } from './types';
import { useResource } from './useResource';

export function GroupsPage({ api, revision, notify, onChange }: { api: ApiClient; revision: number; notify: Notify; onChange: () => void }) {
  const resource = useResource<AccessGroup[]>(api, '/groups', revision);
  const [search, setSearch] = useState('');
  const [form, setForm] = useState<{ group?: AccessGroup } | null>(null);
  const [action, setAction] = useState<{ group: AccessGroup; kind: 'toggle' | 'delete' } | null>(null);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const commitAction = async () => {
    if (!action) return;
    setBusy(true); setError(null);
    try {
      await api(`/groups/${action.group.id}`, action.kind === 'delete' ? { method: 'DELETE' } : { method: 'PATCH', body: { enabled: !action.group.enabled } });
      notify(action.kind === 'delete' ? 'Группа удалена.' : action.group.enabled ? 'Правила группы отключены.' : 'Правила группы включены.');
      setAction(null); onChange();
    } catch (failure) { setError(errorMessage(failure)); }
    finally { setBusy(false); }
  };
  return <>
    <div className="page-actions"><p className="subtle-note"><FolderKey size={17} />Общие правила для учётных записей MQTT</p><button className="button button-primary" onClick={() => setForm({})}><Plus size={17} />Добавить группу</button></div>
    <ResourceView resource={resource}>{(groups) => {
      const term = search.trim().toLowerCase();
      const filtered = groups.filter((group) => `${group.name} ${group.description}`.toLowerCase().includes(term));
      return <section className="card"><div className="table-toolbar"><div className="toolbar-title"><h2>Группы доступа</h2><span className="count-pill">{number(groups.length)}</span></div><SearchField value={search} onChange={setSearch} placeholder="Найти группу" /></div>
        {filtered.length ? <div className="table-scroll"><table className="groups-table"><thead><tr><th>Группа</th><th>Состояние</th><th className="numeric">Участники</th><th className="numeric">Правила</th><th><span className="sr-only">Действия</span></th></tr></thead><tbody>{filtered.map((group) => <tr key={group.id}>
          <td><button className="cell-link group-identity" onClick={() => setForm({ group })}><strong>{group.name}</strong><small>{group.description || 'Общий набор MQTT-прав'}</small></button></td>
          <td><Badge tone={group.enabled ? 'green' : 'neutral'}>{group.enabled ? 'Включена' : 'Отключена'}</Badge></td><td className="numeric tabular"><a className="text-button" title="Показать участников группы" href={`#users?${new URLSearchParams({ groupId: group.id })}`}>{number(group.memberCount)}</a></td><td className="numeric tabular">{number(group.publishFilters.length + group.subscribeFilters.length + group.denyPublishFilters.length + group.denySubscribeFilters.length)}</td>
          <td className="row-actions"><div className="action-group"><button className="icon-button" title="Изменить группу" aria-label={`Изменить группу ${group.name}`} onClick={() => setForm({ group })}><Pencil size={16} /></button><button className="icon-button" title={group.enabled ? 'Отключить правила группы' : 'Включить правила группы'} aria-label={`${group.enabled ? 'Отключить' : 'Включить'} группу ${group.name}`} onClick={() => { setError(null); setAction({ group, kind: 'toggle' }); }}><Power size={16} /></button><button className="icon-button" title="Удалить группу" aria-label={`Удалить группу ${group.name}`} onClick={() => { setError(null); setAction({ group, kind: 'delete' }); }}><Trash2 size={16} /></button></div></td>
        </tr>)}</tbody></table></div> : <EmptyState icon={FolderKey} title={groups.length ? 'Уточните поиск' : 'Создайте группу доступа'} description={groups.length ? 'Укажите другое название или описание.' : 'Задайте общие топики для проекта, площадки или группы MQTT-клиентов. Затем назначьте группу учётным записям.'} />}
        <div className="table-footer"><span>Показано {number(filtered.length)} из {number(groups.length)}</span><span>До 1 000 групп</span></div>
      </section>;
    }}</ResourceView>
    {form && <GroupForm group={form.group} api={api} onClose={() => setForm(null)} onSaved={() => { setForm(null); notify('Группа сохранена.'); onChange(); }} />}
    {action && <ConfirmDialog title={action.kind === 'delete' ? 'Удалить группу?' : action.group.enabled ? 'Отключить правила группы?' : 'Включить правила группы?'} description={<><strong>{action.group.name}</strong>: {action.kind === 'delete' ? 'будут удалены правила группы и назначения её участникам.' : action.group.enabled ? 'участники продолжат работать по личным правилам и другим активным группам.' : 'правила снова станут доступны участникам.'} Активные клиенты участников пройдут повторное подключение.</>} label={action.kind === 'delete' ? 'Удалить группу' : action.group.enabled ? 'Отключить правила' : 'Включить правила'} busy={busy} error={error} onConfirm={() => void commitAction()} onClose={() => setAction(null)} />}
  </>;
}
