import assert from 'node:assert/strict';
import { mkdir } from 'node:fs/promises';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { chromium, expect } from '@playwright/test';

const username = process.env.CLEX_UI_TEST_USERNAME;
const password = process.env.CLEX_UI_TEST_PASSWORD;
if (!username || !password) {
  throw new Error('Set CLEX_UI_TEST_USERNAME and CLEX_UI_TEST_PASSWORD for a disposable local administrator. This script never reads .env files.');
}

const baseURL = process.env.CLEX_UI_BASE_URL || 'http://127.0.0.1:8085';
const target = new URL(baseURL);
assert(['127.0.0.1', 'localhost', '[::1]'].includes(target.hostname), 'UI smoke checks only run against a local loopback server.');
assert(!target.username && !target.password, 'Do not put credentials in CLEX_UI_BASE_URL.');
const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const output = path.join(root, 'work', 'ui');
const checks = [];
const consoleErrors = [];
const pageErrors = [];
const failedRequests = [];
const httpErrors = [];
const loadedAssets = new Set();
let authenticated = false;
let browser;
let context;
let page;

function redact(value) {
  return String(value).split(password).join('[redacted]').split(username).join('[test administrator]');
}

async function checkOverflow(page, label) {
  await page.waitForFunction(() => document.documentElement.scrollWidth <= window.innerWidth + 1, undefined, { timeout: 1500, polling: 'raf' }).catch(() => {});
  const result = await page.evaluate(() => ({
    viewport: window.innerWidth,
    document: document.documentElement.scrollWidth,
    scrollX: window.scrollX,
    geometry: ['html', 'body', '.sidebar', '.main-shell', '.card', '.table-scroll', '.modal-backdrop', '.modal', '.modal-body', '.modal-footer'].map((selector) => {
      const element = document.querySelector(selector);
      if (!element) return { selector, missing: true };
      const box = element.getBoundingClientRect();
      const style = getComputedStyle(element);
      return { selector, x: box.x, right: box.right, width: box.width, scrollWidth: element.scrollWidth, overflowX: style.overflowX, position: style.position };
    }),
    offenders: [...document.querySelectorAll('body *')].filter((element) => {
      const box = element.getBoundingClientRect();
      return box.width > 0 && box.right > window.innerWidth + 1 && !element.closest('.table-scroll, .payload-content, .sidebar');
    }).slice(0, 8).map((element) => ({ tag: element.tagName, className: String(element.className).slice(0, 100) })),
  }));
  assert(result.document <= result.viewport + 1, `${label}: horizontal document overflow ${JSON.stringify(result)}`);
  checks.push(`${label}: no document overflow`);
}

async function navigate(page, title, hash) {
  if ((page.viewportSize()?.width ?? 1440) <= 760) {
    await page.getByRole('button', { name: 'Открыть меню', exact: true }).click();
  }
  await page.getByRole('navigation', { name: 'Основная навигация' }).getByRole('link', { name: title, exact: true }).click();
  await expect(page).toHaveURL(new RegExp(`#${hash}$`));
  await expect(page.getByRole('heading', { name: title, exact: true, level: 1 })).toBeVisible();
}

