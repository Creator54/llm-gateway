import { describe, expect, test } from 'bun:test';
import { mkdtemp, writeFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { diffLogs, diffRequest, formatDifferences, parseDiffArgs, parseLog } from './diff';
import type { RecordedRequest } from './recorder';

const record = (overrides: Partial<RecordedRequest> = {}): RecordedRequest => ({
  method: 'POST',
  url: '/v1/responses',
  headers: [
    ['content-type', 'application/json'],
    ['accept', '*/*'],
  ],
  bodyEncoding: 'utf8',
  body: '{"model":"parity-test","input":"ping"}',
  bodyTruncated: false,
  response: {
    status: 200,
    headers: [
      ['content-type', 'application/json'],
      ['content-length', '2'],
    ],
    bodyEncoding: 'utf8',
    body: '{}',
    bodyTruncated: false,
  },
  ...overrides,
});

const fieldsOf = (difference: ReturnType<typeof diffRequest>): string[] =>
  difference.fields.map((field) => field.field);

describe('diffRequest', () => {
  test('reports body differences', () => {
    const difference = diffRequest(record(), record({ body: '{"model":"parity-test","input":"pong"}' }), 1);
    expect(difference.index).toBe(1);
    expect(fieldsOf(difference)).toEqual(['body line 1']);
  });

  test('reports method and url differences', () => {
    const urlDifference = diffRequest(record({ url: '/v1/responses?x=1' }), record({ url: '/v1/chat/completions' }), 0);
    expect(fieldsOf(urlDifference)).toEqual(['url']);

    const methodDifference = diffRequest(record({ method: 'POST' }), record({ method: 'PUT' }), 0);
    expect(fieldsOf(methodDifference)).toEqual(['method']);
  });

  test('compares header values as an unordered multiset', () => {
    const reordered = diffRequest(
      record({ headers: [['a', '1'], ['b', '2']] }),
      record({ headers: [['b', '2'], ['a', '1']] }),
      0,
    );
    expect(fieldsOf(reordered)).toEqual([]);

    const duplicated = diffRequest(
      record({ headers: [['x', '1'], ['x', '1']] }),
      record({ headers: [['x', '1']] }),
      0,
    );
    expect(fieldsOf(duplicated)).toEqual(['header "x"']);
  });

  test('reports missing and extra headers explicitly', () => {
    const missing = diffRequest(record({ headers: [['x-only', '1']] }), record({ headers: [] }), 0);
    expect(missing.fields).toEqual([{ field: 'header "x-only" (missing)', expected: '["1"]', actual: '<none>' }]);

    const extra = diffRequest(record({ headers: [] }), record({ headers: [['x-only', '1']] }), 0);
    expect(extra.fields).toEqual([{ field: 'header "x-only" (extra)', expected: '<none>', actual: '["1"]' }]);
  });

  test('checks header order only when asked', () => {
    const left = record({ headers: [['a', '1'], ['b', '2']] });
    const right = record({ headers: [['b', '2'], ['a', '1']] });
    expect(fieldsOf(diffRequest(left, right, 0))).toEqual([]);
    expect(fieldsOf(diffRequest(left, right, 0, { checkOrder: true }))).toEqual(['header order']);
  });

  test('compares content-length instead of masking it', () => {
    const left = record({ headers: [['content-length', '40']] });
    const right = record({ headers: [['content-length', '41']] });
    expect(fieldsOf(diffRequest(left, right, 0))).toEqual(['header "content-length"']);
    expect(fieldsOf(diffRequest(left, left, 0))).toEqual([]);
  });

  test('masks volatile values before comparing', () => {
    const left = record({
      headers: [['x-request-id', 'aaaaaaaa-1111-2222-3333-444444444444']],
      body: '{"id":"resp_aaaaaaaa1111","created_at":1770000000,"input":"ping"}',
    });
    const right = record({
      headers: [['x-request-id', 'bbbbbbbb-1111-2222-3333-444444444444']],
      body: '{"id":"resp_bbbbbbbb2222","created_at":1770000001,"input":"ping"}',
    });
    expect(diffRequest(left, right, 0).fields).toEqual([]);
  });

  test('reports response status, headers and body differences', () => {
    const status = diffRequest(
      record(),
      record({
        response: {
          status: 429,
          headers: [
            ['content-type', 'application/json'],
            ['content-length', '2'],
          ],
          bodyEncoding: 'utf8',
          body: '{}',
          bodyTruncated: false,
        },
      }),
      0,
    );
    expect(fieldsOf(status)).toEqual(['response status']);

    const body = diffRequest(
      record(),
      record({
        response: {
          status: 200,
          headers: [
            ['content-type', 'application/json'],
            ['content-length', '2'],
          ],
          bodyEncoding: 'utf8',
          body: '{"x":1}',
          bodyTruncated: false,
        },
      }),
      0,
    );
    expect(fieldsOf(body)).toEqual(['response body line 1']);
  });

  test('honours configurable masked headers', () => {
    const left = record({ headers: [['x-run', '1']] });
    const right = record({ headers: [['x-run', '2']] });
    expect(diffRequest(left, right, 0).fields.length).toBe(1);
    expect(diffRequest(left, right, 0, { headers: ['x-run'] }).fields).toEqual([]);
  });
});

describe('diffLogs', () => {
  test('returns nothing for identical logs', () => {
    expect(diffLogs([record(), record()], [record(), record()])).toEqual([]);
  });

  test('reports a request missing on either side', () => {
    const missing = diffLogs([record()], []);
    expect(missing).toHaveLength(1);
    expect(missing[0]!.fields[0]).toEqual({ field: 'request', expected: 'present', actual: '<missing>' });
  });

  test('reports each differing request index', () => {
    const differences = diffLogs([record(), record()], [record(), record({ body: '{"different":true}' })]);
    expect(differences.map((difference) => difference.index)).toEqual([1]);
  });
});

describe('parseLog and formatDifferences', () => {
  test('parses jsonl and skips blank lines', () => {
    const text = `${JSON.stringify(record())}\n\n${JSON.stringify(record({ method: 'GET' }))}\n`;
    expect(parseLog(text).map((entry) => entry.method)).toEqual(['POST', 'GET']);
  });

  test('rejects broken lines with the line number', () => {
    expect(() => parseLog('{"ok":1}\nnope')).toThrow('line 2');
  });

  test('formats readable output', () => {
    const text = formatDifferences(diffLogs([record()], [record({ body: '{"x":1}' })]));
    expect(text).toContain('request 0 (/v1/responses) differs');
    expect(text).toContain('expected:');
    expect(formatDifferences([])).toBe('no differences');
  });
});

describe('parseDiffArgs', () => {
  test('reads files, masks and the order flag', () => {
    expect(parseDiffArgs(['a.jsonl', 'b.jsonl', '--mask-header', 'x-run', '--check-order'])).toEqual({
      files: ['a.jsonl', 'b.jsonl'],
      masks: ['x-run'],
      checkOrder: true,
    });
  });

  test('rejects unknown flags and missing values', () => {
    expect(() => parseDiffArgs(['a.jsonl', '--nope'])).toThrow('unknown argument');
    expect(() => parseDiffArgs(['a.jsonl', '--mask-header'])).toThrow('missing value');
  });
});

describe('CLI', () => {
  const run = (args: string[]) =>
    Bun.spawnSync({ cmd: ['bun', join(import.meta.dir, 'diff.ts'), ...args], stdout: 'pipe', stderr: 'pipe' });

  test('exits 0 when recordings match after masking', async () => {
    const dir = await mkdtemp(join(tmpdir(), 'parity-diff-'));
    const a = join(dir, 'a.jsonl');
    const b = join(dir, 'b.jsonl');
    await writeFile(a, `${JSON.stringify(record({ body: '{"id":"resp_aaaaaa111111"}' }))}\n`);
    await writeFile(b, `${JSON.stringify(record({ body: '{"id":"resp_bbbbbb222222"}' }))}\n`);
    expect(run([a, b]).exitCode).toBe(0);
  });

  test('exits 1 and prints the difference', async () => {
    const dir = await mkdtemp(join(tmpdir(), 'parity-diff-'));
    const a = join(dir, 'a.jsonl');
    const b = join(dir, 'b.jsonl');
    await writeFile(a, `${JSON.stringify(record())}\n`);
    await writeFile(b, `${JSON.stringify(record({ body: '{"model":"other"}' }))}\n`);
    const result = run([a, b]);
    expect(result.exitCode).toBe(1);
    expect(result.stdout.toString()).toContain('request 0');
  });

  test('exits 2 on bad usage', () => {
    expect(run(['only-one-file']).exitCode).toBe(2);
    expect(run(['a.jsonl', 'b.jsonl', '--nope']).exitCode).toBe(2);
    expect(run(['a.jsonl', 'b.jsonl', '--mask-header']).exitCode).toBe(2);
  });
});
