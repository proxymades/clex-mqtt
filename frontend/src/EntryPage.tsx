import { useEffect, useState } from 'react';
import { ArrowRight } from 'lucide-react';
import { ClexMark } from './ClexMark';

export function EntryPage() {
  const [registrationEnabled, setRegistrationEnabled] = useState(false);
  useEffect(() => {
    const controller = new AbortController();
    fetch('/api/v1/account/options', { signal: controller.signal, credentials: 'omit', cache: 'no-store' })
      .then(async (response) => {
        if (!response.ok) return;
        const options = await response.json() as { registrationEnabled: boolean };
        if (!controller.signal.aborted) setRegistrationEnabled(options.registrationEnabled === true);
      })
      .catch(() => { /* Login remains available when public options cannot be loaded. */ });
    return () => controller.abort();
  }, []);
  return <main className="entry-shell"><section className="entry-content" aria-labelledby="entry-title">
    <a className="entry-brand" href="/" aria-label="CLEX MQTT – Главная"><ClexMark size={48} /><span>CLEX <strong>MQTT</strong></span></a>
    <header className="entry-heading"><h1 id="entry-title">MQTT-брокер для ваших устройств</h1><p>Подключайте устройства и управляйте своими MQTT-учётными записями в личном кабинете.</p></header>
    <div className="entry-actions">
      <a className="button button-primary entry-action" href="/account">Войти в личный кабинет<ArrowRight size={18} aria-hidden="true" /></a>
      {registrationEnabled && <a className="entry-register" href="/account/register">Создать аккаунт</a>}
    </div>
  </section></main>;
}

export function UnknownPage() {
  return <main className="auth-shell"><section className="auth-card entry-not-found"><ClexMark size={43} /><h1>Страница не найдена</h1><p>Перейдите на главную страницу, чтобы открыть личный кабинет.</p><a className="button button-primary" href="/">На главную</a></section></main>;
}
