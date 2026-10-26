import { useEffect, useMemo, useState } from 'react';
import { ListFilter } from 'lucide-react';
import type { ApiClient } from './api';
import type { Notify } from './App';
import { DiagnosticsPage } from './DiagnosticsPage';
import { RetainedPage } from './RetainedPage';
import { MessageViewer } from './MessageViewer';
import { useMessageStream } from './useMessageStream';

interface Props { api: ApiClient; revision: number; notify: Notify; onChange: () => void }
const currentHash = () => window.location.hash.slice(1);

export function MessagesPage(props: Props) {
  const [hash, setHash] = useState(currentHash);
  useEffect(() => {
    const changed = () => setHash(currentHash());
    window.addEventListener('hashchange', changed);
    return () => window.removeEventListener('hashchange', changed);
  }, []);
  const query = new URLSearchParams(hash.split('?')[1] ?? '');
  const diagnostics = query.get('tab') === 'diagnostics';
  const retained = hash === 'retained' || query.get('tab') === 'retain';
  const filters = { clientId: query.get('clientId') ?? '', username: query.get('username') ?? '', topic: query.get('topic') ?? '' };
  const filterKey = JSON.stringify(filters);
  return <>
    <nav className="message-tabs" aria-label="Вкладки сообщений">
      <a href="#messages" aria-current={!retained && !diagnostics ? 'page' : undefined}>Поток</a>
      <a href="#messages?tab=retain" aria-current={retained ? 'page' : undefined}>Retain</a>
      <a href="#messages?tab=diagnostics" aria-current={diagnostics ? 'page' : undefined}>Диагностика</a>
    </nav>
    {diagnostics ? <DiagnosticsPage key={filterKey} {...props} filters={filters} /> : retained ? <RetainedPage {...props} /> : <MessageStream key={filterKey} {...props} filters={filters} />}
  </>;
}

function MessageStream({ api, revision, notify, filters }: Props & { filters: { clientId: string; username: string; topic: string } }) {
  const [draft, setDraft] = useState(filters);
  const [paused, setPaused] = useState(false);
  const query = useMemo(() => {
    const params = new URLSearchParams();
    for (const [key, value] of Object.entries(filters)) if (value) params.set(key, value);
    return params.toString();
  }, [filters.clientId, filters.username, filters.topic]);
  const stream = useMessageStream(api, query, revision, paused);
  const applyFilters = (next = draft) => {
    const params = new URLSearchParams();
    for (const [key, value] of Object.entries(next)) if (value) params.set(key, value);
    window.location.hash = `messages${params.size ? `?${params}` : ''}`;
  };
  return <>
    <section className="card message-filter-card">
      <form className="message-filters" onSubmit={(event) => { event.preventDefault(); applyFilters(); }}>
        <label>Client ID<input value={draft.clientId} onChange={(event) => setDraft({ ...draft, clientId: event.target.value })} placeholder="Все клиенты" maxLength={65535} /></label>
        <label>MQTT-логин<input value={draft.username} onChange={(event) => setDraft({ ...draft, username: event.target.value })} placeholder="Все учётные записи" maxLength={128} /></label>
        <label>Топик или фильтр<input value={draft.topic} onChange={(event) => setDraft({ ...draft, topic: event.target.value })} placeholder="Например, building/+/status" maxLength={65535} /></label>
        <button className="button button-secondary" type="submit"><ListFilter size={15} />Применить</button>
        {query && <button type="button" className="text-button" onClick={() => applyFilters({ clientId: '', username: '', topic: '' })}>Все источники</button>}
      </form>
    </section>
    <MessageViewer stream={stream} paused={paused} onPausedChange={setPaused} notify={notify} filtersApplied={!!query}
      onSourceSelect={(clientId, username) => applyFilters({ ...filters, clientId, username })}
      diagnosticsHref={(message) => '#messages?tab=diagnostics&' + new URLSearchParams({ clientId: message.clientId, username: message.username, topic: message.topic })}
      bufferNote="В памяти брокера – до 1 000 последних публикаций, предпросмотр содержимого – до 4 КБ. Новое наблюдение начинается с последних 100 событий. Буфер обновляется по мере обмена и при перезапуске брокера." />
  </>;
}