async function inspectSections(page, viewportName) {
  await navigate(page, 'Подключения', 'clients');
  await expect(page.locator('.segmented')).toBeVisible();
  await checkOverflow(page, `${viewportName}/clients`);

  await navigate(page, 'Мосты', 'bridges');
  await expect(page.getByRole('heading', { name: 'Мосты MQTT', exact: true })).toBeVisible();
  await checkOverflow(page, `${viewportName}/bridges`);
  await page.getByRole('button', { name: 'Добавить мост', exact: true }).click();
  const bridgeDialog = page.getByRole('dialog', { name: 'Новый мост MQTT', exact: true });
  await expect(bridgeDialog.getByLabel('Адрес MQTT-сервера', { exact: true })).toBeVisible();
  await checkOverflow(page, `${viewportName}/bridge-form`);
  await bridgeDialog.getByRole('button', { name: 'Отмена', exact: true }).click();

  await navigate(page, 'MQTT-доступ', 'users');
  await expect(page.getByRole('heading', { name: 'Учётные записи MQTT', exact: true })).toBeVisible();
  await checkOverflow(page, `${viewportName}/users-before-dialog`);
  await page.getByRole('button', { name: 'Добавить учётную запись', exact: true }).first().click();
  const userDialog = page.getByRole('dialog', { name: 'Новая учётная запись MQTT' });
  await expect(userDialog).toBeVisible();
  await expect(userDialog.getByLabel('MQTT-логин', { exact: true })).toBeVisible();
  await expect(userDialog.getByLabel('Публикация', { exact: true })).toBeVisible();
  await expect(userDialog.getByLabel('Подписка', { exact: true })).toBeVisible();
  if (viewportName === 'desktop') {
    const usernameField = userDialog.getByLabel('MQTT-логин', { exact: true });
    await usernameField.fill('ui-smoke.user@local-01');
    assert(await usernameField.evaluate((input) => input.checkValidity()), 'Username validation should allow the documented characters.');
    await usernameField.fill('invalid/user');
    assert(!(await usernameField.evaluate((input) => input.checkValidity())), 'Username validation must reject a slash.');
    await usernameField.clear();
  }
  await checkOverflow(page, `${viewportName}/user-form`);
  await userDialog.getByRole('button', { name: 'Отмена', exact: true }).click();
  await expect(userDialog).not.toBeVisible();
  await checkOverflow(page, `${viewportName}/users`);
  if (viewportName === 'desktop') await page.screenshot({ path: path.join(output, 'users-desktop.png'), fullPage: true });
  await page.getByRole('navigation', { name: 'Вкладки доступа' }).getByRole('link', { name: 'Группы', exact: true }).click();
  await expect(page.getByRole('heading', { name: 'Группы доступа', exact: true })).toBeVisible();
  await checkOverflow(page, `${viewportName}/access-groups`);
  await page.getByRole('navigation', { name: 'Вкладки доступа' }).getByRole('link', { name: 'Проверка доступа', exact: true }).click();
  await expect(page.getByRole('button', { name: 'Проверить доступ', exact: true })).toBeVisible();
  await checkOverflow(page, `${viewportName}/access-inspector`);
  await navigate(page, 'Система', 'settings');
  await expect(page.getByRole('heading', { name: 'Сетевые интерфейсы', exact: true })).toBeVisible();
  await expect(page.getByRole('heading', { name: 'Возможности версии', exact: true })).toBeVisible();
  await expect(page.getByRole('heading', { name: 'Мониторинг', exact: true })).toBeVisible();
  assert(await page.locator('table tbody tr').count() > 0, 'The actual server must report at least one listener.');
  await checkOverflow(page, `${viewportName}/settings`);
  if (viewportName === 'desktop') await page.screenshot({ path: path.join(output, 'settings-desktop.png'), fullPage: true });

  await navigate(page, 'Сообщения', 'messages');
  await expect(page.getByRole('heading', { name: 'Поток сообщений', exact: true })).toBeVisible();
  await page.getByRole('button', { name: 'Пауза', exact: true }).click();
  await expect(page.getByRole('button', { name: 'Продолжить', exact: true })).toBeVisible();
  await checkOverflow(page, `${viewportName}/message-stream`);
  await page.getByRole('navigation', { name: 'Вкладки сообщений' }).getByRole('link', { name: 'Retain', exact: true }).click();
  await expect(page.getByRole('heading', { name: 'Retain', exact: true, level: 2 })).toBeVisible();
  const emptyRetained = await page.getByRole('heading', { name: 'Опубликуйте сообщение с Retain', exact: true }).isVisible();
  if (process.env.CLEX_UI_EXPECT_EMPTY_RETAINED === '1') assert(emptyRetained, 'The disposable test fixture must have an empty retained list.');
  checks.push(`${viewportName}/retained: ${emptyRetained ? 'empty state verified' : 'actual retained records rendered'}`);
  await checkOverflow(page, `${viewportName}/retained`);

  await navigate(page, 'Журнал действий', 'audit');
  await expect(page.getByRole('heading', { name: 'Последние действия', exact: true })).toBeVisible();
  await checkOverflow(page, `${viewportName}/audit`);

  await navigate(page, 'Обзор', 'overview');
  await expect(page.getByRole('heading', { name: 'База данных доступна', exact: true })).toBeVisible();
  await expect(page.locator('.stat-card')).toHaveCount(4);
  await checkOverflow(page, `${viewportName}/overview`);
}

