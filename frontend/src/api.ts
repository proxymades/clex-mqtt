export class ApiError extends Error {
  constructor(
    message: string,
    public readonly status: number,
    public readonly code: string,
  ) {
    super(message);
    this.name = 'ApiError';
  }
}

export interface RequestOptions {
  method?: 'GET' | 'POST' | 'PATCH' | 'DELETE';
  body?: unknown;
  csrfToken?: string;
  signal?: AbortSignal;
}

export type ApiClient = <T>(path: string, options?: RequestOptions) => Promise<T>;

export async function request<T>(path: string, options: RequestOptions = {}): Promise<T> {
  const headers: Record<string, string> = { Accept: 'application/json' };
  if (options.body !== undefined) headers['Content-Type'] = 'application/json';
  if (options.csrfToken) headers['X-CSRF-Token'] = options.csrfToken;

  let response: Response;
  try {
    response = await fetch(`/api/v1${path}`, {
      method: options.method ?? 'GET',
      credentials: 'same-origin',
      cache: 'no-store',
      headers,
      body: options.body === undefined ? undefined : JSON.stringify(options.body),
      signal: options.signal,
    });
  } catch (error) {
    if (error instanceof DOMException && error.name === 'AbortError') throw error;
    throw new ApiError('Ошибка соединения с сервером. Проверьте подключение и повторите запрос.', 0, 'network_error');
  }

  if (!response.ok) {
    let message = `Ошибка запроса: HTTP ${response.status}. Повторите попытку.`;
    let code = 'request_failed';
    try {
      const data = await response.json() as { error?: { message?: string; code?: string } };
      message = data.error?.message || message;
      code = data.error?.code || code;
    } catch {
      // A reverse proxy can return a non-JSON response.
    }
    throw new ApiError(message, response.status, code);
  }

  if (response.status === 204) return undefined as T;
  const body = await response.text();
  if (!body) return undefined as T;
  try {
    return JSON.parse(body) as T;
  } catch {
    throw new ApiError('Ошибка формата ответа сервера. Повторите запрос.', response.status, 'invalid_response');
  }
}

export function errorMessage(error: unknown): string {
  return error instanceof Error ? error.message : 'Ошибка запроса. Повторите попытку.';
}
