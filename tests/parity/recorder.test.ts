import { describe, expect, test } from 'bun:test';
import { mkdtemp, readFile } from 'node:fs/promises';
import { connect } from 'node:net';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import {
  decodeBody,
  parseRecorderArgs,
  replyKindFor,
  startRecorder,
  type RecordedRequest,
  type RecorderOptions,
} from './recorder';

async function readLog(logFile: string): Promise<RecordedRequest[]> {
  const text = await readFile(logFile, 'utf8');
  return text
    .split('\n')
    .filter((line) => line.trim().length > 0)
    .map((line) => JSON.parse(line) as RecordedRequest);
}

async function withRecorder(
  run: (base: string) => Promise<void>,
  options: Partial<RecorderOptions> = {},
): Promise<RecordedRequest[]> {
  const dir = await mkdtemp(join(tmpdir(), 'parity-recorder-'));
  const logFile = join(dir, 'requests.jsonl');
  const server = startRecorder({ port: 0, logFile, ...options });
  try {
    await run(`http://127.0.0.1:${server.port}`);
  } finally {
    server.stop(true);
  }
  return readLog(logFile);
}

/** Sends bytes straight down a socket so header wire order survives. */
function rawRequest(port: number, text: string): Promise<string> {
  return new Promise((resolve, reject) => {
    const chunks: Buffer[] = [];
    const socket = connect(port, '127.0.0.1', () => socket.write(text));
    socket.on('data', (chunk) => chunks.push(chunk));
    socket.on('end', () => resolve(Buffer.concat(chunks).toString('utf8')));
    socket.on('error', reject);
  });
}

function rawHead(headers: string[], body = ''): string {
  return `POST /v1/chat/completions HTTP/1.1\r\n${headers.join('\r\n')}\r\nContent-Length: ${body.length}\r\nConnection: close\r\n\r\n${body}`;
}

async function rawRecorder(
  run: (port: number) => Promise<void>,
): Promise<RecordedRequest[]> {
  const dir = await mkdtemp(join(tmpdir(), 'parity-raw-'));
  const logFile = join(dir, 'requests.jsonl');
  const server = startRecorder({ port: 0, logFile });
  try {
    await run(server.port);
  } finally {
    server.stop(true);
  }
  return readLog(logFile);
}

describe('decodeBody', () => {
  test('uses utf8 for text and base64 otherwise', () => {
    expect(decodeBody(new Uint8Array())).toEqual({
      bodyEncoding: 'utf8',
      body: '',
      bodyTruncated: false,
    });
    expect(decodeBody(new TextEncoder().encode('{"a":1}'))).toEqual({
      bodyEncoding: 'utf8',
      body: '{"a":1}',
      bodyTruncated: false,
    });
    expect(decodeBody(new Uint8Array([0xff, 0xfe, 0x00]).slice(0, 2)).bodyEncoding).toBe('base64');
  });

  test('caps the kept body and flags truncation', () => {
    const decoded = decodeBody(new TextEncoder().encode('0123456789abc'), 10);
    expect(decoded.body).toBe('0123456789');
    expect(decoded.bodyTruncated).toBe(true);
  });
});

