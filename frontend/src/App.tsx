import { useCallback, useEffect, useMemo, useState } from 'react';
import { Check, CircleAlert, X } from 'lucide-react';
import { ApiError, errorMessage, request } from './api';
import type { ApiClient, RequestOptions } from './api';
import { Loading } from './components';
import { Login } from './Login';
import { Console } from './Console';
import { AccountApp } from './AccountApp';
import { EntryPage, UnknownPage } from './EntryPage';
import type { Session } from './types';

export type Notify = (message: string, error?: boolean) => void;

const legacyAdminPages = new Set(['overview', 'clients', 'messages', 'retained', 'users', 'accounts', 'bridges', 'audit', 'settings']);

export default function App() {
  const path = window.location.pathname.replace(/\/+$/, '') || '/';
  if (path === '/account' || path.startsWith('/account/')) return <AccountApp />;
  if (path === '/admin' || path.startsWith('/admin/')) return <AdminApp />;
  if (path === '/') {
    const legacyPage = window.location.hash.slice(1).split('?')[0];
    return legacyAdminPages.has(legacyPage) ? <LegacyAdminRedirect /> : <EntryPage />;
  }
  return <UnknownPage />;
}

function LegacyAdminRedirect() {
  useEffect(() => { window.location.replace('/admin' + window.location.search + window.location.hash); }, []);
  return <div className="auth-shell"><Loading label="Открываем административную панель" /></div>;
}

function AdminApp() {
  const [session, setSession] = useState<Session | null>(null);
  const [checking, setChecking] = useState(true);
  const [sessionError, setSessionError] = useState<string | null>(null);
  const [toast, setToast] = useState<{ message: string; error: boolean } | null>(null);
  useEffect(() => {
    const controller = new AbortController();
    request<Session>('/session', { signal: controller.signal }).then(setSession).catch((error: unknown) => {
      if (!controller.signal.aborted && !(error instanceof ApiError && error.status === 401)) setSessionError(errorMessage(error));
    }).finally(() => { if (!controller.signal.aborted) setChecking(false); });
    return () => controller.abort();
  }, []);
  const notify: Notify = useCallback((message, error = false) => setToast({ message, error }), []);
  useEffect(() => {
    if (!toast) return;
    const timeout = window.setTimeout(() => setToast(null), toast.error ? 7000 : 4000);
    return () => window.clearTimeout(timeout);
  }, [toast]);
  const api = useMemo<ApiClient>(() => async <T,>(path: string, options: RequestOptions = {}) => {
    try { return await request<T>(path, { ...options, csrfToken: session?.csrfToken }); }
    catch (error) {
      if (error instanceof ApiError && error.status === 401) { setSession(null); setSessionError('Сеанс завершён. Войдите снова.'); }
      throw error;
    }
  }, [session]);
  if (checking) return <div className="auth-shell"><Loading label="Проверяем сеанс" /></div>;
  return <>
    {session ? <Console session={session} api={api} notify={notify} onLogout={() => { setSession(null); setSessionError(null); }} />
      : <Login error={sessionError} onLogin={(next) => { setSession(next); setSessionError(null); }} />}
    {toast && <div className={`toast ${toast.error ? 'toast-error' : ''}`} role="status">{toast.error ? <CircleAlert size={18} /> : <Check size={18} />}<span>{toast.message}</span><button aria-label="Закрыть уведомление" onClick={() => setToast(null)}><X size={15} /></button></div>}
  </>;
}
