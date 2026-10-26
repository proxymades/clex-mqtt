export const number = (value: number) => new Intl.NumberFormat('ru-RU').format(value);

export function dateTime(value: string | null | undefined): string {
  if (!value) return '–';
  const date = new Date(value);
  if (Number.isNaN(date.getTime())) return '–';
  return new Intl.DateTimeFormat('ru-RU', {
    day: '2-digit', month: 'short', hour: '2-digit', minute: '2-digit',
  }).format(date);
}

export function fullDateTime(value: string | null | undefined): string {
  if (!value) return '–';
  const date = new Date(value);
  if (Number.isNaN(date.getTime())) return '–';
  return new Intl.DateTimeFormat('ru-RU', { dateStyle: 'medium', timeStyle: 'medium' }).format(date);
}

export function duration(seconds: number): string {
  const total = Math.max(0, Math.floor(seconds));
  if (total < 60) return `${total} с`;
  const minutes = Math.floor(total / 60);
  const hours = Math.floor(minutes / 60);
  const days = Math.floor(hours / 24);
  if (days > 0) return `${days} д ${hours % 24} ч`;
  if (hours > 0) return `${hours} ч ${minutes % 60} мин`;
  return `${minutes} мин`;
}

export function bytes(value: number): string {
  if (value < 1024) return `${number(value)} Б`;
  if (value < 1024 * 1024) return `${number(Math.round(value / 1024 * 10) / 10)} КБ`;
  return `${number(Math.round(value / (1024 * 1024) * 10) / 10)} МБ`;
}

export function decodePayload(base64: string, partial = false): { text: string; binary: boolean } {
  try {
    const payload = Uint8Array.from(atob(base64), (character) => character.charCodeAt(0));
    const decoded = new TextDecoder('utf-8', { fatal: true }).decode(payload, { stream: partial });
    if (/[\u0000-\u0008\u000E-\u001F]/.test(decoded)) return { text: base64, binary: true };
    try { return { text: JSON.stringify(JSON.parse(decoded), null, 2), binary: false }; }
    catch { return { text: decoded, binary: false }; }
  } catch {
    return { text: base64, binary: true };
  }
}
