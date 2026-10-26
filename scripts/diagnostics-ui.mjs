// Headless console checks with isolated API responses; wire tests cover MQTT.
import assert from 'node:assert/strict';
import { mkdir } from 'node:fs/promises';
import { chromium, expect } from '@playwright/test';
const baseURL = process.env.CLEX_UI_BASE_URL || 'http://127.0.0.1:8085';
assert(['127.0.0.1', 'localhost', '[::1]'].includes(new URL(baseURL).hostname));
const browser = await chromium.launch({ headless: true });
try {
  const page = await browser.newPage({ viewport: { width: 1440, height: 1000 } });
  const errors = [];
  page.on('pageerror', (error) => errors.push(error.message));
  page.on('console', (message) => { if (message.type() === 'error') errors.push(message.text()); });
  const state = { instanceId: 'process-one', id: null, active: false, filter: null, expiresAt: null, capacity: 100, eventCapacity: 128, bytes: 0, evicted: 0, messages: [] };
  const message = { id: 'captured-publication', receivedAt: new Date().toISOString(), clientId: 'esp32-01', username: 'ivan', topic: 'building/a/status', qos: 2, status: 'committed', queued: 1, truncated: 0 };
  let starts = 0;
  await page.route('**/api/v1/**', async (route) => {
    const request = route.request(); const url = new URL(request.url()); let json;
    if (url.pathname.endsWith('/session')) json = { username: 'fixture-admin', csrfToken: 'fixture-only' };
    else if (url.pathname.endsWith('/overview')) json = { version: 'fixture', uptimeSeconds: 60, activeConnections: 2, storedSessions: 2, subscriptions: 1, retainedMessages: 0, queuedMessages: 0, incomingMessages: 1, outgoingMessages: 1, rejectedConnections: 0, database: { healthy: true }, listeners: [], capabilities: [] };
    else if (url.pathname === '/api/v1/diagnostics' && request.method() === 'POST') {
      assert.equal(request.headers()['x-csrf-token'], 'fixture-only');
      const filter = request.postDataJSON(); assert.equal(filter.clientId, 'esp32-01'); assert.equal(filter.username, 'ivan');
      starts += 1; Object.assign(state, { id: `observer-${starts}`, active: true, filter, expiresAt: new Date(Date.now() + filter.seconds * 1000).toISOString(), messages: starts === 1 ? [message] : [], bytes: 2500 }); json = state;
    } else if (url.pathname === '/api/v1/diagnostics') json = state;
    else if (url.pathname.includes('/diagnostics/messages/')) json = { ...message, retain: true, payloadBytes: 16, metadataTruncated: false, events: ['received', 'session_locked', 'subscription_match', 'queue_prepared', 'committed', 'publish_sent', 'pubrec', 'pubrel_sent', 'pubcomp'].map((code, index) => ({ at: message.receivedAt, elapsedMs: index * 1.25, code, deliveryId: index > 2 ? 'delivery-one' : null, sessionId: null, clientId: index > 4 ? 'reader' : null, username: index > 4 ? 'alex' : null, packetId: index > 4 ? 9 : null, detail: null })) };
    else if (url.pathname.startsWith('/api/v1/diagnostics/') && request.method() === 'DELETE') { assert.equal(request.headers()['x-csrf-token'], 'fixture-only'); state.active = false; json = state; }
    else throw new Error(`Unexpected fixture endpoint ${url.pathname}`);
    await route.fulfill({ json, status: request.method() === 'POST' ? 201 : 200 });
  });
  await page.goto(`${baseURL}/admin#messages?tab=diagnostics&clientId=esp32-01&username=ivan&topic=building%2F%2B%2Fstatus`);
  await expect(page.getByLabel('Client ID', { exact: true })).toHaveValue('esp32-01');
  await expect(page.getByRole('button', { name: 'Запустить', exact: true })).toBeEnabled();
  const select = page.getByLabel('Длительность', { exact: true });
  assert((await select.boundingBox()).height >= 42);
  await page.getByRole('button', { name: 'Запустить', exact: true }).click();
  await expect(page.locator('.diagnostics-table tbody tr')).toHaveCount(1);
  await expect(page.getByLabel('Client ID', { exact: true })).toBeDisabled();
  state.filter = { ...state.filter, seconds: 180 };
  await expect(select).toHaveValue('180', { timeout: 4000 });
  await page.getByRole('button', { name: /^Путь сообщения / }).click();
  const dialog = page.getByRole('dialog', { name: 'Путь сообщения', exact: true });
  await expect(dialog).toContainText('PUBCOMP получателя · QoS 2 завершён');
  await expect(dialog).toContainText('alex / reader');
  assert.equal(await dialog.locator('.trace-timeline li').count(), 9);
  await mkdir('work/ui', { recursive: true });
  await page.screenshot({ animations: 'disabled', path: 'work/ui/diagnostics-desktop.png' });
  await page.setViewportSize({ width: 390, height: 844 });
  await page.waitForFunction(() => document.querySelector('.sidebar').getBoundingClientRect().right <= 1);
  assert(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth + 1), 'Mobile timeline overflow');
  await page.screenshot({ animations: 'disabled', path: 'work/ui/diagnostics-mobile.png' });
  await dialog.locator('.modal-footer').getByRole('button', { name: 'Закрыть', exact: true }).click();
  await page.getByRole('button', { name: 'Завершить сбор', exact: true }).click();
  await expect(page.locator('.diagnostics-table tbody tr')).toHaveCount(1);
  await page.getByRole('button', { name: 'Запустить', exact: true }).click();
  await expect(page.getByText('Ожидаем выбранные публикации', { exact: true })).toBeVisible();
  Object.assign(state, { active: false, instanceId: 'process-two', id: null, messages: [], filter: null });
  await expect(page.getByRole('button', { name: 'Запустить', exact: true })).toBeVisible({ timeout: 4000 });
  assert(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth + 1), 'Mobile form overflow');
  await page.screenshot({ animations: 'disabled', path: 'work/ui/diagnostics-form-mobile.png' });
  assert.deepEqual(errors, []);
  console.log(JSON.stringify({ ok: true, checks: ['prefilled filters', 'explicit select height', 'start with CSRF', 'source scope', 'active observer settings sync', 'recipient QoS2 timeline', 'stop preserves results', 'new observer clears history', 'process reset', 'mobile form and timeline'], screenshots: ['work/ui/diagnostics-desktop.png', 'work/ui/diagnostics-mobile.png', 'work/ui/diagnostics-form-mobile.png'] }));
} finally { await browser.close(); }
