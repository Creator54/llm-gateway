import { describe, expect, test } from 'bun:test';
import { mkdtemp, readFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { decodeBody, startRecorder, type RecordedRequest } from './recorder';

async function withRecorder(run: (base: string) => Promise<void>): Promise<RecordedRequest[]> {
  const dir = await mkdtemp(join(tmpdir(), 'parity-recorder-'));
  const logFile = join(dir, 'requests.jsonl');
  const server = startRecorder({ port: 0, logFile });
  try {
    await run(`http://127.0.0.1:${server.port}`);
  } finally {
    server.stop(true);
  }
  const text = await readFile(logFile, 'utf8');
  return text
    .split('\n')
    .filter((line) => line.trim().length > 0)
    .map((line) => JSON.parse(line) as RecordedRequest);
}

describe('decodeBody', () => {
  test('uses utf8 for text and base64 otherwise', () => {
    expect(decodeBody(new Uint8Array())).toEqual({ encoding: 'utf8', value: '' });
    expect(decodeBody(new TextEncoder().encode('{"a":1}'))).toEqual({ encoding: 'utf8', value: '{"a":1}' });
    expect(decodeBody(new Uint8Array([0xff, 0xfe, 0x00]).slice(0, 2)).encoding).toBe('base64');
  });
});

describe('recorder', () => {
  test('logs method, path+query, lowercase headers in order, and body', async () => {
    const [entry] = await withRecorder(async (base) => {
      await fetch(`${base}/v1/responses?trace=1`, {
        method: 'POST',
        headers: { 'Content-Type': 'application/json', 'X-Order': '1' },
        body: '{"model":"parity-test","stream":false}',
      });
    });
    expect(entry!.method).toBe('POST');
    expect(entry!.url).toBe('/v1/responses?trace=1');
    expect(entry!.headers.every(([name]) => name === name.toLowerCase())).toBe(true);
    const names = entry!.headers.map(([name]) => name);
    expect(names.indexOf('content-type')).toBeLessThan(names.indexOf('x-order'));
    expect(entry!.body).toBe('{"model":"parity-test","stream":false}');
    expect(entry!.bodyEncoding).toBe('utf8');
  });

  test('answers OpenAI Responses with JSON and SSE', async () => {
    await withRecorder(async (base) => {
      const json = await fetch(`${base}/v1/responses`, { method: 'POST', body: '{"stream":false}' });
      expect(json.headers.get('content-type')).toContain('application/json');
      expect(((await json.json()) as { status: string }).status).toBe('completed');

      const sse = await fetch(`${base}/v1/responses`, { method: 'POST', body: '{"stream":true}' });
      const text = await sse.text();
      expect(sse.headers.get('content-type')).toContain('text/event-stream');
      expect(text).toContain('event: response.completed');
    });
  });

  test('answers Chat Completions with JSON and SSE', async () => {
    await withRecorder(async (base) => {
      const json = await fetch(`${base}/v1/chat/completions`, { method: 'POST', body: '{}' });
      expect(((await json.json()) as { object: string }).object).toBe('chat.completion');

      const sse = await fetch(`${base}/v1/chat/completions`, { method: 'POST', body: '{"stream":true}' });
      const text = await sse.text();
      expect(text).toContain('chat.completion.chunk');
      expect(text.trimEnd().endsWith('data: [DONE]')).toBe(true);
    });
  });

  test('answers Anthropic Messages with JSON and SSE', async () => {
    await withRecorder(async (base) => {
      const json = await fetch(`${base}/v1/messages`, { method: 'POST', body: '{}' });
      expect(((await json.json()) as { type: string }).type).toBe('message');

      const sse = await fetch(`${base}/v1/messages`, { method: 'POST', body: '{"stream":true}' });
      expect(await sse.text()).toContain('event: message_stop');
    });
  });

  test('serves a models list', async () => {
    await withRecorder(async (base) => {
      const body = (await (await fetch(`${base}/v1/models`)).json()) as { data: { id: string }[] };
      expect(body.data[0]!.id).toBe('parity-test');
    });
  });

  test('returns 429 with retry-after, 401, and mid-stream errors via the path', async () => {
    await withRecorder(async (base) => {
      const limited = await fetch(`${base}/429/v1/responses`, { method: 'POST', body: '{}' });
      expect(limited.status).toBe(429);
      expect(limited.headers.get('retry-after')).toBe('1');

      const unauthorized = await fetch(`${base}/401/v1/chat/completions`, { method: 'POST', body: '{}' });
      expect(unauthorized.status).toBe(401);

      const midStream = await fetch(`${base}/stream-error/v1/messages`, { method: 'POST', body: '{"stream":true}' });
      expect(midStream.status).toBe(200);
      expect(await midStream.text()).toContain('overloaded_error');
    });
  });

  test('returns 404 for unknown paths', async () => {
    await withRecorder(async (base) => {
      expect((await fetch(`${base}/v1/embeddings`, { method: 'POST', body: '{}' })).status).toBe(404);
    });
  });
});
