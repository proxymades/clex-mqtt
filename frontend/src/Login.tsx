import { useState } from 'react';
import type { FormEvent } from 'react';
import { ArrowRight, Eye, EyeOff, LoaderCircle, ShieldCheck } from 'lucide-react';
import { errorMessage, request } from './api';
import { ErrorNotice } from './components';
import type { Session } from './types';
import { ClexMark } from './ClexMark';

export function Login({ error: initialError, onLogin }: { error: string | null; onLogin: (session: Session) => void }) {
  const [username, setUsername] = useState('');
  const [password, setPassword] = useState('');
  const [showPassword, setShowPassword] = useState(false);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(initialError);
  const submit = async (event: FormEvent) => {
    event.preventDefault(); setBusy(true); setError(null);
    try { onLogin(await request<Session>('/session', { method: 'POST', body: { username: username.trim(), password } })); }
    catch (failure) { setError(errorMessage(failure)); }
    finally { setBusy(false); }
  };
  return <main className="auth-shell">
    <div className="auth-ambient ambient-one" /><div className="auth-ambient ambient-two" />
    <section className="auth-card">
      <div className="auth-brand"><span className="brand-mark"><ClexMark size={43} /></span><span>CLEX <strong>MQTT</strong></span></div>
      <div className="auth-heading"><p className="eyebrow">Администрирование</p><h1>Вход администратора</h1><p>Войдите, чтобы управлять брокером<br className="desktop-break" /> и подключениями устройств.</p></div>
      <form onSubmit={(event) => void submit(event)}>
        <label className="field"><span>Имя администратора</span><input autoComplete="username" autoCapitalize="none" spellCheck={false} value={username} onChange={(event) => setUsername(event.target.value)} required disabled={busy} autoFocus /></label>
        <label className="field"><span>Пароль</span><span className="password-input"><input type={showPassword ? 'text' : 'password'} autoComplete="current-password" value={password} onChange={(event) => setPassword(event.target.value)} required disabled={busy} /><button type="button" className="password-toggle" aria-label={showPassword ? 'Скрыть пароль' : 'Показать пароль'} onClick={() => setShowPassword(!showPassword)}>{showPassword ? <EyeOff size={18} /> : <Eye size={18} />}</button></span></label>
        {error && <ErrorNotice>{error}</ErrorNotice>}
        <button type="submit" className="button button-primary auth-submit" disabled={busy}>{busy ? <LoaderCircle size={18} className="spin" /> : <>Войти в панель<ArrowRight size={17} /></>}</button>
      </form>
      <div className="account-auth-links"><a href="/account">Личный кабинет пользователя</a><a href="/">На главную</a></div>
      <div className="auth-footnote"><ShieldCheck size={14} /><span>Доступ для администратора сервера</span></div>
    </section><p className="auth-caption">CLEX MQTT <span>·</span> Broker console</p>
  </main>;
}