describe('recorder', () => {
  test('logs method, path+query, body and the response it sent', async () => {
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
    expect(entry!.body).toBe('{"model":"parity-test","stream":false}');
    expect(entry!.bodyEncoding).toBe('utf8');

    const response = entry!.response!;
    expect(response.status).toBe(200);
    expect(response.headers).toContainEqual(['content-type', 'application/json']);
    expect(response.headers).toContainEqual(['content-length', String(Buffer.byteLength(response.body))]);
    expect((JSON.parse(response.body) as { status: string }).status).toBe('completed');
  });

  test('keeps raw header wire order and duplicates', async () => {
    const [first] = await rawRecorder((port) =>
      rawRequest(
        port,
        rawHead([
          'Host: parity',
          'X-Dup: one',
          'Content-Type: application/json',
          'x-dup: two',
          'Accept: */*',
        ]),
      ),
    );
    expect(first!.headers).toEqual([
      ['host', 'parity'],
      ['x-dup', 'one'],
      ['content-type', 'application/json'],
      ['x-dup', 'two'],
      ['accept', '*/*'],
      ['content-length', '0'],
      ['connection', 'close'],
    ]);

    const [reordered] = await rawRecorder((port) =>
      rawRequest(port, rawHead(['Host: parity', 'Accept: */*', 'x-dup: one', 'Content-Type: application/json'])),
    );
    expect(reordered!.headers.slice(0, 3)).toEqual([
      ['host', 'parity'],
      ['accept', '*/*'],
      ['x-dup', 'one'],
    ]);
  });

  test('reads chunked request bodies', async () => {
    const body = '{"stream":false}';
    const [entry] = await rawRecorder((port) =>
      rawRequest(
        port,
        `POST /v1/responses HTTP/1.1\r\nHost: parity\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n${body.length.toString(16)}\r\n${body}\r\n0\r\n\r\n`,
      ),
    );
    expect(entry!.body).toBe(body);
  });

  test('caps recorded bodies with maxBody', async () => {
    const [entry] = await withRecorder(
      async (base) => {
        await fetch(`${base}/v1/responses`, { method: 'POST', body: 'x'.repeat(4096) });
      },
      { maxBody: 32 },
    );
    expect(entry!.bodyTruncated).toBe(true);
    expect(entry!.body).toHaveLength(32);
    expect(entry!.response!.bodyTruncated).toBe(true);
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

  test('returns 404 for unknown paths, even behind a scenario prefix', async () => {
    await withRecorder(async (base) => {
      expect((await fetch(`${base}/v1/embeddings`, { method: 'POST', body: '{}' })).status).toBe(404);
      expect((await fetch(`${base}/429/v1/embeddings`, { method: 'POST', body: '{}' })).status).toBe(404);
      expect((await fetch(`${base}/429`)).status).toBe(404);
    });
  });

  test('picks the canned reply from metadata.parity or a model suffix', async () => {
    await withRecorder(async (base) => {
      const tools = (await (
        await fetch(`${base}/v1/responses`, { method: 'POST', body: '{"metadata":{"parity":"tools"}}' })
      ).json()) as { output: { type: string }[] };
      expect(tools.output[0]!.type).toBe('function_call');

      const reasoning = (await (
        await fetch(`${base}/v1/messages`, { method: 'POST', body: '{"model":"claude-parity-reasoning"}' })
      ).json()) as { content: { type: string }[] };
      expect(reasoning.content[0]!.type).toBe('thinking');

      const image = (await (
        await fetch(`${base}/v1/chat/completions`, { method: 'POST', body: '{"metadata":{"parity":"image"}}' })
      ).json()) as { choices: { message: { content: { type: string }[] } }[] };
      expect(image.choices[0]!.message.content[1]).toMatchObject({ type: 'image_url' });

      const streamed = await (
        await fetch(`${base}/v1/responses`, {
          method: 'POST',
          body: '{"stream":true,"metadata":{"parity":"tools"}}',
        })
      ).text();
      expect(streamed).toContain('"function_call"');
    });
  });
});

describe('replyKindFor', () => {
  const kind = (text: string) => replyKindFor(Buffer.from(text));

  test('prefers metadata.parity', () => {
    expect(kind('{"metadata":{"parity":"image"},"model":"x-tools"}')).toBe('image');
    expect(kind('{"metadata":{"parity":"bogus"}}')).toBe('text');
  });

  test('falls back to the model suffix', () => {
    expect(kind('{"model":"parity-tools"}')).toBe('tools');
    expect(kind('{"model":"plain"}')).toBe('text');
    expect(kind('not json')).toBe('text');
  });
});

describe('parseRecorderArgs', () => {
  test('reads flags with defaults', () => {
    expect(parseRecorderArgs([])).toEqual({ port: 10303, logFile: 'parity-requests.jsonl', maxBody: 8388608 });
    expect(parseRecorderArgs(['--port', '0', '--out', '/tmp/x.jsonl', '--max-body', '16'])).toEqual({
      port: 0,
      logFile: '/tmp/x.jsonl',
      maxBody: 16,
    });
  });

  test('rejects unknown flags and missing or bad values', () => {
    expect(() => parseRecorderArgs(['--nope'])).toThrow('unknown argument');
    expect(() => parseRecorderArgs(['--port'])).toThrow('missing value');
    expect(() => parseRecorderArgs(['--out', '--port', '1'])).toThrow('missing value');
    expect(() => parseRecorderArgs(['--port', 'abc'])).toThrow('bad --port');
    expect(() => parseRecorderArgs(['--max-body', '-1'])).toThrow('bad --max-body');
  });
});
