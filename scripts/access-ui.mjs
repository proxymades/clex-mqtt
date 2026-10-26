// Exercise access-management screens with isolated API fixtures.
import assert from 'node:assert/strict';
import { mkdir } from 'node:fs/promises';
import { randomUUID } from 'node:crypto';
import { chromium, expect } from '@playwright/test';

const baseURL = process.env.CLEX_UI_BASE_URL || 'http://127.0.0.1:8085';
const target = new URL(baseURL);
assert(['127.0.0.1', 'localhost', '[::1]'].includes(target.hostname) && !target.username && !target.password);
const browser = await chromium.launch({ headless: true });
try {
  const page = await browser.newPage({ viewport: { width: 1440, height: 1000 }, reducedMotion: 'reduce' });
  const errors = [];
  page.on('pageerror', (error) => errors.push(error.message));
  page.on('console', (message) => { if (message.type() === 'error') errors.push(message.text()); });
  let groups = [];
  const users = [{ username: 'sensor-01', enabled: true, createdAt: new Date().toISOString(), publishFilters: [], subscribeFilters: [], groups: [] }];
  await page.route('**/api/v1/**', async (route) => {
    const request = route.request();
    const url = new URL(request.url());
    const method = request.method();
    let json;
    let status = 200;
    if (url.pathname.endsWith('/session')) json = { username: 'fixture-admin', csrfToken: 'fixture-only' };
    else if (url.pathname.endsWith('/overview')) json = { version: 'fixture', uptimeSeconds: 60, activeConnections: 0, storedSessions: 0,
      subscriptions: 0, retainedMessages: 0, queuedMessages: 0, incomingMessages: 0, outgoingMessages: 0, rejectedConnections: 0,
      database: { healthy: true }, listeners: [], capabilities: [] };
    else {
      if (method !== 'GET') assert.equal(request.headers()['x-csrf-token'], 'fixture-only');
      const body = method === 'GET' || method === 'DELETE' ? null : request.postDataJSON();
      if (url.pathname.endsWith('/groups') && method === 'GET') json = groups;
      else if (url.pathname.endsWith('/groups') && method === 'POST') {
        groups.push({ id: randomUUID(), enabled: true, createdAt: new Date().toISOString(), memberCount: 0, ...body });
        json = { id: groups.at(-1).id }; status = 201;
      } else if (url.pathname.includes('/groups/')) {
        const id = url.pathname.split('/').at(-1);
        const group = groups.find((item) => item.id === id);
        assert(group, 'Selected group must exist');
        if (method === 'PATCH') { Object.assign(group, body); json = { id }; }
        else if (method === 'DELETE') {
          groups = groups.filter((item) => item.id !== id);
          for (const user of users) user.groups = user.groups.filter((item) => item.id !== id);
          return route.fulfill({ status: 204 });
        }
      } else if (url.pathname.endsWith('/users') && method === 'GET') json = users;
      else if (url.pathname.endsWith('/users') && method === 'POST') {
        const assigned = groups.filter((group) => body.groupIds.includes(group.id));
        assert.equal(assigned.length, 1);
        for (const group of assigned) group.memberCount += 1;
        users.push({ username: body.username, enabled: true, createdAt: new Date().toISOString(),
          publishFilters: body.publishFilters, subscribeFilters: body.subscribeFilters,
          groups: assigned.map(({ id, name, enabled }) => ({ id, name, enabled })) });
        json = { username: body.username }; status = 201;
      } else if (url.pathname.endsWith('/access/check')) {
        const group = groups[0];
        const forbidden = body.topic.includes('/private/');
        json = { ...body, decision: { allowed: !forbidden, reason: forbidden ? 'explicit_deny' : 'rule_allowed', matchingTotal: 1,
          matchingRules: [{ id: randomUUID(), filter: forbidden ? 'workshop/private/#' : group.publishFilters[0], allow: !forbidden,
            source: 'group', groupId: group.id, groupName: group.name, coversRequest: true, overlapsRequest: true }] } };
      } else throw new Error(`Unexpected access fixture endpoint ${method} ${url.pathname}`);
    }
    await route.fulfill({ status, json });
  });
  await page.goto(`${baseURL}/admin#users?tab=groups`);
  await page.getByRole('button', { name: 'Добавить группу', exact: true }).click();
  const groupDialog = page.getByRole('dialog', { name: 'Новая группа доступа' });
  await groupDialog.getByLabel('Название группы', { exact: true }).fill('Мастерская 1');
  await groupDialog.getByRole('button', { name: 'Применить шаблон', exact: true }).click();
  await expect(groupDialog.getByRole('alert')).toContainText('базовый топик');
  await groupDialog.getByLabel('Базовый топик', { exact: true }).fill('workshop/line-1');
  await groupDialog.getByRole('button', { name: 'Применить шаблон', exact: true }).click();
  await expect(groupDialog.getByLabel('Разрешить публикацию', { exact: true })).toHaveValue('workshop/line-1/state/#\nworkshop/line-1/events/#');
  await expect(groupDialog.getByLabel('Разрешить подписку', { exact: true })).toHaveValue('workshop/line-1/commands/#');
  await groupDialog.locator('summary').click();
  await groupDialog.getByLabel('Запретить публикацию', { exact: true }).fill('workshop/private/#');
  await groupDialog.getByRole('button', { name: 'Создать группу', exact: true }).click();
  await expect(page.locator('.groups-table')).toContainText('Мастерская 1');
  await page.getByRole('navigation', { name: 'Вкладки доступа' }).getByRole('link', { name: 'Учётные записи', exact: true }).click();
  await page.getByRole('button', { name: 'Добавить учётную запись', exact: true }).first().click();
  const userDialog = page.getByRole('dialog', { name: 'Новая учётная запись MQTT' });
  await userDialog.getByLabel('MQTT-логин', { exact: true }).fill('sensor-02');
  await userDialog.getByLabel('Пароль', { exact: true }).fill('fixture-only-password');
  await userDialog.getByLabel('Найти группу доступа', { exact: true }).fill('Мастерская');
  await userDialog.getByRole('checkbox', { name: /Мастерская 1/ }).check();
  await userDialog.getByRole('button', { name: 'Создать учётную запись', exact: true }).click();
  await expect(page.locator('.users-table')).toContainText('sensor-02');
  await page.getByRole('combobox', { name: 'Группа учётных записей', exact: true }).selectOption(groups[0].id);
  await expect(page.locator('.users-table tbody tr')).toHaveCount(1);
  await page.getByRole('link', { name: 'Проверить доступ sensor-02', exact: true }).click();
  await expect(page.getByLabel('MQTT-логин', { exact: true })).toHaveValue('sensor-02');
  await page.getByLabel('Топик', { exact: true }).fill('workshop/line-1/state/value');
  await page.getByRole('button', { name: 'Проверить доступ', exact: true }).click();
  await expect(page.getByRole('heading', { name: 'Доступ разрешён', exact: true })).toBeVisible();
  await expect(page.locator('.access-rules-table')).toContainText('Мастерская 1');
  await mkdir('work/ui', { recursive: true });
  await page.screenshot({ path: 'work/ui/access-desktop.png' });
  await page.getByLabel('Топик', { exact: true }).fill('workshop/private/value');
  await page.getByRole('button', { name: 'Проверить доступ', exact: true }).click();
  await expect(page.getByRole('heading', { name: 'Доступ закрыт', exact: true })).toBeVisible();
  await page.goto(`${baseURL}/admin#users?tab=groups`);
  await page.getByRole('button', { name: 'Отключить группу Мастерская 1', exact: true }).click();
  await page.getByRole('dialog').getByRole('button', { name: 'Отключить правила', exact: true }).click();
  await expect(page.locator('.groups-table')).toContainText('Отключена');
  await page.setViewportSize({ width: 390, height: 844 });
  await page.waitForFunction(() => document.querySelector('.sidebar').getBoundingClientRect().right <= 1);
  await page.getByRole('button', { name: 'Изменить группу Мастерская 1', exact: true }).click();
  await expect(page.getByRole('dialog', { name: 'Права группы' })).toBeVisible();
  await expect(page.getByRole('dialog', { name: 'Права группы' })).toHaveCSS('opacity', '1');
  const modalBox = await page.getByRole('dialog', { name: 'Права группы' }).boundingBox();
  assert(modalBox && modalBox.y >= 0 && modalBox.y + modalBox.height <= 845, 'Mobile group form must fit vertically');
  assert(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth + 1), 'Mobile group form overflow');
  await page.screenshot({ path: 'work/ui/access-mobile.png' });
  await page.getByRole('dialog').getByRole('button', { name: 'Отмена', exact: true }).click();
  await page.getByRole('button', { name: 'Удалить группу Мастерская 1', exact: true }).click();
  await page.getByRole('dialog').getByRole('button', { name: 'Удалить группу', exact: true }).click();
  await expect(page.getByRole('heading', { name: 'Создайте группу доступа', exact: true })).toBeVisible();
  assert.deepEqual(users[1].groups, []);
  assert.deepEqual(errors, []);
  console.log(JSON.stringify({ ok: true, checks: ['group create', 'role template', 'deny editor', 'membership assignment', 'user group filter', 'access inspector allow/deny', 'group disable', 'group delete', 'mobile form'], screenshots: ['work/ui/access-desktop.png', 'work/ui/access-mobile.png'] }, null, 2));
} finally { await browser.close(); }
