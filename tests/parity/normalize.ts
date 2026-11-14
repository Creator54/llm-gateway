// Mask values that change on every run so two recordings can be compared.
import type { RecordedHeaders, RecordedRequest } from './recorder';

// Never useful in a diff, and some of these are secrets.
export const ALWAYS_MASKED_HEADERS = ['authorization', 'x-api-key', 'api-key', 'cookie'];
export const DEFAULT_MASKED_HEADERS = ['date', 'x-request-id', 'request-id', 'idempotency-key'];

export type NormalizeOptions = {
  /** Extra header names to mask, case-insensitive. */
  headers?: string[];
};

const PATTERNS: [RegExp, string][] = [
  [/\b[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}\b/gi, '<uuid>'],
  [/\b(?:req|resp|msg|chatcmpl|rs|call|run|trace|span|sess|session)[_-][A-Za-z0-9_-]{6,}/g, '<id>'],
  [/\b\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}(?:\.\d+)?(?:Z|[+-]\d{2}:?\d{2})?\b/g, '<timestamp>'],
  [/\b\d{4}-\d{2}-\d{2}\b/g, '<date>'],
];

// Epoch seconds or milliseconds, but only under a key that really is a time.
const TIMESTAMP_KEY_PATTERN =
  /((?:^|[\s?&,{"'])(?:created|created_at|expires_at|timestamp|iat|exp|reset_at)"?\s*[:=]\s*"?)(\d{9,13})(?!\d)/g;

/** Replace uuids, request ids, timestamps and dates with stable placeholders. */
export function maskVolatile(text: string): string {
  let out = text;
  for (const [pattern, placeholder] of PATTERNS) out = out.replace(pattern, placeholder);
  return out.replace(TIMESTAMP_KEY_PATTERN, '$1<timestamp>');
}

function maskHeaders(headers: RecordedHeaders, masked: Set<string>): RecordedHeaders {
  return headers.map(([name, value]): [string, string] => {
    const lower = name.toLowerCase();
    return [lower, masked.has(lower) ? '<masked>' : maskVolatile(value)];
  });
}

function maskBody(encoding: 'utf8' | 'base64', body: string): string {
  // Binary bodies are kept verbatim; there is no safe text masking for them.
  return encoding === 'base64' ? body : maskVolatile(body);
}

/** Pure: returns a new record, never mutates the input. */
export function normalizeRequest(input: RecordedRequest, options: NormalizeOptions = {}): RecordedRequest {
  const maskedHeaders = new Set(
    [...ALWAYS_MASKED_HEADERS, ...DEFAULT_MASKED_HEADERS, ...(options.headers ?? [])].map((name) => name.toLowerCase()),
  );
  return {
    method: maskVolatile(input.method),
    url: maskVolatile(input.url),
    headers: maskHeaders(input.headers, maskedHeaders),
    bodyEncoding: input.bodyEncoding,
    body: maskBody(input.bodyEncoding, input.body),
    bodyTruncated: input.bodyTruncated,
    response: input.response
      ? {
          status: input.response.status,
          headers: maskHeaders(input.response.headers, maskedHeaders),
          bodyEncoding: input.response.bodyEncoding,
          body: maskBody(input.response.bodyEncoding, input.response.body),
          bodyTruncated: input.response.bodyTruncated,
        }
      : undefined,
  };
}

export function normalizeLog(records: RecordedRequest[], options: NormalizeOptions = {}): RecordedRequest[] {
  return records.map((record) => normalizeRequest(record, options));
}
