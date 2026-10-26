import { useCallback, useEffect, useRef, useState } from 'react';
import { errorMessage } from './api';
import type { ApiClient } from './api';
import type { MessageEvent, MessagePage } from './types';

export function useMessageStream(api: ApiClient, query: string, revision: number, paused: boolean) {
  const [messages, setMessages] = useState<MessageEvent[]>([]);
  const [error, setError] = useState<string | null>(null);
  const [refreshing, setRefreshing] = useState(false);
  const [updatedAt, setUpdatedAt] = useState<Date | null>(null);
  const [lost, setLost] = useState(0);
  const pausedRef = useRef(paused);
  const loadRef = useRef<((force?: boolean) => Promise<void>) | null>(null);
  const clearRef = useRef<(() => void) | null>(null);
  const clearVersion = useRef(0);
  useEffect(() => { pausedRef.current = paused; }, [paused]);

  useEffect(() => {
    let disposed = false;
    let busy = false;
    let cursor: number | null = null;
    let instance: string | null = null;
    let baseline = false;
    let controller: AbortController | null = null;
    setMessages([]); setError(null); setLost(0); setUpdatedAt(null);
    const load = async (force = false) => {
      if (disposed || busy || (!force && (pausedRef.current || document.hidden))) return;
      busy = true; setRefreshing(true);
      const version = clearVersion.current;
      controller = new AbortController();
      try {
        for (let batch = 0; batch < 5; batch += 1) {
          const resetting = baseline;
          const suffix = cursor === null || resetting ? '' : `&after=${cursor}`;
          let page = await api<MessagePage>(`/messages?${query}${suffix}`, { signal: controller.signal });
          if (disposed) return;
          if (instance !== null && instance !== page.instanceId) {
            // A fresh process can already have advanced beyond the old cursor.
            page = await api<MessagePage>(`/messages?${query}`, { signal: controller.signal });
            if (disposed) return;
            setMessages([]); setLost(0);
          }
          instance = page.instanceId; cursor = page.cursor;
          if (resetting) {
            baseline = false;
            setError(null); setUpdatedAt(new Date());
            break;
          }
          if (version === clearVersion.current) {
            setMessages((previous) => {
              const ids = new Set(previous.map((entry) => entry.id));
              return [...previous, ...page.messages.filter((entry) => !ids.has(entry.id))].slice(-500);
            });
            setLost((previous) => previous + page.lost);
          }
          setError(null); setUpdatedAt(new Date());
          if (!page.hasMore || pausedRef.current) break;
        }
      } catch (failure) {
        if (!disposed && !controller.signal.aborted) setError(errorMessage(failure));
      } finally {
        busy = false;
        if (!disposed) setRefreshing(false);
      }
    };
    loadRef.current = load;
    clearRef.current = () => { baseline = true; void load(true); };
    void load();
    const timer = window.setInterval(() => { void load(); }, 2000);
    const visible = () => { if (!document.hidden) void load(); };
    window.addEventListener('focus', visible);
    document.addEventListener('visibilitychange', visible);
    return () => {
      disposed = true; controller?.abort(); loadRef.current = null; clearRef.current = null;
      window.clearInterval(timer);
      window.removeEventListener('focus', visible);
      document.removeEventListener('visibilitychange', visible);
    };
  }, [api, query]);

  const previousRevision = useRef(revision);
  useEffect(() => {
    if (revision !== previousRevision.current) {
      previousRevision.current = revision;
      void loadRef.current?.(true);
    }
  }, [revision]);
  useEffect(() => { if (!paused) void loadRef.current?.(); }, [paused]);
  const clear = useCallback(() => {
    clearVersion.current += 1;
    setMessages([]); setLost(0);
    clearRef.current?.();
  }, []);

  return { messages, error, refreshing, updatedAt, lost, clear };
}
