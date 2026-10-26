import { useState } from 'react';
import { ChevronRight, Copy, Database, Trash2 } from 'lucide-react';
import { errorMessage } from './api';
import type { ApiClient } from './api';
import type { Notify } from './App';
import { ConfirmDialog, DetailRow, EmptyState, Modal, ResourceView, SearchField } from './components';
import { bytes, dateTime, decodePayload, fullDateTime, number } from './format';
import type { RetainedMessage } from './types';
import { useResource } from './useResource';

interface Props { api: ApiClient; revision: number; notify: Notify; onChange: () => void }

function preview(message: RetainedMessage): string {
  const decoded = decodePayload(message.payloadBase64.slice(0, 344), message.payloadBase64.length > 344 || message.payloadTruncated);
  return decoded.binary ? 'Бинарные данные' : decoded.text.replace(/\s+/g, ' ').slice(0, 110) || 'Пустое сообщение';
}

export function RetainedPage({ api, revision, notify, onChange }: Props) {
  const resource = useResource<RetainedMessage[]>(api, '/retained', revision);
  const [search, setSearch] = useState('');
  const [selectedTopic, setSelectedTopic] = useState<string | null>(null);
  const [deleting, setDeleting] = useState<RetainedMessage | null>(null);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const selected = resource.data?.find((message) => message.topic === selectedTopic);
  const decoded = selected ? decodePayload(selected.payloadBase64, selected.payloadTruncated) : null;
  const remove = async () => {
    if (!deleting) return;
    setBusy(true); setError(null);
    try { await api('/retained/delete', { method: 'POST', body: { topic: deleting.topic } }); setDeleting(null); notify('Сообщение Retain удалено.'); onChange(); }
    catch (failure) { setError(errorMessage(failure)); }
    finally { setBusy(false); }
  };
  const copy = async (text: string) => {
    try { await navigator.clipboard.writeText(text); notify('Скопировано.'); }
    catch { notify('Ошибка копирования. Выделите текст и скопируйте вручную.', true); }
  };
  return <>
    <ResourceView resource={resource}>{(messages) => {
      const filtered = messages.filter((message) => message.topic.toLowerCase().includes(search.trim().toLowerCase()));
      return <section className="card"><div className="table-toolbar"><div className="toolbar-title"><h2>Retain</h2><span className="count-pill">{number(messages.length)}</span></div><SearchField value={search} onChange={setSearch} placeholder="Найти по топику" /></div>
        {filtered.length ? <div className="table-scroll"><table className="retained-table"><thead><tr><th>Топик / сообщение</th><th>QoS</th><th className="numeric">Размер</th><th>Обновлено</th><th><span className="sr-only">Действия</span></th></tr></thead><tbody>{filtered.map((message) => <tr key={message.topic}><td><button className="cell-link retained-identity" onClick={() => setSelectedTopic(message.topic)}><strong className="mono">{message.topic}</strong><small>{preview(message)}</small></button></td><td><span className="qos-label">{message.qos}</span></td><td className="numeric muted tabular">{bytes(message.payloadBytes)}</td><td className="muted tabular">{dateTime(message.updatedAt)}</td><td className="row-actions"><button className="icon-button" aria-label={`Открыть ${message.topic}`} title="Открыть сообщение" onClick={() => setSelectedTopic(message.topic)}><ChevronRight size={18} /></button></td></tr>)}</tbody></table></div> : <EmptyState icon={Database} title={messages.length ? 'Уточните поиск топика' : 'Опубликуйте сообщение с Retain'} description={messages.length ? 'Укажите другое имя или часть топика.' : 'В MQTT-клиенте включите Retain при публикации. Брокер передаст последнее сообщение топика новым подписчикам.'} />}
        <div className="table-footer"><span>Показано {number(filtered.length)} из {number(messages.length)} загруженных</span><span>До 100 сообщений</span></div>
      </section>;
    }}</ResourceView>
    {selected && decoded && <Modal title="Сообщение Retain" onClose={() => setSelectedTopic(null)} wide>
      <div className="modal-body">
        <div className="topic-heading"><code>{selected.topic}</code><button className="icon-button" title="Скопировать топик" aria-label="Скопировать топик" onClick={() => void copy(selected.topic)}><Copy size={16} /></button></div>
        <dl className="detail-list detail-list-compact"><DetailRow label="QoS">{selected.qos}</DetailRow><DetailRow label="Размер">{bytes(selected.payloadBytes)}</DetailRow><DetailRow label="Обновлено">{fullDateTime(selected.updatedAt)}</DetailRow></dl>
        {selected.payloadTruncated && <div className="notice notice-info payload-truncation">Показан фрагмент сообщения. Полный размер – {bytes(selected.payloadBytes)}. При копировании сохраняется этот фрагмент.</div>}
        <div className="payload-heading"><span>{decoded.binary ? 'Бинарные данные · Base64' : 'Содержимое · UTF-8'}</span><button className="text-button" onClick={() => void copy(decoded.text)}><Copy size={14} />{selected.payloadTruncated ? 'Скопировать фрагмент' : 'Скопировать'}</button></div>
        <pre className="payload-content">{decoded.text.slice(0, 65536) || 'Пустое сообщение'}</pre>
        {decoded.text.length > 65536 && <p className="field-hint">Показаны первые 65 536 символов. Копирование сохраняет содержимое, полученное от сервера.</p>}
      </div>
      <footer className="modal-footer"><button className="button button-secondary" onClick={() => setSelectedTopic(null)}>Закрыть</button><button className="button button-danger-soft" onClick={() => { setError(null); setDeleting(selected); setSelectedTopic(null); }}><Trash2 size={16} />Удалить сообщение</button></footer>
    </Modal>}
    {deleting && <ConfirmDialog title="Удалить сообщение Retain?" description={<>Сообщение Retain топика <strong className="mono break-word">{deleting.topic}</strong> будет удалено.</>} label="Удалить сообщение" busy={busy} error={error} onConfirm={() => void remove()} onClose={() => setDeleting(null)} />}
  </>;
}
