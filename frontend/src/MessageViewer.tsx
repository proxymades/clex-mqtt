import { useState } from 'react';
import { ChevronRight, Copy, Pause, Play, Radio, Trash2 } from 'lucide-react';
import type { Notify } from './App';
import { Badge, DetailRow, EmptyState, ErrorNotice, Modal, SearchField } from './components';
import { bytes, decodePayload, fullDateTime, number } from './format';
import type { MessageEvent } from './types';

export type MessagePreview = Omit<MessageEvent, 'id'> & { id: number | string };
interface Props {
  stream: { messages: readonly MessagePreview[]; error: string | null; lost: number; refreshing: boolean; updatedAt: Date | null; clear: () => void };
  paused: boolean; onPausedChange: (paused: boolean) => void; notify: Notify;
  filtersApplied?: boolean; windowLimit?: number; bufferNote: string;
  onSourceSelect?: (clientId: string, username: string) => void;
  diagnosticsHref?: (message: MessagePreview) => string;
}
function messagePreview(message: MessagePreview) {
  const decoded = decodePayload(message.payloadBase64.slice(0, 344), message.payloadTruncated || message.payloadBase64.length > 344);
  return decoded.binary ? 'Бинарные данные' : decoded.text.replace(/\s+/g, ' ').slice(0, 110) || 'Пустое сообщение';
}
export function MessageViewer({ stream, paused, onPausedChange, notify, filtersApplied = false, windowLimit = 500, bufferNote, onSourceSelect, diagnosticsHref }: Props) {
  const [search, setSearch] = useState('');
  const [selected, setSelected] = useState<MessagePreview | null>(null);
  const term = search.toLowerCase().trim();
  const messages = stream.messages.filter((message) => !term || `${message.clientId} ${message.username} ${message.topic} ${decodePayload(message.payloadBase64, message.payloadTruncated).text}`.toLowerCase().includes(term)).reverse();
  const decoded = selected ? decodePayload(selected.payloadBase64, selected.payloadTruncated) : null;
  const copy = async (value: string) => {
    try { await navigator.clipboard.writeText(value); notify('Скопировано.'); }
    catch { notify('Выделите содержимое и скопируйте вручную.', true); }
  };
  return <>
    {stream.error && <ErrorNotice>{stream.error}</ErrorNotice>}
    {stream.lost > 0 && <div className="notice notice-info">Общий буфер обновился: {number(stream.lost)} более ранних событий вышли за его пределы. Уточните источник или топик для удобного просмотра потока.</div>}
    <section className="card">
      <div className="table-toolbar message-toolbar"><div className="toolbar-title"><h2>Поток сообщений</h2><Badge tone={paused ? 'neutral' : 'green'}>{paused ? 'Пауза' : 'Наблюдение'}</Badge></div><div className="message-actions"><button className="button button-secondary" onClick={() => onPausedChange(!paused)}>{paused ? <Play size={14} /> : <Pause size={14} />}{paused ? 'Продолжить' : 'Пауза'}</button><button className="icon-button" title="Очистить список на экране" aria-label="Очистить список на экране" onClick={stream.clear}><Trash2 size={16} /></button></div></div>
      <div className="message-search"><SearchField value={search} onChange={setSearch} placeholder="Найти в топиках и содержимом" /></div>
      {messages.length ? <div className="table-scroll"><table className="messages-table"><thead><tr><th>Время</th><th>Источник</th><th>Топик / содержимое</th><th>QoS</th><th className="numeric">Размер</th><th><span className="sr-only">Подробнее</span></th></tr></thead><tbody>{messages.map((message) => <tr key={message.id}>
        <td className="muted tabular" title={fullDateTime(message.receivedAt)}>{new Date(message.receivedAt).toLocaleTimeString('ru-RU')}</td>
        <td><button className="cell-link message-source" title="Показать сообщения этого клиента" onClick={() => onSourceSelect ? onSourceSelect(message.clientId, message.username) : setSearch(message.clientId)}><strong className="mono">{message.clientId}</strong><small>{message.username}</small></button></td>
        <td><button className="cell-link retained-identity" onClick={() => setSelected(message)}><strong className="mono">{message.topic}</strong><small>{messagePreview(message)}</small></button><div className="message-flags">{message.retain && <Badge>Retain</Badge>}{message.will && <Badge tone="amber">LWT</Badge>}</div></td>
        <td><span className="qos-label">{message.qos}</span></td><td className="numeric muted tabular">{bytes(message.payloadBytes)}</td><td className="row-actions"><button className="icon-button" aria-label={`Открыть сообщение ${message.id}`} onClick={() => setSelected(message)}><ChevronRight size={18} /></button></td>
      </tr>)}</tbody></table></div> : <EmptyState icon={Radio} title={term ? 'Уточните поиск' : paused ? 'Просмотр на паузе' : 'Ожидаем публикации'} description={term ? 'Укажите другой топик или фрагмент содержимого.' : paused ? 'Нажмите «Продолжить», чтобы загрузить новые события.' : filtersApplied ? 'Здесь появятся публикации выбранных источников и топиков.' : 'Здесь появляются принятые брокером публикации. Выберите клиента, MQTT-логин или топик для наблюдения.'} />}
      <div className="table-footer"><span>Показано {number(messages.length)} · последние {windowLimit} в окне</span><span>{stream.updatedAt ? `Обновлено в ${stream.updatedAt.toLocaleTimeString('ru-RU')}` : stream.refreshing ? 'Загружаем события' : 'Обновление каждые 2 с'}</span></div>
    </section>
    <p className="field-hint message-buffer-note">{bufferNote}</p>
    {selected && decoded && <Modal title="Сообщение MQTT" subtitle={selected.clientId} onClose={() => setSelected(null)} wide>
      <div className="modal-body"><div className="topic-heading"><code>{selected.topic}</code><button className="icon-button" aria-label="Скопировать топик" onClick={() => void copy(selected.topic)}><Copy size={16} /></button></div>
        <dl className="detail-list detail-list-compact"><DetailRow label="Время">{fullDateTime(selected.receivedAt)}</DetailRow><DetailRow label="Client ID"><code>{selected.clientId}</code></DetailRow><DetailRow label="MQTT-логин">{selected.username}</DetailRow><DetailRow label="QoS">{selected.qos}</DetailRow><DetailRow label="Retain">{selected.retain ? 'Включён' : 'Выключен'}</DetailRow><DetailRow label="Источник публикации">{selected.will ? 'Сообщение при обрыве связи (LWT)' : 'MQTT-клиент'}</DetailRow><DetailRow label="Размер">{bytes(selected.payloadBytes)}</DetailRow></dl>
        {selected.payloadTruncated && <div className="notice notice-info payload-truncation">Показаны первые 4 КБ. Полный размер – {bytes(selected.payloadBytes)}.</div>}
        <div className="payload-heading"><span>{decoded.binary ? 'Бинарные данные · Base64' : 'Содержимое · UTF-8'}</span><button className="text-button" onClick={() => void copy(decoded.text)}><Copy size={14} />{selected.payloadTruncated ? 'Скопировать фрагмент' : 'Скопировать'}</button></div><pre className="payload-content">{decoded.text || 'Пустое сообщение'}</pre>
      </div><footer className="modal-footer">{diagnosticsHref && <a className="button button-secondary" href={diagnosticsHref(selected)} onClick={() => setSelected(null)}>Диагностика новых публикаций</a>}<button className="button button-secondary" onClick={() => setSelected(null)}>Закрыть</button></footer>
    </Modal>}
  </>;
}
