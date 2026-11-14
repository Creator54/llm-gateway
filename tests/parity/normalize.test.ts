import { describe, expect, test } from 'bun:test';
import { maskVolatile, normalizeLog, normalizeRequest } from './normalize';
import type { RecordedRequest } from './recorder';

const record = (overrides: Partial<RecordedRequest> = {}): RecordedRequest => ({
  method: 'POST',
  url: '/v1/responses',
  headers: [['content-type', 'application/json']],
  bodyEncoding: 'utf8',
  body: '{}',
  ...overrides,
});

describe('maskVolatile', () => {
  test('masks uuids and request ids', () => {
    expect(maskVolatile('id 123e4567-e89b-12d3-a456-426614174000 req_abc123def')).toBe('id <uuid> <id>');
    expect(maskVolatile('chatcmpl-abc123def456')).toBe('<id>');
  });

  test('masks timestamps and dates', () => {
    expect(maskVolatile('at 2026-01-02T03:04:05.678Z')).toBe('at <timestamp>');
    expect(maskVolatile('created 1770000000 expired 1770000000123')).toBe(
      'created <timestamp> expired <timestamp>',
    );
    expect(maskVolatile('on 2026-01-02')).toBe('on <date>');
  });

  test('leaves ordinary text alone', () => {
    expect(maskVolatile('{"input":"hello"}')).toBe('{"input":"hello"}');
  });
});

describe('normalizeRequest', () => {
  test('masks sensitive headers by default', () => {
    const input = record({
      headers: [
        ['authorization', 'Bearer secret'],
        ['content-length', '42'],
        ['x-api-key', 'secret'],
        ['date', 'Thu, 01 Jan 2026 00:00:00 GMT'],
        ['x-request-id', 'abc'],
        ['x-custom', 'keep'],
      ],
    });
    expect(normalizeRequest(input).headers).toEqual([
      ['authorization', '<masked>'],
      ['content-length', '<masked>'],
      ['x-api-key', '<masked>'],
      ['date', '<masked>'],
      ['x-request-id', '<masked>'],
      ['x-custom', 'keep'],
    ]);
  });

  test('masks configurable header names case-insensitively', () => {
    const input = record({ headers: [['X-Session', 'abc'], ['Host', 'example']] });
    const normalized = normalizeRequest(input, { headers: ['x-session'] });
    expect(normalized.headers).toEqual([
      ['x-session', '<masked>'],
      ['host', 'example'],
    ]);
  });

  test('keeps header order and lowercases names', () => {
    const input = record({ headers: [['X-B', '2'], ['X-A', '1']] });
    expect(normalizeRequest(input).headers.map(([name]) => name)).toEqual(['x-b', 'x-a']);
  });

  test('masks url and utf8 body', () => {
    const input = record({
      url: '/v1/responses?session=123e4567-e89b-12d3-a456-426614174000',
      body: '{"id":"resp_abcdef123456","input":"hi"}',
    });
    const normalized = normalizeRequest(input);
    expect(normalized.url).toBe('/v1/responses?session=<uuid>');
    expect(normalized.body).toBe('{"id":"<id>","input":"hi"}');
  });

  test('leaves base64 bodies untouched', () => {
    const input = record({ bodyEncoding: 'base64', body: '//4AgA==' });
    expect(normalizeRequest(input).body).toBe('//4AgA==');
  });

  test('is pure', () => {
    const input = record({
      headers: [['x-request-id', 'abc'], ['x-custom', 'keep']],
      body: '{"id":"msg_abcdef123456"}',
    });
    const snapshot = structuredClone(input);
    normalizeRequest(input);
    expect(input).toEqual(snapshot);
  });

  test('normalizeLog maps every record', () => {
    const normalized = normalizeLog([record({ method: 'POST' }), record({ method: 'GET' })]);
    expect(normalized.map((entry) => entry.method)).toEqual(['POST', 'GET']);
  });
});
