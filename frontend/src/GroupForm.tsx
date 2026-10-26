import { useState } from 'react';
import type { FormEvent } from 'react';
import { LoaderCircle, ShieldCheck } from 'lucide-react';
import type { ApiClient } from './api';
import { errorMessage } from './api';
import { ErrorNotice, Modal } from './components';
import type { AccessGroup } from './types';

const lines = (value: string) => [...new Set(value.split('\n').filter((line) => line.length > 0))];
const templates = {
  observer: { title: 'Наблюдение', description: 'Чтение всех топиков выбранного раздела.' },
  telemetry: { title: 'Телеметрия', description: 'Публикация состояния и событий, получение команд.' },
  operator: { title: 'Управление', description: 'Отправка команд, чтение состояния и событий.' },
  exchange: { title: 'Полный обмен', description: 'Публикация и подписка в выбранном разделе.' },
};

export function GroupForm({ group, api, onClose, onSaved }: { group?: AccessGroup; api: ApiClient; onClose: () => void; onSaved: () => void }) {
  const [name, setName] = useState(group?.name ?? '');
  const [description, setDescription] = useState(group?.description ?? '');
  const [publish, setPublish] = useState(group?.publishFilters.join('\n') ?? '');
  const [subscribe, setSubscribe] = useState(group?.subscribeFilters.join('\n') ?? '');
  const [denyPublish, setDenyPublish] = useState(group?.denyPublishFilters.join('\n') ?? '');
  const [denySubscribe, setDenySubscribe] = useState(group?.denySubscribeFilters.join('\n') ?? '');
  const [template, setTemplate] = useState<keyof typeof templates>('telemetry');
  const [prefix, setPrefix] = useState('');
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const applyTemplate = () => {
    if (!prefix || prefix.includes('\0') || /[+#]/.test(prefix) || new TextEncoder().encode(prefix).length > 65000) {
      setError('Укажите базовый топик, например building/floor-1.'); return;
    }
    const base = prefix.endsWith('/') ? prefix.slice(0, -1) : prefix;
    const state = `${base}/state/#`, events = `${base}/events/#`, commands = `${base}/commands/#`;
    setPublish(template === 'observer' ? '' : template === 'telemetry' ? `${state}\n${events}` : template === 'operator' ? commands : `${base}/#`);
    setSubscribe(template === 'observer' || template === 'exchange' ? `${base}/#` : template === 'telemetry' ? commands : `${state}\n${events}`);
    setError(null);
  };
  const submit = async (event: FormEvent) => {
    event.preventDefault(); setBusy(true); setError(null);
    try {
      await api(group ? `/groups/${group.id}` : '/groups', { method: group ? 'PATCH' : 'POST', body: {
        name, description, publishFilters: lines(publish), subscribeFilters: lines(subscribe),
        denyPublishFilters: lines(denyPublish), denySubscribeFilters: lines(denySubscribe),
      } });
      onSaved();
    } catch (failure) { setError(errorMessage(failure)); }
    finally { setBusy(false); }
  };
  return <Modal title={group ? 'Права группы' : 'Новая группа доступа'} subtitle="Общий набор MQTT-прав для нескольких учётных записей" onClose={() => { if (!busy) onClose(); }} wide>
    <form onSubmit={(event) => void submit(event)}><div className="modal-body form-stack">
      <label className="field"><span>Название группы</span><input value={name} onChange={(event) => setName(event.target.value)} required maxLength={80} disabled={busy} placeholder="Например, мастерская" /></label>
      <label className="field"><span>Описание</span><input value={description} onChange={(event) => setDescription(event.target.value)} maxLength={1000} disabled={busy} placeholder="Назначение и зона доступа" /></label>
      <section className="access-template"><div className="form-section-heading"><ShieldCheck size={17} /><div><h3>Шаблон прав</h3><p>{templates[template].description} Применение заменяет разрешающие правила ниже.</p></div></div>
        <div className="template-fields"><label className="field"><span>Назначение</span><select aria-label="Назначение" value={template} onChange={(event) => setTemplate(event.target.value as keyof typeof templates)} disabled={busy}>{Object.entries(templates).map(([key, value]) => <option key={key} value={key}>{value.title}</option>)}</select></label>
          <label className="field"><span>Базовый топик</span><input value={prefix} onChange={(event) => setPrefix(event.target.value)} placeholder="building/floor-1" disabled={busy} /></label></div>
        <button type="button" className="button button-secondary" onClick={applyTemplate} disabled={busy}>Применить шаблон</button>
      </section>
      <div className="form-section-heading"><ShieldCheck size={17} /><div><h3>Разрешающие правила</h3><p>По одному MQTT-фильтру на строку.</p></div></div>
      <div className="acl-fields"><label className="field"><span>Разрешить публикацию</span><textarea aria-label="Разрешить публикацию" className="mono" rows={4} value={publish} onChange={(event) => setPublish(event.target.value)} disabled={busy} /></label><label className="field"><span>Разрешить подписку</span><textarea aria-label="Разрешить подписку" className="mono" rows={4} value={subscribe} onChange={(event) => setSubscribe(event.target.value)} disabled={busy} /></label></div>
      <details className="access-deny"><summary>Запреты и исключения</summary><p className="field-hint">Явный запрет имеет приоритет над разрешениями учётной записи и всех её групп.</p>
        <div className="acl-fields"><label className="field"><span>Запретить публикацию</span><textarea aria-label="Запретить публикацию" className="mono" rows={3} value={denyPublish} onChange={(event) => setDenyPublish(event.target.value)} disabled={busy} /></label><label className="field"><span>Запретить подписку</span><textarea aria-label="Запретить подписку" className="mono" rows={3} value={denySubscribe} onChange={(event) => setDenySubscribe(event.target.value)} disabled={busy} /></label></div>
      </details>
      {group && <p className="field-hint">После сохранения активные клиенты участников группы пройдут повторное подключение с актуальными правами.</p>}
      {error && <ErrorNotice>{error}</ErrorNotice>}
    </div><footer className="modal-footer"><button className="button button-secondary" type="button" onClick={onClose} disabled={busy}>Отмена</button><button className="button button-primary" type="submit" disabled={busy}>{busy && <LoaderCircle size={16} className="spin" />}{group ? 'Сохранить группу' : 'Создать группу'}</button></footer></form>
  </Modal>;
}
