// Mask values that change on every run so two recordings can be compared.
import type { RecordedRequest } from './recorder';

// Never useful in a diff, and some of these are secrets.
export const ALWAYS_MASKED_HEADERS = ['authorization', 'x-api-key', 'api-key', 'cookie', 'content-length'];
export const DEFAULT_MASKED_HEADERS = ['date', 'x-request-id', 'request-id', 'idempotency-key'];

export type NormalizeOptions = {
  /** Extra header names to mask, case-insensitive. */
  headers?: string[];
};

const PATTERNS: [RegExp, string][] = [
  [/\b[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}\b/gi, '<uuid>'],
  [/\b(?:req|resp|msg|chatcmpl|rs|call|run|trace|span|sess|session)[_-][A-Za-z0-9_-]{6,}/g, '<id>'],
  [/\b\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}(?:\.\d+)?(?:Z|[+-]\d{2}:?\d{2})?\b/g, '<timestamp>'],
  [/\b(?:1\d{12}|1\d{9})\b/g, '<timestamp>'],
  [/\b\d{4}-\d{2}-\d{2}\b/g, '<date>'],
];

/** Replace uuids, request ids, timestamps and dates with stable placeholders. */
export function maskVolatile(text: string): string {
  let out = text;
  for (const [pattern, placeholder] of PATTERNS) out = out.replace(pattern, placeholder);
  return out;
}

/** Pure: returns a new record, never mutates the input. */
export function normalizeRequest(input: RecordedRequest, options: NormalizeOptions = {}): RecordedRequest {
  const maskedHeaders = new Set(
    [...ALWAYS_MASKED_HEADERS, ...DEFAULT_MASKED_HEADERS, ...(options.headers ?? [])].map((name) => name.toLowerCase()),
  );
  return {
    method: maskVolatile(input.method),
    url: maskVolatile(input.url),
    headers: input.headers.map(([name, value]): [string, string] => {
      const lower = name.toLowerCase();
      return [lower, maskedHeaders.has(lower) ? '<masked>' : maskVolatile(value)];
    }),
    bodyEncoding: input.bodyEncoding,
    // Binary bodies are kept verbatim; there is no safe text masking for them.
    body: input.bodyEncoding === 'base64' ? input.body : maskVolatile(input.body),
  };
}

export function normalizeLog(records: RecordedRequest[], options: NormalizeOptions = {}): RecordedRequest[] {
  return records.map((record) => normalizeRequest(record, options));
}
