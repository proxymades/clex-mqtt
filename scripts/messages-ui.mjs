// Exercise the served console with isolated API fixtures and many message sources.
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
  let instance = 'first-process';
  let requests = 0;
  let events = [];
  const clients = Array.from({ length: 24 }, (_, index) => ({
    sessionId: `session-${index}`, clientId: `client-${String(index + 1).padStart(2, '0')}`, username: `tenant-${index % 4 + 1}`,
    connected: true, connectedAt: new Date().toISOString(), lastSeen: new Date().toISOString(),
    subscriptions: 2, queued: 0, inflight: 0, remoteAddress: `192.0.2.${index + 1}:1234`,
    protocol: '5.0', keepAliveSeconds: 30, expiresAt: null,
  }));
  clients[23].clientId = clients[0].clientId;
  const append = (client, value) => events.push({ id: (events.at(-1)?.id ?? 0) + 1, receivedAt: new Date().toISOString(),
    clientId: client.clientId, username: client.username, topic: `building/${client.clientId}/temperature`,
    qos: 1, retain: false, will: false, payloadBase64: Buffer.from(JSON.stringify({ value })).toString('base64'),
    payloadBytes: 12, payloadTruncated: false });
  for (const client of clients) for (let value = 0; value < 4; value += 1) append(client, value);
  await page.route('**/api/v1/**', async (route) => {
    const url = new URL(route.request().url());
    let json;
    if (url.pathname.endsWith('/session')) json = { username: 'fixture-admin', csrfToken: 'fixture-only' };
    else if (url.pathname.endsWith('/overview')) json = { version: 'fixture', uptimeSeconds: 60, activeConnections: clients.length,
      storedSessions: clients.length, subscriptions: 48, retainedMessages: 1, queuedMessages: 0,
      incomingMessages: 96, outgoingMessages: 96, rejectedConnections: 0, database: { healthy: true }, listeners: [], capabilities: [] };
    else if (url.pathname.endsWith('/clients')) json = clients;
    else if (url.pathname.endsWith('/retained')) json = [{ topic: 'building/state', payloadBase64: 'b25saW5l', payloadBytes: 6,
      payloadTruncated: false, qos: 1, updatedAt: new Date().toISOString() }];
    else if (url.pathname.endsWith('/messages')) {
      requests += 1;
      const after = url.searchParams.has('after') ? Number(url.searchParams.get('after')) : null;
      const matching = events.filter((event) =>
        (!url.searchParams.get('clientId') || event.clientId === url.searchParams.get('clientId')) &&
        (!url.searchParams.get('username') || event.username === url.searchParams.get('username')) &&
        (!url.searchParams.get('topic') || url.searchParams.get('topic') === 'building/+/temperature' || event.topic === url.searchParams.get('topic')));
      const messages = after === null ? matching.slice(-100) : matching.filter((event) => event.id > after).slice(0, 100);
      const cursor = after === null || messages.length < 100 ? events.at(-1)?.id ?? 0 : messages.at(-1).id;
      json = { instanceId: instance, cursor, lost: 0, hasMore: cursor < (events.at(-1)?.id ?? 0), capacity: 1000, messages };
    } else throw new Error(`Unexpected fixture endpoint ${url.pathname}`);
    await route.fulfill({ json });
  });
  await page.goto(`${baseURL}/admin#messages`);
  await expect(page.locator('.messages-table tbody tr')).toHaveCount(96);
  await page.getByLabel('Client ID', { exact: true }).fill('client-07');
  await page.getByLabel('MQTT-логин', { exact: true }).fill('tenant-3');
  await page.getByLabel('Топик или фильтр', { exact: true }).fill('building/+/temperature');
  await page.getByRole('button', { name: 'Применить', exact: true }).click();
  await expect(page.locator('.messages-table tbody tr')).toHaveCount(4);
  assert((await page.locator('.message-source strong').allTextContents()).every((value) => value === 'client-07'));
  await page.getByRole('button', { name: 'Пауза', exact: true }).click();
  const pausedRequests = requests;
  append(clients[6], 42);
  await page.waitForTimeout(2300);
  assert.equal(requests, pausedRequests, 'Paused view must stop polling');
  await page.getByRole('button', { name: 'Продолжить', exact: true }).click();
  await expect(page.locator('.messages-table tbody tr')).toHaveCount(5);
  await page.getByRole('button', { name: /^Открыть сообщение / }).first().click();
  await expect(page.getByRole('dialog', { name: 'Сообщение MQTT' })).toContainText('42');
  await page.getByRole('dialog').locator('.modal-footer').getByRole('button', { name: 'Закрыть', exact: true }).click();
  await mkdir('work/ui', { recursive: true });
  await page.screenshot({ path: 'work/ui/messages-desktop.png' });
  for (let value = 0; value < 650; value += 1) append(clients[6], value);
  await expect(page.locator('.messages-table tbody tr')).toHaveCount(500, { timeout: 7000 });
  await page.getByRole('button', { name: 'Очистить список на экране', exact: true }).click();
  await expect(page.locator('.messages-table tbody tr')).toHaveCount(0);
  await page.waitForTimeout(2200);
  await expect(page.locator('.messages-table tbody tr')).toHaveCount(0);
  instance = 'restarted-process';
  events = [];
  append(clients[6], 'after restart');
  await expect(page.locator('.messages-table tbody tr')).toHaveCount(1, { timeout: 4000 });
  await expect(page.locator('.messages-table')).toContainText('after restart');
  await page.getByRole('navigation', { name: 'Вкладки сообщений' }).getByRole('link', { name: 'Retain', exact: true }).click();
  await expect(page.getByRole('heading', { name: 'Retain', exact: true, level: 2 })).toBeVisible();
  await expect(page.locator('.retained-table')).toContainText('online');
  await page.goto(`${baseURL}/admin#retained`);
  await expect(page.getByRole('heading', { name: 'Retain', exact: true, level: 2 })).toBeVisible();
  await page.goto(`${baseURL}/admin#clients`);
  await page.locator('.client-identity').first().click();
  await expect(page.getByRole('dialog')).toContainText('30 с · ожидание пакета до 45 с');
  await page.getByRole('link', { name: 'Сообщения клиента', exact: true }).click();
  await expect(page.getByLabel('Client ID', { exact: true })).toHaveValue('client-01');
  await expect(page.getByLabel('MQTT-логин', { exact: true })).toHaveValue('tenant-1');
  await page.goto(`${baseURL}/admin#clients`);
  await page.locator('.client-identity').last().click();
  await expect(page.getByRole('dialog')).toContainText('tenant-4');
  await page.getByRole('link', { name: 'Сообщения клиента', exact: true }).click();
  await expect(page.getByLabel('Client ID', { exact: true })).toHaveValue('client-01');
  await expect(page.getByLabel('MQTT-логин', { exact: true })).toHaveValue('tenant-4');
  await page.setViewportSize({ width: 390, height: 844 });
  await page.waitForFunction(() => document.querySelector('.sidebar').getBoundingClientRect().right <= 1);
  assert(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth + 1), 'Mobile document overflow');
  await page.screenshot({ path: 'work/ui/messages-mobile.png' });
  assert.deepEqual(errors, []);
  console.log(JSON.stringify({ ok: true, clients: clients.length, checks: ['source/user/topic filters', 'pause/resume', 'payload detail', '500 event bound', 'clear view', 'process restart', 'Retain tab', 'legacy link', 'client drilldown', 'mobile layout'], screenshots: ['work/ui/messages-desktop.png', 'work/ui/messages-mobile.png'] }, null, 2));
} finally { await browser.close(); }
