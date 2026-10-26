// Actual UI assets with isolated API fixtures; runtime data stays untouched.
import assert from 'node:assert/strict';
import { mkdir } from 'node:fs/promises';
import { randomUUID } from 'node:crypto';
import { chromium, expect } from '@playwright/test';

const baseURL = process.env.CLEX_UI_BASE_URL || 'http://127.0.0.1:8085';
assert(['127.0.0.1', 'localhost', '[::1]'].includes(new URL(baseURL).hostname));
const browser = await chromium.launch({ headless: true });
let page;
try {
  page = await browser.newPage({ viewport: { width: 1440, height: 1000 }, reducedMotion: 'reduce', locale: 'ru-RU' });
  page.setDefaultTimeout(12000);
  const errors = [];
  page.on('pageerror', error => errors.push(error.message));
  page.on('console', message => { if (message.type() === 'error') errors.push(message.text()); });
  let bridges = [], savedPassword = false;
  await page.route('**/api/v1/**', async route => {
    const request = route.request(), url = new URL(request.url()), method = request.method();
    if (method !== 'GET') assert.equal(request.headers()['x-csrf-token'], 'fixture-only');
    let json, status = 200;
    if (url.pathname.endsWith('/session')) json = { username: 'fixture-admin', csrfToken: 'fixture-only' };
    else if (url.pathname.endsWith('/overview')) json = { version: 'fixture', uptimeSeconds: 60, activeConnections: 0, storedSessions: 0, subscriptions: 0, retainedMessages: 0, queuedMessages: 0, incomingMessages: 0, outgoingMessages: 0, rejectedConnections: 0, database: { healthy: true }, listeners: [], capabilities: [] };
    else if (url.pathname.endsWith('/bridges') && method === 'GET') json = { bridges, secretKeyReady: true, maxQueuedMessages: 10000, maxQueuedBytes: 67108864 };
    else if (url.pathname.endsWith('/bridges') && method === 'POST') {
      const body = request.postDataJSON();
      assert.equal(body.password, 'fixture-only-password'); savedPassword = true;
      assert.equal(body.rules.length, 2);
      assert.equal(body.rules[0].localPrefix, 'workshop/');
      assert.equal(body.rules[1].direction, 'in');
      const { password, clearPassword, ...settings } = body;
      bridges.push({ ...settings, id: randomUUID(), revision: 1, hasPassword: true, status: 'connected', connectedAt: new Date().toISOString(), queued: 12, sent: 318, received: 72, lastError: null, createdAt: new Date().toISOString() });
      json = { id: bridges.at(-1).id }; status = 201;
    } else if (url.pathname.includes('/bridges/')) {
      const id = url.pathname.split('/').at(-1);
      if (method === 'PATCH') {
        const body = request.postDataJSON(), bridge = bridges.find(link => link.id === id);
        assert.equal(body.revision, bridge.revision); assert(!('password' in body));
        Object.assign(bridge, body, { revision: body.revision + 1, status: body.enabled ? 'connected' : 'disabled' }); json = { updated: true };
      } else { assert.equal(method, 'DELETE'); bridges = bridges.filter(link => link.id !== id); json = { deleted: true }; }
    } else throw new Error(`Unexpected fixture endpoint ${method} ${url.pathname}`);
    return route.fulfill({ status, json });
  });
  await page.goto(`${baseURL}/admin#bridges`);
  await expect(page.getByRole('heading', { name: 'Соедините MQTT-брокеры' })).toBeVisible();
  await page.getByRole('button', { name: 'Добавить мост', exact: true }).click();
  let modal = page.getByRole('dialog', { name: 'Новый мост MQTT', exact: true });
  await modal.getByLabel('Название моста', { exact: true }).fill('Мастерская');
  await modal.getByLabel('Адрес MQTT-сервера', { exact: true }).fill('mqtts://broker.example.com:8883');
  await modal.getByLabel('MQTT-логин', { exact: true }).fill('workshop-link');
  await modal.getByLabel('MQTT-пароль', { exact: true }).fill('fixture-only-password');
  let rules = modal.locator('.bridge-rule-editor');
  await rules.nth(0).getByLabel('Префикс на этом брокере', { exact: true }).fill('workshop/');
  await rules.nth(0).getByLabel('Префикс удалённого брокера', { exact: true }).fill('sites/workshop/');
  await modal.getByRole('button', { name: 'Добавить правило', exact: true }).click();
  await expect(rules).toHaveCount(2);
  await rules.nth(1).getByLabel('Направление', { exact: true }).selectOption('in');
  await rules.nth(1).getByLabel('Фильтр топиков', { exact: true }).fill('commands/#');
  await rules.nth(1).getByLabel('Префикс на этом брокере', { exact: true }).fill('workshop/');
  await rules.nth(1).getByLabel('Префикс удалённого брокера', { exact: true }).fill('sites/workshop/');
  for (const select of await modal.locator('select').all()) assert((await select.boundingBox()).height >= 44);
  await mkdir('work/ui', { recursive: true });
  await page.screenshot({ path: 'work/ui/bridge-form-desktop.png' });
  await modal.getByRole('button', { name: 'Создать мост', exact: true }).click();
  await expect(page.locator('.bridge-card')).toContainText('Мастерская');
  assert(savedPassword);
  await expect(page.locator('.bridge-card')).toContainText('На связи');
  assert(!await page.locator('body').innerText().then(text => text.includes('fixture-only-password')));
  await page.getByRole('button', { name: 'Изменить мост Мастерская', exact: true }).click();
  modal = page.getByRole('dialog', { name: 'Настройки моста', exact: true });
  await expect(modal.getByLabel('Новый пароль', { exact: true })).toHaveValue('');
  await modal.getByLabel('Название моста', { exact: true }).fill('Мастерская и склад');
  await page.setViewportSize({ width: 390, height: 844 });
  await page.waitForFunction(() => document.querySelector('.sidebar').getBoundingClientRect().right <= 1);
  await page.waitForFunction(() => document.documentElement.scrollWidth <= innerWidth + 1);
  const box = await modal.boundingBox(); assert(box.y >= 0 && box.y + box.height <= 845);
  await page.screenshot({ path: 'work/ui/bridge-form-mobile.png' });
  await modal.getByRole('button', { name: 'Сохранить мост', exact: true }).click();
  await page.getByRole('button', { name: 'Отключить мост Мастерская и склад', exact: true }).click();
  await page.getByRole('dialog').getByRole('button', { name: 'Отключить мост', exact: true }).click();
  await expect(page.locator('.bridge-card')).toContainText('Отключён');
  await page.getByRole('button', { name: 'Включить мост Мастерская и склад', exact: true }).click();
  await page.getByRole('dialog').getByRole('button', { name: 'Включить мост', exact: true }).click();
  await expect(page.locator('.bridge-card')).toContainText('На связи');
  await page.getByPlaceholder('Найти мост').fill('отсутствующая площадка');
  await expect(page.getByRole('heading', { name: 'Уточните поиск' })).toBeVisible();
  await page.getByPlaceholder('Найти мост').fill('');
  await page.screenshot({ path: 'work/ui/bridges-mobile.png' });
  assert(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth + 1));
  await page.setViewportSize({ width: 1440, height: 1000 });
  await page.screenshot({ path: 'work/ui/bridges-desktop.png' });
  await page.getByRole('button', { name: 'Удалить мост Мастерская и склад', exact: true }).click();
  await expect(page.getByRole('dialog')).toContainText('12 сообщений');
  await page.getByRole('dialog').getByRole('button', { name: 'Удалить мост', exact: true }).click();
  await expect(page.getByRole('heading', { name: 'Соедините MQTT-брокеры' })).toBeVisible();
  assert.deepEqual(errors, []);
  console.log(JSON.stringify({ ok: true, checks: ['create with two directions', 'prefix preview', 'TLS CA form', 'preserve saved password', 'disable/enable', 'confirmed queue deletion', 'search', 'select height', 'desktop/mobile layout'] }, null, 2));
} catch (error) {
  await mkdir('work/ui', { recursive: true });
  await page?.screenshot({ path: 'work/ui/bridges-ui-failure.png' });
  console.error(await page?.locator('[role="dialog"]').innerText().catch(() => 'Dialog closed'));
  throw error;
} finally { await browser.close(); }
