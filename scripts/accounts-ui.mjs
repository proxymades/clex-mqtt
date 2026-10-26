// Real account/browser/MQTT checks against scripts/test_accounts_ui.py's fixture.
import assert from 'node:assert/strict';
import { writeFile } from 'node:fs/promises';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { randomUUID } from 'node:crypto';
import { once } from 'node:events';
import { chromium, expect } from '@playwright/test';
import mqtt from 'mqtt';

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const base = process.env.CLEX_UI_BASE_URL;
const mqttURL = process.env.CLEX_UI_MQTT_URL;
const output = process.env.CLEX_ACCOUNT_UI_OUTPUT;
const password = process.env.CLEX_ACCOUNT_TEST_PASSWORD;
const adminPassword = process.env.CLEX_UI_TEST_PASSWORD;
assert(base && mqttURL && output && password && adminPassword);
for (const url of [new URL(base), new URL(mqttURL)]) assert(['127.0.0.1', 'localhost'].includes(url.hostname) && !url.username && !url.password);
assert(path.resolve(process.env.PLAYWRIGHT_BROWSERS_PATH || '').startsWith(path.join(root, '.cache') + path.sep));
assert(path.resolve(output).startsWith(path.join(root, 'work', 'accounts-ui') + path.sep));
const browser = await chromium.launch({ headless: true });
const clients = [];
const errors = [];
const names = ['alice-' + randomUUID().slice(0, 8), 'bob-' + randomUUID().slice(0, 8)];
async function checkPublicEntryFallback() {
  for (const unavailable of [false, true]) {
    const context = await browser.newContext();
    try {
      await context.route('**/api/v1/account/options', (route) => unavailable ? route.abort() : route.fulfill({ json: { registrationEnabled: false, csrfToken: 'fixture-only' } }));
      const page = await context.newPage();
      await page.goto(base);
      await expect(page.getByRole('link', { name: 'Войти в личный кабинет', exact: true })).toBeVisible();
      await expect(page.getByRole('link', { name: 'Создать аккаунт', exact: true })).toHaveCount(0);
      await expect(page.locator('a[href^="/admin"]')).toHaveCount(0);
      if (!unavailable) {
        await page.goto(base + '/account/register');
        await expect(page.getByText('Самостоятельная регистрация отключена.', { exact: true })).toBeVisible();
        await expect(page.getByRole('heading', { name: 'Ваши устройства на связи', exact: true })).toBeVisible();
        await expect(page.getByLabel('Повторите пароль', { exact: true })).toHaveCount(0);
      }
    } finally { await context.close(); }
  }
}
async function expectLocalNavigation(page, action) {
  const documents = [];
  const track = (request) => { if (request.isNavigationRequest() && request.frame() === page.mainFrame()) documents.push(request.url()); };
  page.on('request', track);
  try { await action(); assert.deepEqual(documents, [], 'section logo reloaded the document'); }
  finally { page.off('request', track); }
}
async function checkDeviceFormSpacing(dialog) {
  await expect.poll(() => dialog.evaluate((element) => getComputedStyle(element).opacity)).toBe('1');
  await expect(dialog).toBeInViewport({ ratio: .9 });
  const fields = await dialog.locator('.field').all();
  assert.equal(fields.length, 4);
  const bounds = await Promise.all(fields.map((field) => field.boundingBox()));
  assert(bounds.every(Boolean));
  for (let index = 1; index < bounds.length; index++) {
    assert(bounds[index].y - (bounds[index - 1].y + bounds[index - 1].height) >= 16, 'device form fields touch vertically');
  }
}
async function checkDeviceActions(page) {
  const row = page.getByRole('row').filter({ has: page.getByText('esp32-01', { exact: true }) });
  const rowBounds = await row.boundingBox();
  assert(rowBounds && rowBounds.height < 180, 'MQTT login collapsed into an unreadable vertical column');
  const actions = row.locator('.action-group');
  await actions.scrollIntoViewIfNeeded();
  const buttons = await actions.getByRole('button').all();
  assert.equal(buttons.length, 3);
  const bounds = await Promise.all(buttons.map((button) => button.boundingBox()));
  assert(bounds.every(Boolean));
  for (let index = 0; index < bounds.length; index++) {
    assert(Math.abs(bounds[index].y - bounds[0].y) < 1, 'device actions wrapped onto different lines');
    if (index > 0) assert(bounds[index].x >= bounds[index - 1].x + bounds[index - 1].width, 'device actions overlap');
  }
}
async function checkAdminFormSpacing(page, name, open) {
  await open();
  const dialog = page.getByRole('dialog');
  await expect.poll(() => dialog.evaluate((el) => getComputedStyle(el).opacity)).toBe('1');
  for (const width of [1440, 390]) {
    await page.setViewportSize({ width, height: width === 1440 ? 1000 : 844 });
    await page.evaluate(() => new Promise((resolve) => requestAnimationFrame(() => requestAnimationFrame(resolve))));
    await expect.poll(() => page.locator('.main-shell').evaluate((el) => parseFloat(getComputedStyle(el).marginLeft))).toBe(width <= 760 ? 0 : 256);
    const fields = await dialog.locator('label.field').all();
    assert(fields.length > 0);
    const boxes = await Promise.all(fields.map((field) => field.boundingBox()));
    assert(boxes.every(Boolean));
    for (const a of boxes) for (const b of boxes) {
      const overlap = Math.min(a.x + a.width, b.x + b.width) - Math.max(a.x, b.x);
      if (b.y >= a.y + a.height - 1 && overlap > Math.min(a.width, b.width) / 2) assert(b.y - a.y - a.height >= 12, name + ' fields touch vertically');
    }
    await noOverflow(page);
    await page.screenshot({ path: path.join(output, name + '-' + width + '.png'), animations: 'disabled' });
    await fields.at(-1).scrollIntoViewIfNeeded();
    await page.screenshot({ path: path.join(output, name + '-bottom-' + width + '.png'), animations: 'disabled' });
  }
  await dialog.locator('.modal-header').getByRole('button', { name: 'Закрыть', exact: true }).click();
  await page.setViewportSize({ width: 1440, height: 1000 });
}
async function copyLoginWithoutSaving(page, dialog, expected) {
  const changes = [];
  const track = (request) => {
    if (new URL(request.url()).pathname.startsWith('/api/v1/account/') && request.method() !== 'GET') changes.push(request.method());
  };
  page.on('request', track);
  try {
    await dialog.getByRole('button', { name: 'Скопировать MQTT-логин', exact: true }).click();
    assert.equal(await page.evaluate(() => navigator.clipboard.readText()), expected);
    await page.evaluate(() => new Promise((resolve) => requestAnimationFrame(() => requestAnimationFrame(resolve))));
    await expect(dialog).toBeVisible();
    assert.deepEqual(changes, [], 'copying the login submitted the account form');
  } finally { page.off('request', track); }
}
async function account(name) {
  const context = await browser.newContext({ viewport: { width: 1440, height: 1000 }, reducedMotion: 'reduce', permissions: ['clipboard-read', 'clipboard-write'] });
  const page = await context.newPage();
  page.on('pageerror', (e) => errors.push(e.message));
  page.on('response', (response) => { if (response.status() >= 500) errors.push('Server HTTP ' + response.status()); });
  const adminSessionRequests = [];
  page.on('request', (request) => { if (new URL(request.url()).pathname === '/api/v1/session') adminSessionRequests.push(request.url()); });
  await page.goto(base);
  await expect(page.getByRole('heading', { name: 'MQTT-брокер для ваших устройств', exact: true })).toBeVisible();
  assert.equal(adminSessionRequests.length, 0, 'public entry requested an administrator session');
  await expect(page.getByRole('link', { name: 'Войти в личный кабинет', exact: true })).toHaveAttribute('href', '/account');
  await expect(page.locator('a[href^="/admin"]')).toHaveCount(0);
  await expect(page.getByRole('link', { name: 'Создать аккаунт', exact: true })).toHaveAttribute('href', '/account/register');
  assert.equal((await context.cookies()).filter((cookie) => cookie.name === 'clex_account_preauth').length, 0, 'public entry changed the login CSRF cookie');
  if (name === names[0]) {
    await noOverflow(page);
    await page.screenshot({ path: path.join(output, 'entry-desktop.png'), fullPage: true });
    await page.setViewportSize({ width: 390, height: 844 });
    await noOverflow(page);
    await page.screenshot({ path: path.join(output, 'entry-mobile.png'), fullPage: true });
    await page.setViewportSize({ width: 1440, height: 1000 });
    await page.goto(base + '/admin');
    await expect(page).toHaveURL(base + '/admin');
    await expect(page.getByRole('heading', { name: 'Вход администратора', exact: true })).toBeVisible();
    await page.getByRole('link', { name: 'На главную', exact: true }).click();
  }
  if (name === names[0]) {
    await page.getByRole('link', { name: 'Создать аккаунт', exact: true }).click();
    await expect(page).toHaveURL(base + '/account/register');
    await page.reload();
  } else {
    await page.getByRole('link', { name: 'Войти в личный кабинет', exact: true }).click();
    await expect(page).toHaveURL(base + '/account');
    await page.getByRole('button', { name: 'Создать аккаунт', exact: true }).click();
  }
  await expect(page.getByRole('heading', { name: 'Создать аккаунт', exact: true })).toBeVisible();
  await expect(page.locator('.auth-heading .eyebrow')).toHaveCount(0);
  if (name === names[0]) {
    await page.screenshot({ path: path.join(output, 'registration-desktop.png'), fullPage: true });
    await page.setViewportSize({ width: 390, height: 844 });
    await noOverflow(page);
    await page.screenshot({ path: path.join(output, 'registration-mobile.png'), fullPage: true });
    await page.setViewportSize({ width: 1440, height: 1000 });
  }

  await page.getByLabel('Логин', { exact: true }).fill(name);
  await page.getByLabel('Пароль', { exact: true }).fill(password);
  await page.getByLabel('Повторите пароль', { exact: true }).fill(password);
  await page.getByRole('button', { name: 'Создать аккаунт', exact: true }).click();
  await expect(page.getByRole('heading', { name: 'MQTT-доступ' })).toBeVisible();
  await expect(page).toHaveURL(base + '/account');
  await page.getByRole('button', { name: 'Добавить учётную запись', exact: true }).click();
  const dialog = page.getByRole('dialog');
  await dialog.getByLabel('Название учётной записи', { exact: true }).fill('esp32-01');
  await expect(dialog.getByText(name + '.esp32-01', { exact: true })).toBeVisible();
  await dialog.getByLabel(/^MQTT-пароль/).fill(password);
  await copyLoginWithoutSaving(page, dialog, name + '.esp32-01');
  await checkDeviceFormSpacing(dialog);
  await dialog.getByRole('button', { name: 'Создать учётную запись', exact: true }).click();
  await expect(dialog).toHaveCount(0);
  await expect(page.locator('.account-mqtt-users-table').getByText('esp32-01', { exact: true })).toBeVisible();
  const session = await (await page.request.get(base + '/api/v1/account/session')).json();
  const devices = await (await page.request.get(base + '/api/v1/account/users')).json();
  assert.deepEqual(devices[0].publishFilters, [session.topicPrefix + '#']);
  assert.deepEqual(devices[0].subscribeFilters, [session.topicPrefix + '#']);
  assert.equal(devices.length, 1);
  assert.equal(session.topicPrefix, 'accounts/' + name + '/');
  assert.equal(devices[0].username, name + '.esp32-01');
  for (const [label, value] of [['Скопировать адрес брокера', session.connection.host], ['Скопировать префикс топиков', session.topicPrefix], ['Скопировать пример топика', session.topicPrefix + 'esp32/ask'], ['Скопировать MQTT-логин esp32-01', devices[0].username]]) {
    await page.getByRole('button', { name: label, exact: true }).click();
    assert.equal(await page.evaluate(() => navigator.clipboard.readText()), value);
  }

  assert.equal((await page.request.get(base + '/api/v1/accounts')).status(), 401);
  await checkDeviceActions(page);
  if (name === names[0]) {
    await page.getByRole('button', { name: 'Отключить учётную запись esp32-01', exact: true }).click();
    await expect(page.getByRole('button', { name: 'Включить учётную запись esp32-01', exact: true })).toBeVisible();
    assert.equal((await (await page.request.get(base + '/api/v1/account/users')).json())[0].enabled, false);
    await page.getByRole('button', { name: 'Включить учётную запись esp32-01', exact: true }).click();
    await expect(page.getByRole('button', { name: 'Отключить учётную запись esp32-01', exact: true })).toBeVisible();
    assert.equal((await (await page.request.get(base + '/api/v1/account/users')).json())[0].enabled, true);
  }
  assert.equal(await page.evaluate(() => localStorage.length), 0);
  await page.getByRole('button', { name: 'Настройки', exact: true }).click();
  await expect(page.getByRole('heading', { name: 'Пароль аккаунта', exact: true })).toBeVisible();
  await expect(page.getByRole('link', { name: 'CLEX MQTT – MQTT-доступ', exact: true })).toHaveAttribute('href', '/account');
  await expectLocalNavigation(page, async () => {
    await page.getByRole('link', { name: 'CLEX MQTT – MQTT-доступ', exact: true }).click();
    await expect(page.getByRole('button', { name: 'Добавить учётную запись', exact: true })).toBeVisible();
    await expect(page).toHaveURL(base + '/account');
  });
  await page.goto(base);
  await expect(page.getByRole('heading', { name: 'MQTT-брокер для ваших устройств', exact: true })).toBeVisible();
  await page.getByRole('link', { name: 'Войти в личный кабинет', exact: true }).click();
  await expect(page.getByRole('heading', { name: 'MQTT-доступ', exact: true })).toBeVisible();
  return { context, page, session, device: devices[0] };
}
async function connect(account) {
  const client = await mqtt.connectAsync(mqttURL, { protocolVersion: 5, clientId: 'ui-' + randomUUID(), username: account.device.username, password, reconnectPeriod: 0, connectTimeout: 5000 });
  client.on('error', () => {});
  clients.push(client);
  return client;
}
async function noOverflow(page) {
  const fits = await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth + 1);
  if (!fits) {
    console.log('OVERFLOW', JSON.stringify(await page.evaluate(() => ({ viewport: innerWidth, width: document.documentElement.scrollWidth, elements: [...document.querySelectorAll('body *')].filter((el) => el.getBoundingClientRect().right > innerWidth + 1).slice(0, 16).map((el) => ({ tag: el.tagName, class: el.className, x: el.getBoundingClientRect().x, width: el.getBoundingClientRect().width })) }))));
    await page.screenshot({ path: path.join(output, 'overflow.png'), animations: 'disabled' });
  }
  assert(fits, 'interface has horizontal page overflow');
}
try {
  await checkPublicEntryFallback();
  const alice = await account(names[0]);
  const bob = await account(names[1]);
  assert.notEqual(alice.device.username, bob.device.username);
  assert.notEqual(alice.session.topicPrefix, bob.session.topicPrefix);
  let ac = await connect(alice);
  const bc = await connect(bob);
  const shared = await connect(alice);
  assert(ac.connected && shared.connected, 'one MQTT login did not permit two Client IDs');
  const sharedTopic = alice.session.topicPrefix + 'shared-client/check';
  const sharedMessage = once(shared, 'message');
  await shared.subscribeAsync(sharedTopic, { qos: 1 });
  await shared.publishAsync(sharedTopic, 'shared-credentials', { qos: 1 });
  const [sharedReceived] = await sharedMessage;
  assert.equal(sharedReceived, sharedTopic);
  const concurrent = await (await alice.page.request.get(base + '/api/v1/account/clients')).json();
  assert.equal(concurrent.filter((connection) => connection.username === alice.device.username && connection.connected).length, 2);
  await shared.endAsync(true);

  const seenA = [], seenB = [];
  ac.on('message', (topic) => seenA.push(topic));
  bc.on('message', (topic) => seenB.push(topic));
  await ac.subscribeAsync(alice.session.topicPrefix + '#', { qos: 1 });
  await bc.subscribeAsync(bob.session.topicPrefix + '#', { qos: 1 });
  await ac.publishAsync(alice.session.topicPrefix + 'sensor/value', 'alice-temperature-25', { qos: 1, retain: true });
  await bc.publishAsync(bob.session.topicPrefix + 'sensor/value', 'bob-temperature-99', { qos: 1, retain: true });
  await expect.poll(() => seenA.length).toBe(1);
  await expect.poll(() => seenB.length).toBe(1);
  assert(seenA.every((t) => t.startsWith(alice.session.topicPrefix)));
  assert(seenB.every((t) => t.startsWith(bob.session.topicPrefix)));
  await assert.rejects(ac.publishAsync(bob.session.topicPrefix + 'forbidden', 'blocked', { qos: 1 }));
  const foreign = await alice.page.request.patch(base + '/api/v1/account/users/' + bob.device.id, { headers: { Origin: base, 'X-CSRF-Token': alice.session.csrfToken }, data: { enabled: false } });
  assert.equal(foreign.status(), 404);

  await alice.page.bringToFront();
  await alice.page.getByRole('button', { name: 'Сообщения', exact: true }).click();
  await expect(alice.page.getByText('alice-temperature-25', { exact: true })).toBeVisible();
  await expect(alice.page.getByText('bob-temperature-99', { exact: true })).toHaveCount(0);
  await ac.publishAsync(alice.session.topicPrefix + 'sensor/newest', 'alice-newest-message', { qos: 1 });
  await expect(alice.page.locator('.messages-table tbody tr').first()).toContainText('alice-newest-message');
  await alice.page.getByRole('button', { name: /^Открыть сообщение / }).first().click();
  const messageDialog = alice.page.getByRole('dialog', { name: 'Сообщение MQTT' });
  await expect(messageDialog).toContainText(alice.device.username);
  await messageDialog.getByRole('button', { name: 'Скопировать топик', exact: true }).click();
  assert.equal(await alice.page.evaluate(() => navigator.clipboard.readText()), alice.session.topicPrefix + 'sensor/newest');
  await messageDialog.getByRole('button', { name: 'Скопировать', exact: true }).click();
  assert.equal(await alice.page.evaluate(() => navigator.clipboard.readText()), 'alice-newest-message');
  await messageDialog.locator('.modal-footer').getByRole('button', { name: 'Закрыть', exact: true }).click();
  await alice.page.getByPlaceholder('Найти в топиках и содержимом').fill('alice-newest-message');
  await expect(alice.page.locator('.messages-table tbody tr')).toHaveCount(1);
  await alice.page.getByPlaceholder('Найти в топиках и содержимом').fill('');
  const beforePause = await alice.page.locator('.messages-table tbody tr').count();
  let personalPolls = 0;
  const countPoll = (request) => { if (new URL(request.url()).pathname === '/api/v1/account/messages') personalPolls++; };
  alice.page.on('request', countPoll);
  await alice.page.getByRole('button', { name: 'Пауза', exact: true }).click();
  const pausedPolls = personalPolls;
  await ac.publishAsync(alice.session.topicPrefix + 'sensor/during-pause', 'alice-during-pause', { qos: 1 });
  await alice.page.waitForTimeout(2300);
  assert.equal(personalPolls, pausedPolls, 'personal paused viewer kept polling');
  await expect(alice.page.locator('.messages-table tbody tr')).toHaveCount(beforePause);
  await alice.page.getByRole('button', { name: 'Продолжить', exact: true }).click();
  await expect(alice.page.locator('.messages-table tbody tr').first()).toContainText('alice-during-pause');
  await alice.page.getByRole('button', { name: 'Очистить список на экране', exact: true }).click();
  await expect(alice.page.locator('.messages-table tbody tr')).toHaveCount(0);
  await alice.page.waitForTimeout(2200);
  await expect(alice.page.locator('.messages-table tbody tr')).toHaveCount(0);
  const bufferAfterClear = await (await alice.page.request.get(base + '/api/v1/account/messages')).json();
  assert(bufferAfterClear.messages.length >= 3, 'clearing the personal screen removed server messages');
  assert(bufferAfterClear.messages.every((message) => /^[0-9a-f-]{36}$/.test(message.eventKey) && !Object.hasOwn(message, 'id')));
  await ac.publishAsync(alice.session.topicPrefix + 'sensor/after-clear', 'alice-after-clear', { qos: 1 });
  await expect(alice.page.locator('.messages-table tbody tr')).toHaveCount(1);
  await expect(alice.page.locator('.messages-table tbody tr').first()).toContainText('alice-after-clear');
  await alice.page.screenshot({ path: path.join(output, 'messages-account-desktop.png'), fullPage: true });
  await alice.page.setViewportSize({ width: 390, height: 844 });
  await alice.page.evaluate(() => new Promise((resolve) => requestAnimationFrame(() => requestAnimationFrame(resolve))));
  await expect.poll(() => alice.page.locator('.main-shell').evaluate((el) => parseFloat(getComputedStyle(el).marginLeft))).toBe(0);
  await noOverflow(alice.page);
  await alice.page.getByRole('button', { name: /^Открыть сообщение / }).first().click();
  await alice.page.screenshot({ path: path.join(output, 'message-account-mobile.png'), animations: 'disabled' });
  await alice.page.getByRole('dialog').locator('.modal-footer').getByRole('button', { name: 'Закрыть', exact: true }).click();
  await alice.page.setViewportSize({ width: 1440, height: 1000 });
  alice.page.off('request', countPoll);
  await alice.page.getByRole('button', { name: 'Retain', exact: true }).click();
  await expect(alice.page.getByText('alice-temperature-25', { exact: true })).toBeVisible();
  await alice.page.getByRole('button', { name: 'Открыть ' + alice.session.topicPrefix + 'sensor/value', exact: true }).click();
  const retainedDialog = alice.page.getByRole('dialog', { name: 'Сообщение Retain', exact: true });
  await retainedDialog.getByRole('button', { name: 'Скопировать', exact: true }).click();
  assert.equal(await alice.page.evaluate(() => navigator.clipboard.readText()), 'alice-temperature-25');
  await retainedDialog.getByRole('button', { name: 'Удалить сообщение', exact: true }).click();
  await alice.page.getByRole('dialog').getByRole('button', { name: 'Отмена', exact: true }).click();
  assert.equal((await (await alice.page.request.get(base + '/api/v1/account/retained')).json()).length, 1);
  await alice.page.getByRole('button', { name: 'MQTT-доступ', exact: true }).click();
  await alice.page.getByRole('button', { name: 'Обновить данные', exact: true }).click();
  await expect(alice.page.locator('.account-usage').getByText('1 / 10', { exact: true })).toHaveCount(2);
  const sidebar = alice.page.getByRole('navigation', { name: 'Разделы кабинета', exact: true });
  await expect(sidebar).toBeVisible();
  const sidebarBounds = await alice.page.locator('.account-sidebar').boundingBox();
  const contentBounds = await alice.page.locator('.main-shell').boundingBox();
  assert(sidebarBounds && contentBounds && sidebarBounds.x === 0 && contentBounds.x >= sidebarBounds.width, 'desktop navigation is not beside the content');
  await checkDeviceActions(alice.page);
  await noOverflow(alice.page);
  await alice.page.screenshot({ path: path.join(output, 'account-desktop.png'), fullPage: true });
  for (const width of [390, 360]) {
    await alice.page.setViewportSize({ width, height: 844 });
    await expect(sidebar).toBeHidden();
    await alice.page.getByRole('button', { name: 'Открыть меню', exact: true }).click();
    await expect(sidebar).toBeVisible();
    await expect(alice.page.getByRole('button', { name: 'Открыть меню', exact: true })).toHaveAttribute('aria-expanded', 'true');
    await noOverflow(alice.page);
    await alice.page.screenshot({ path: path.join(output, 'account-menu-' + width + '.png'), fullPage: true });
    await sidebar.getByRole('button', { name: 'Настройки', exact: true }).click();
    await expect(sidebar).toBeHidden();
    await expect(alice.page.getByRole('heading', { name: 'Пароль аккаунта', exact: true })).toBeVisible();
    await expect(alice.page.locator('.account-connection')).toHaveCount(0);
    await alice.page.getByRole('button', { name: 'Открыть меню', exact: true }).click();
    await alice.page.getByRole('button', { name: 'Закрыть меню', exact: true }).click({ position: { x: width - 12, y: 100 } });
    await expect(sidebar).toBeHidden();
    await alice.page.getByRole('button', { name: 'Открыть меню', exact: true }).click();
    await alice.page.keyboard.press('Escape');
    await expect(sidebar).toBeHidden();
    await expect(alice.page.getByRole('button', { name: 'Открыть меню', exact: true })).toBeFocused();
    await alice.page.getByRole('button', { name: 'Открыть меню', exact: true }).click();
    await expectLocalNavigation(alice.page, async () => {
      await alice.page.getByRole('link', { name: 'CLEX MQTT – MQTT-доступ', exact: true }).click();
      await expect(alice.page.getByRole('button', { name: 'Добавить учётную запись', exact: true })).toBeVisible();
    });
    await expect(sidebar).toBeHidden();
    await expect(alice.page.getByRole('button', { name: 'Открыть меню', exact: true })).toHaveAttribute('aria-expanded', 'false');
    await expect(alice.page.locator('.account-usage').getByText('1 / 10', { exact: true })).toHaveCount(2);
    await checkDeviceActions(alice.page);
    await noOverflow(alice.page);
    await alice.page.screenshot({ path: path.join(output, 'account-mobile-' + width + '.png'), fullPage: true });
    await alice.page.getByRole('button', { name: 'Изменить учётную запись esp32-01', exact: true }).click();
    const form = alice.page.getByRole('dialog');
    await checkDeviceFormSpacing(form);
    await noOverflow(alice.page);
    await alice.page.screenshot({ path: path.join(output, 'device-edit-mobile-' + width + '.png'), animations: 'disabled' });
    await form.getByRole('button', { name: 'Отмена', exact: true }).click();
  }
  await alice.page.setViewportSize({ width: 1440, height: 1000 });

  const admin = await browser.newContext({ viewport: { width: 1440, height: 1000 }, reducedMotion: 'reduce' });
  const ap = await admin.newPage();
  ap.on('pageerror', (e) => errors.push(e.message));
  await ap.goto(base + '/admin');
  await ap.getByLabel('Имя администратора').fill(process.env.CLEX_UI_TEST_USERNAME);
  await ap.getByLabel('Пароль', { exact: true }).fill(adminPassword);
  await ap.getByRole('button', { name: 'Войти в панель' }).click();
  await expect(ap.locator('.server-summary')).toHaveCount(0);
  await ap.getByRole('link', { name: 'Аккаунты', exact: true }).click();
  await expect(ap.getByRole('heading', { name: 'Аккаунты', exact: true })).toBeVisible();
  await expect(ap.getByRole('link', { name: 'CLEX MQTT – Обзор', exact: true })).toHaveAttribute('href', '#overview');
  await expectLocalNavigation(ap, async () => {
    await ap.getByRole('link', { name: 'CLEX MQTT – Обзор', exact: true }).click();
    await expect(ap.getByRole('heading', { name: 'Обзор', exact: true })).toBeVisible();
    await expect(ap).toHaveURL(base + '/admin#overview');
  });
  await ap.setViewportSize({ width: 390, height: 844 });
  await ap.getByRole('button', { name: 'Открыть меню', exact: true }).click();
  await ap.getByRole('link', { name: 'CLEX MQTT – Обзор', exact: true }).click();
  await ap.waitForFunction(() => document.querySelector('.sidebar').getBoundingClientRect().right <= 1);
  await ap.setViewportSize({ width: 1440, height: 1000 });
  await ap.goto(base);
  await expect(ap.getByRole('heading', { name: 'MQTT-брокер для ваших устройств', exact: true })).toBeVisible();
  await expect(ap.locator('a[href^="/admin"]')).toHaveCount(0);
  await ap.goto(base + '/admin');
  await expect(ap.getByRole('link', { name: 'Аккаунты', exact: true })).toBeVisible();
  await ap.goto(base + '/#users?tab=groups');
  await expect(ap).toHaveURL(base + '/admin#users?tab=groups');
  await expect(ap.getByRole('heading', { name: 'MQTT-доступ', exact: true })).toBeVisible();
  await ap.getByRole('link', { name: 'Аккаунты', exact: true }).click();
  await ap.getByRole('link', { name: 'MQTT-доступ', exact: true }).click();
  await checkAdminFormSpacing(ap, 'admin-mqtt-user-form', () => ap.getByRole('button', { name: 'Добавить учётную запись', exact: true }).click());
  await checkAdminFormSpacing(ap, 'admin-mqtt-password-form', () => ap.getByRole('button', { name: 'Сменить пароль ' + alice.device.username, exact: true }).click());
  await checkAdminFormSpacing(ap, 'admin-certificate-form', async () => {
    await ap.getByRole('button', { name: 'Сертификаты ' + alice.device.username, exact: true }).click();
    await ap.getByRole('button', { name: 'Добавить сертификат', exact: true }).click();
  });
  await ap.getByRole('link', { name: 'Группы', exact: true }).click();
  await checkAdminFormSpacing(ap, 'admin-group-form', () => ap.getByRole('button', { name: 'Добавить группу', exact: true }).first().click());
  await ap.getByRole('link', { name: 'Мосты', exact: true }).click();
  await checkAdminFormSpacing(ap, 'admin-bridge-form', () => ap.getByRole('button', { name: 'Добавить мост', exact: true }).click());
  await ap.getByRole('link', { name: 'Аккаунты', exact: true }).click();
  await ap.getByRole('button', { name: 'Настроить аккаунт ' + names[0] }).click();
  let dialog = ap.getByRole('dialog');
  for (const width of [1440, 390, 360]) {
    await ap.setViewportSize({ width, height: width === 1440 ? 1000 : 844 });
    await expect.poll(() => dialog.evaluate((el) => getComputedStyle(el).opacity)).toBe('1');
    const fields = await dialog.locator('.field').all();
    assert.equal(fields.length, 7);
    const boxes = await Promise.all(fields.map((field) => field.boundingBox()));
    assert(boxes.every(Boolean));
    for (let i = 1; i < boxes.length; i++) assert(boxes[i].y - boxes[i - 1].y - boxes[i - 1].height >= 16, 'administrator account fields touch vertically');
    await ap.screenshot({ path: path.join(output, 'admin-account-edit-' + width + '.png'), animations: 'disabled' });
    await dialog.getByLabel(/^Сбросить пароль аккаунта/).scrollIntoViewIfNeeded();
    await ap.screenshot({ path: path.join(output, 'admin-account-edit-bottom-' + width + '.png'), animations: 'disabled' });
  }
  await ap.setViewportSize({ width: 1440, height: 1000 });
  await dialog.getByLabel('MQTT-учётные записи', { exact: true }).fill('1');
  await dialog.getByLabel('Одновременные подключения', { exact: true }).fill('1');
  const closed = once(ac, 'close');
  await dialog.getByRole('button', { name: 'Сохранить', exact: true }).click();
  await expect(dialog).toHaveCount(0);
  await closed;
  assert(bc.connected);
  ac = await connect(alice);
  await ap.getByRole('button', { name: 'Настроить аккаунт ' + names[0] }).click();
  dialog = ap.getByRole('dialog');
  await dialog.getByLabel('Аккаунт включён').uncheck();
  const blocked = once(ac, 'close');
  await dialog.getByRole('button', { name: 'Сохранить', exact: true }).click();
  await expect(dialog).toHaveCount(0);
  await blocked;
  assert(bc.connected);
  await bc.publishAsync(bob.session.topicPrefix + 'sensor/after', 'other-account-stays-online', { qos: 1 });
  await alice.page.getByRole('button', { name: 'Обновить данные', exact: true }).click();
  await expect(alice.page.getByRole('button', { name: 'Войти в кабинет', exact: true })).toBeVisible();
  await ap.screenshot({ path: path.join(output, 'accounts-admin.png'), fullPage: true });

  await ap.getByRole('button', { name: 'Настроить аккаунт ' + names[0] }).click();
  dialog = ap.getByRole('dialog');
  await dialog.getByLabel('Аккаунт включён').check();
  await dialog.getByLabel(/^Сбросить пароль аккаунта/).fill(password + '-reset');
  await dialog.getByRole('button', { name: 'Сохранить', exact: true }).click();
  await expect(dialog).toHaveCount(0);
  await alice.page.getByLabel('Логин', { exact: true }).fill(names[0]);
  await alice.page.getByLabel('Пароль', { exact: true }).fill(password + '-reset');
  await alice.page.getByRole('button', { name: 'Войти в кабинет', exact: true }).click();
  await expect(alice.page.getByRole('heading', { name: 'MQTT-доступ' })).toBeVisible();
  await expect(alice.page.getByRole('button', { name: 'Добавить учётную запись', exact: true })).toBeDisabled();
  ac = await connect(alice);
  assert(ac.connected, 'resetting the account password changed MQTT credentials');
  await alice.page.getByRole('button', { name: 'Изменить учётную запись esp32-01', exact: true }).click();
  dialog = alice.page.getByRole('dialog');
  await checkDeviceFormSpacing(dialog);
  await alice.page.screenshot({ path: path.join(output, 'device-edit-desktop.png'), animations: 'disabled' });
  await expect(dialog.getByLabel(/^Публикация/)).toHaveValue(alice.session.topicPrefix + '#');
  await copyLoginWithoutSaving(alice.page, dialog, alice.device.username);
  assert(ac.connected, 'copying the login disconnected the MQTT client');
  await dialog.getByLabel(/^Публикация/).fill('sensor/#');
  await dialog.getByRole('button', { name: 'Сохранить', exact: true }).click();
  await expect(dialog.getByText('Начните каждый MQTT-фильтр с ' + alice.session.topicPrefix, { exact: true })).toBeVisible();
  assert(ac.connected, 'invalid short filter disconnected the MQTT client');
  await dialog.getByLabel(/^Публикация/).fill(alice.session.topicPrefix + 'sensor/#');
  await dialog.getByRole('button', { name: 'Сохранить', exact: true }).click();
  await expect(dialog).toHaveCount(0);
  const devices = await (await alice.page.request.get(base + '/api/v1/account/users')).json();
  assert.deepEqual(devices[0].publishFilters, [alice.session.topicPrefix + 'sensor/#']);
  await alice.page.getByRole('button', { name: 'Удалить учётную запись esp32-01', exact: true }).click();
  await alice.page.getByRole('dialog').getByRole('button', { name: 'Удалить', exact: true }).click();
  await expect(alice.page.getByText('Учётных записей пока нет', { exact: true })).toBeVisible();
  assert.equal((await (await alice.page.request.get(base + '/api/v1/account/users')).json()).length, 0);
  assert.equal((await (await bob.page.request.get(base + '/api/v1/account/users')).json()).length, 1);
  assert.deepEqual(errors, []);
  const report = { ok: true, checks: ['shared personal/admin message viewer, newest first, pause/resume, local clear, details, clipboard and account isolation','copying login never submits a valid create or edit form','one MQTT login supports two simultaneous distinct Client IDs','personal MQTT access terminology and login preview copy','MQTT user, password, certificate, group and bridge form spacing on desktop and mobile','administrator account form spacing at desktop and mobile widths','readable account prefix and device login','copy broker address, prefix, full topic and MQTT login','personal desktop sidebar','mobile navigation, backdrop, Escape and logo closure','device icon actions stay in one row','device enable and disable persist','device form spacing on desktop and mobile','section logos preserve sessions without document reload','mobile admin logo closes navigation','registration heading without redundant eyebrow','account password wording','public user entry without admin links','registration availability and options failure','direct registration and reload','entry routes','entry desktop and mobile','existing sessions across role entry','legacy administrator bookmarks','registration','device CRUD','separate MQTT credentials','cross-account API denial','MQTT publish and delivery isolation','message and Retain visibility','administrator quotas','targeted account blocking','password reset','desktop and mobile layout'], pageErrors: errors };
  await writeFile(path.join(output, 'report.json'), JSON.stringify(report, null, 2) + '\n', { mode: 0o600 });
  console.log('Account UI: OK; real browser, MQTT, ownership, limits and recovery flows passed.');
} finally {
  for (const client of clients) { if (!client.disconnected) await client.endAsync(true).catch(() => {}); }
  await browser.close();
}
