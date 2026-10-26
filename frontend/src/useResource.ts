import { useCallback, useEffect, useRef, useState } from 'react';
import { errorMessage } from './api';
import type { ApiClient } from './api';

export interface Resource<T> {
  data: T | null;
  error: string | null;
  loading: boolean;
  refreshing: boolean;
  updatedAt: Date | null;
  refresh: () => Promise<void>;
}

export function useResource<T>(api: ApiClient, path: string, revision = 0, intervalMs = 10_000): Resource<T> {
  const [data, setData] = useState<T | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [refreshing, setRefreshing] = useState(false);
  const [updatedAt, setUpdatedAt] = useState<Date | null>(null);
  const controller = useRef<AbortController | null>(null);
  const mounted = useRef(true);

  const refresh = useCallback(async () => {
    controller.current?.abort();
    const next = new AbortController();
    controller.current = next;
    setRefreshing(true);
    try {
      const result = await api<T>(path, { signal: next.signal });
      if (mounted.current && !next.signal.aborted) {
        setData(result);
        setError(null);
        setUpdatedAt(new Date());
      }
    } catch (failure) {
      if (mounted.current && !next.signal.aborted) setError(errorMessage(failure));
    } finally {
      if (mounted.current && !next.signal.aborted) setRefreshing(false);
      if (controller.current === next) controller.current = null;
    }
  }, [api, path]);

  useEffect(() => {
    mounted.current = true;
    void refresh();
    const interval = window.setInterval(() => {
      if (!document.hidden && !controller.current) void refresh();
    }, intervalMs);
    const onFocus = () => { if (!controller.current) void refresh(); };
    const onVisibility = () => { if (!document.hidden && !controller.current) void refresh(); };
    window.addEventListener('focus', onFocus);
    document.addEventListener('visibilitychange', onVisibility);
    return () => {
      mounted.current = false;
      controller.current?.abort();
      window.clearInterval(interval);
      window.removeEventListener('focus', onFocus);
      document.removeEventListener('visibilitychange', onVisibility);
    };
  }, [refresh, intervalMs]);

  const previousRevision = useRef(revision);
  useEffect(() => {
    if (previousRevision.current !== revision) {
      previousRevision.current = revision;
      void refresh();
    }
  }, [refresh, revision]);

  return { data, error, loading: data === null && !error, refreshing, updatedAt, refresh };
}
