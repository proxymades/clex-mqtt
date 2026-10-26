// Monitoring configuration UI with isolated API data and project browser assets.
import assert from 'node:assert/strict';
import { mkdir } from 'node:fs/promises';
import { chromium, expect } from '@playwright/test';

const baseURL = process.env.CLEX_UI_BASE_URL || 'http://127.0.0.1:8085';
assert(['127.0.0.1', 'localhost', '[::1]'].includes(new URL(baseURL).hostname));
const browser = await chromium.launch({ headless: true });
try {
  const page = await browser.newPage({ viewport: { width: 1440, height: 1000 }, reducedMotion: 'reduce', locale: 'ru-RU' });
  page.setDefaultTimeout(12000);
  const errors = [];
  page.on('pageerror', error => errors.push(error.message));
  page.on('console', message => { if (message.type() === 'error') errors.push(message.text()); });
  let enabled = false;
  await page.route('**/api/v1/**', async route => {
    const endpoint = new URL(route.request().url()).pathname;
    if (endpoint.endsWith('/session')) return route.fulfill({ json: { username: 'fixture-admin', csrfToken: 'fixture-only' } });
    assert(endpoint.endsWith('/overview'));
    return route.fulfill({ json: { version: 'fixture', uptimeSeconds: 60, activeConnections: 0, storedSessions: 0, subscriptions: 0, retainedMessages: 0, queuedMessages: 0, incomingMessages: 0, outgoingMessages: 0, rejectedConnections: 5, rejectedPublications: 2, database: { healthy: true }, listeners: [], capabilities: [], monitoring: { enabled, path: '/metrics', databaseSnapshotSeconds: 5 } } });
  });
  await page.goto(`${baseURL}/admin#settings`);
  const monitoring = page.locator('section').filter({ has: page.getByRole('heading', { name: 'Мониторинг', exact: true }) });
  await expect(monitoring).toContainText('Настройка');
  await expect(monitoring).toContainText('CLEX_METRICS_TOKEN');
  enabled = true;
  await page.getByRole('button', { name: 'Обновить данные', exact: true }).click();
  await expect(monitoring).toContainText('Включён');
  await expect(monitoring).toContainText('Отдельный Bearer-токен');
  await expect(monitoring.locator('code')).toHaveText('/metrics');
  await mkdir('work/ui', { recursive: true });
  await page.screenshot({ path: 'work/ui/monitoring-desktop.png', fullPage: true });
  await page.setViewportSize({ width: 390, height: 844 });
  await page.waitForFunction(() => document.querySelector('.sidebar').getBoundingClientRect().right <= 1);
  await page.waitForFunction(() => document.documentElement.scrollWidth <= innerWidth + 1);
  await page.screenshot({ path: 'work/ui/monitoring-mobile.png', fullPage: true });
  await page.goto(`${baseURL}/admin#overview`);
  const rejected = page.locator('.metric-line').filter({ hasText: 'Отклонённые публикации' });
  await expect(rejected.locator('strong')).toHaveText('2');
  assert.deepEqual(errors, []);
  console.log(JSON.stringify({ ok: true, checks: ['disabled setup', 'enabled bearer access', 'endpoint metadata', 'distinct rejection counters', 'desktop/mobile layout'] }, null, 2));
} finally { await browser.close(); }
