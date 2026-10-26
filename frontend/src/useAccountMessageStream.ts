import { useCallback, useEffect, useRef, useState } from 'react';
import { errorMessage } from './api';
import type { ApiClient } from './api';
import type { MessagePreview } from './MessageViewer';

interface PersonalMessage extends Omit<MessagePreview, 'id'> { eventKey: string }
export function useAccountMessageStream(api: ApiClient, revision: number, paused: boolean) {
  const [messages, setMessages] = useState<MessagePreview[]>([]);
  const [error, setError] = useState<string | null>(null);
  const [refreshing, setRefreshing] = useState(false);
  const [updatedAt, setUpdatedAt] = useState<Date | null>(null);
  const snapshot = useRef<PersonalMessage[]>([]);
  const hidden = useRef(new Set<string>());
  useEffect(() => {
    if (paused) { setRefreshing(false); return; }
    let disposed = false, busy = false;
    let controller: AbortController | null = null;
    const load = async () => {
      if (disposed || busy || document.hidden) return;
      busy = true; setRefreshing(true); controller = new AbortController();
      try {
        const page = await api<{ messages: PersonalMessage[] }>('/messages', { signal: controller.signal });
        if (disposed) return;
        snapshot.current = page.messages;
        const keys = new Set(page.messages.map((message) => message.eventKey));
        hidden.current = new Set([...hidden.current].filter((key) => keys.has(key)));
        setMessages(page.messages.filter((message) => !hidden.current.has(message.eventKey)).map(({ eventKey, ...message }) => ({ ...message, id: eventKey })));
        setError(null); setUpdatedAt(new Date());
      } catch (failure) {
        if (!disposed && !controller.signal.aborted) setError(errorMessage(failure));
      } finally {
        busy = false;
        if (!disposed) setRefreshing(false);
      }
    };
    void load();
    const timer = window.setInterval(() => { void load(); }, 2000);
    const visible = () => { if (!document.hidden) void load(); };
    window.addEventListener('focus', visible);
    document.addEventListener('visibilitychange', visible);
    return () => {
      disposed = true; controller?.abort(); window.clearInterval(timer);
      window.removeEventListener('focus', visible);
      document.removeEventListener('visibilitychange', visible);
    };
  }, [api, revision, paused]);
  const clear = useCallback(() => {
    hidden.current = new Set(snapshot.current.map((message) => message.eventKey));
    setMessages([]);
  }, []);
  return { messages, error, refreshing, updatedAt, lost: 0, clear };
}