try {
  await mkdir(output, { recursive: true });
  browser = await chromium.launch({
    headless: true,
    executablePath: process.env.PLAYWRIGHT_CHROMIUM_EXECUTABLE_PATH || undefined,
  });
  context = await browser.newContext({ baseURL, viewport: { width: 1440, height: 1000 }, locale: 'ru-RU', timezoneId: 'Asia/Almaty', reducedMotion: 'reduce' });
  page = await context.newPage();
  page.setDefaultTimeout(15_000);
  page.on('console', (message) => {
    if (message.type() !== 'error') return;
    if (!authenticated && message.text().includes('401') && message.location().url.includes('/api/v1/session')) return;
    const location = message.location().url;
    consoleErrors.push(redact(`${message.text()}${location ? ` (${new URL(location).pathname})` : ''}`));
  });
  page.on('response', (response) => {
    const pathname = new URL(response.url()).pathname;
    if (pathname.startsWith('/assets/') && response.ok()) loadedAssets.add(pathname);
    if (response.status() < 400) return;
    if (!authenticated && response.status() === 401 && pathname === '/api/v1/session') return;
    httpErrors.push(`${response.status()} ${pathname}`);
  });
  page.on('pageerror', (error) => pageErrors.push(redact(error.message)));
  page.on('requestfailed', (request) => {
    const failure = request.failure()?.errorText ?? 'unknown request failure';
    if (!failure.includes('ERR_ABORTED')) failedRequests.push(redact(`${request.method()} ${new URL(request.url()).pathname}: ${failure}`));
  });

  await page.goto('/admin', { waitUntil: 'domcontentloaded' });
  await expect(page.getByRole('heading', { name: 'На связи!', exact: true })).toBeVisible();
  await checkOverflow(page, 'desktop/login');
  await page.screenshot({ path: path.join(output, 'login.png'), fullPage: true });
  await page.getByLabel('Имя администратора', { exact: true }).fill(username);
  await page.getByLabel('Пароль', { exact: true }).fill(password);
  await page.getByRole('button', { name: 'Войти в панель', exact: true }).click();
  await expect(page.getByRole('heading', { name: 'Обзор', exact: true })).toBeVisible();
  authenticated = true;
  await expect(page.getByRole('heading', { name: 'База данных доступна', exact: true })).toBeVisible();
  checks.push('login: authenticated against actual local API');

  await inspectSections(page, 'desktop');
  await page.screenshot({ path: path.join(output, 'desktop.png'), fullPage: true });
  await page.setViewportSize({ width: 390, height: 844 });
  await page.reload({ waitUntil: 'domcontentloaded' });
  await expect(page.getByRole('heading', { name: 'База данных доступна', exact: true })).toBeVisible();
  await inspectSections(page, 'mobile');
  await page.screenshot({ path: path.join(output, 'mobile.png'), fullPage: true });
  await page.getByRole('button', { name: 'Открыть меню', exact: true }).click();
  await page.getByRole('button', { name: 'Выйти из панели', exact: true }).click();
  await expect(page.getByRole('heading', { name: 'На связи!', exact: true })).toBeVisible();
  authenticated = false;
  checks.push('logout: returned to login screen');
  await checkOverflow(page, 'mobile/login');
  assert.deepEqual(pageErrors, [], 'Uncaught browser JavaScript errors');
  assert.deepEqual(consoleErrors, [], 'Unexpected browser console errors');
  assert.deepEqual(failedRequests, [], 'Unexpected failed browser requests');
  assert.deepEqual(httpErrors, [], 'Unexpected HTTP errors');
  console.log(JSON.stringify({ ok: true, browser: browser.version(), assets: [...loadedAssets], checks, screenshots: ['login.png', 'desktop.png', 'users-desktop.png', 'settings-desktop.png', 'mobile.png'].map((file) => `work/ui/${file}`), pageErrors, consoleErrors, failedRequests, httpErrors }, null, 2));
} catch (error) {
  if (page && !page.isClosed()) {
    try { await page.screenshot({ path: path.join(output, 'failure.png'), fullPage: true }); }
    catch { /* The browser itself may have failed. */ }
  }
  console.error(redact(error instanceof Error ? error.stack : error));
  console.error(JSON.stringify({ ok: false, checks, pageErrors, consoleErrors, failedRequests, httpErrors }, null, 2));
  process.exitCode = 1;
} finally {
  if (authenticated && context) {
    try {
      const response = await context.request.get('/api/v1/session');
      if (response.ok()) {
        const session = await response.json();
        await context.request.delete('/api/v1/session', { headers: { 'X-CSRF-Token': session.csrfToken, Origin: target.origin } });
      }
    } catch {
      console.error('The disposable administrator may still have an active server session; remove it after this test.');
    }
  }
  await context?.close();
  await browser?.close();
}
