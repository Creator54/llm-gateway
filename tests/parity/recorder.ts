// Parity recorder: a fake upstream that logs exactly what a gateway sends it.
// Run: bun tests/parity/recorder.ts --port 10303 --out /tmp/requests.jsonl
import { appendFile, mkdir } from 'node:fs/promises';
import { dirname } from 'node:path';
import type { Socket } from 'bun';

export type RecordedHeaders = [string, string][];

export type RecordedBody = {
  bodyEncoding: 'utf8' | 'base64';
  body: string;
  bodyTruncated: boolean;
};

export type RecordedResponse = RecordedBody & {
  status: number;
  headers: RecordedHeaders;
};

export type RecordedRequest = RecordedBody & {
  method: string;
  url: string;
  headers: RecordedHeaders;
  response?: RecordedResponse;
};

export type Scenario = 'ok' | 'rate-limit' | 'unauthorized' | 'mid-stream-error';

export type ReplyKind = 'text' | 'tools' | 'reasoning' | 'image';

export type RecorderOptions = {
  port: number;
  hostname?: string;
  logFile: string;
  /** Recorded request and response bodies are cut at this many bytes. */
  maxBody?: number;
};

export const DEFAULT_MAX_BODY = 8 * 1024 * 1024;
export const TEXT = 'parity-pong';
const IMAGE_DATA = 'cGFyaXR5';

// Prefix the path with a scenario segment, e.g. base URL http://host:port/429
// turns /v1/responses into /429/v1/responses.
const SCENARIO_SEGMENTS: Record<string, Scenario> = {
  '429': 'rate-limit',
  'rate-limit': 'rate-limit',
  '401': 'unauthorized',
  unauthorized: 'unauthorized',
  'stream-error': 'mid-stream-error',
  'mid-stream-error': 'mid-stream-error',
};

type Family = 'responses' | 'chat' | 'anthropic' | 'models' | 'unknown';

function splitScenario(pathname: string): { scenario: Scenario; pathname: string } {
  const [, first, ...rest] = pathname.split('/');
  const scenario = first ? SCENARIO_SEGMENTS[first.toLowerCase()] : undefined;
  if (!scenario) return { scenario: 'ok', pathname };
  return { scenario, pathname: `/${rest.join('/')}` };
}

function family(pathname: string): Family {
  if (pathname.endsWith('/chat/completions')) return 'chat';
  if (pathname.endsWith('/responses')) return 'responses';
  if (pathname.endsWith('/messages')) return 'anthropic';
  if (pathname.endsWith('/models')) return 'models';
  return 'unknown';
}

export function decodeBody(bytes: Uint8Array, maxBody = DEFAULT_MAX_BODY): RecordedBody {
  const bodyTruncated = bytes.length > maxBody;
  const kept = bodyTruncated ? bytes.subarray(0, maxBody) : bytes;
  if (kept.length === 0) return { bodyEncoding: 'utf8', body: '', bodyTruncated };
  try {
    const decoder = new TextDecoder('utf-8', { fatal: !bodyTruncated });
    return { bodyEncoding: 'utf8', body: decoder.decode(kept), bodyTruncated };
  } catch {
    return { bodyEncoding: 'base64', body: Buffer.from(kept).toString('base64'), bodyTruncated };
  }
}

type Reply = {
  status: number;
  headers: RecordedHeaders;
  body: string;
};

function textReply(status: number, body: string): Reply {
  return { status, headers: [['content-type', 'text/plain; charset=utf-8']], body };
}

function jsonReply(body: unknown, status = 200, headers: RecordedHeaders = []): Reply {
  return {
    status,
    headers: [['content-type', 'application/json'], ...headers],
    body: JSON.stringify(body),
  };
}

function sseReply(events: [string, unknown][]): Reply {
  return {
    status: 200,
    headers: [['content-type', 'text/event-stream']],
    body: events.map(([event, data]) => `event: ${event}\ndata: ${JSON.stringify(data)}\n\n`).join(''),
  };
}

function chatSseReply(chunks: unknown[]): Reply {
  return {
    status: 200,
    headers: [['content-type', 'text/event-stream']],
    body: chunks.map((chunk) => `data: ${JSON.stringify(chunk)}\n\n`).join('') + 'data: [DONE]\n\n',
  };
}

function responsesItems(kind: ReplyKind, text: string): Record<string, unknown>[] {
  const message = {
    id: 'msg_parity',
    type: 'message',
    role: 'assistant',
    status: 'completed',
    content: [{ type: 'output_text', text, annotations: [] }],
  };
  if (kind === 'tools') {
    return [
      {
        id: 'fc_parity',
        type: 'function_call',
        call_id: 'call_parity',
        name: 'parity_tool',
        arguments: '{"city":"parity"}',
        status: 'completed',
      },
    ];
  }
  if (kind === 'reasoning') {
    return [
      { id: 'rs_parity', type: 'reasoning', summary: [{ type: 'summary_text', text: 'parity-thought' }] },
      message,
    ];
  }
  if (kind === 'image') {
    return [{ id: 'ig_parity', type: 'image_generation_call', status: 'completed', result: IMAGE_DATA }, message];
  }
  return [message];
}

function responsesBody(items: Record<string, unknown>[], status = 'completed'): Record<string, unknown> {
  return {
    id: 'resp_parity',
    object: 'response',
    created_at: 1,
    status,
    model: 'parity-test',
    output: items,
    usage: { input_tokens: 1, output_tokens: 1, total_tokens: 2 },
  };
}

function responsesReply(kind: ReplyKind, streaming: boolean, text: string): Reply {
  const items = responsesItems(kind, text);
  const body = responsesBody(items);
  if (!streaming) return jsonReply(body);
  const created: [string, unknown] = [
    'response.created',
    {
      type: 'response.created',
      sequence_number: 0,
      response: { ...body, status: 'in_progress', output: [] },
    },
  ];
  if (kind === 'text') {
    return sseReply([
      created,
      [
        'response.output_text.delta',
        {
          type: 'response.output_text.delta',
          sequence_number: 1,
          item_id: 'msg_parity',
          output_index: 0,
          content_index: 0,
          delta: text,
        },
      ],
      ['response.completed', { type: 'response.completed', sequence_number: 2, response: body }],
    ]);
  }
  return sseReply([
    created,
    ...items.map((item, index): [string, unknown] => [
      'response.output_item.added',
      { type: 'response.output_item.added', sequence_number: index + 1, output_index: index, item },
    ]),
    [
      'response.completed',
      { type: 'response.completed', sequence_number: items.length + 1, response: body },
    ],
  ]);
}

type ChatBody = {
  id: string;
  object: string;
  created: number;
  model: string;
  choices: { index: number; message: Record<string, unknown>; finish_reason: string }[];
  usage: { prompt_tokens: number; completion_tokens: number; total_tokens: number };
};

function chatMessage(kind: ReplyKind, text: string): Record<string, unknown> {
  if (kind === 'tools') {
    return {
      role: 'assistant',
      content: null,
      tool_calls: [
        { id: 'call_parity', type: 'function', function: { name: 'parity_tool', arguments: '{"city":"parity"}' } },
      ],
    };
  }
  if (kind === 'reasoning') return { role: 'assistant', content: text, reasoning_content: 'parity-thought' };
  if (kind === 'image') {
    return {
      role: 'assistant',
      content: [
        { type: 'text', text },
        { type: 'image_url', image_url: { url: `data:image/png;base64,${IMAGE_DATA}` } },
      ],
    };
  }
  return { role: 'assistant', content: text };
}

function chatBody(kind: ReplyKind, text: string): ChatBody {
  return {
    id: 'chatcmpl-parity',
    object: 'chat.completion',
    created: 1,
    model: 'parity-test',
    choices: [
      {
        index: 0,
        message: chatMessage(kind, text),
        finish_reason: kind === 'tools' ? 'tool_calls' : 'stop',
      },
    ],
    usage: { prompt_tokens: 1, completion_tokens: 1, total_tokens: 2 },
  };
}

function chatReply(kind: ReplyKind, streaming: boolean, text: string): Reply {
  const body = chatBody(kind, text);
  if (!streaming) return jsonReply(body);
  const chunk = { id: body.id, object: 'chat.completion.chunk', created: body.created, model: body.model };
  if (kind === 'text') {
    return chatSseReply([
      { ...chunk, choices: [{ index: 0, delta: { role: 'assistant', content: text }, finish_reason: null }] },
      { ...chunk, choices: [{ index: 0, delta: {}, finish_reason: 'stop' }] },
    ]);
  }
  const message = body.choices[0]!.message;
  const delta =
    kind === 'tools'
      ? { role: 'assistant', content: null, tool_calls: message.tool_calls }
      : kind === 'reasoning'
        ? { role: 'assistant', content: message.content, reasoning_content: 'parity-thought' }
        : { role: 'assistant', content: message.content };
  return chatSseReply([
    { ...chunk, choices: [{ index: 0, delta, finish_reason: null }] },
    { ...chunk, choices: [{ index: 0, delta: {}, finish_reason: body.choices[0]!.finish_reason }] },
  ]);
}

type AnthropicBody = {
  id: string;
  type: string;
  role: string;
  model: string;
  content: Record<string, unknown>[];
  stop_reason: string;
  stop_sequence: null;
  usage: { input_tokens: number; output_tokens: number };
};

function anthropicBlocks(kind: ReplyKind, text: string): Record<string, unknown>[] {
  if (kind === 'tools') {
    return [{ type: 'tool_use', id: 'toolu_parity', name: 'parity_tool', input: { city: 'parity' } }];
  }
  if (kind === 'reasoning') {
    return [
      { type: 'thinking', thinking: 'parity-thought', signature: 'parity-signature' },
      { type: 'text', text },
    ];
  }
  if (kind === 'image') {
    return [
      { type: 'text', text },
      { type: 'image', source: { type: 'base64', media_type: 'image/png', data: IMAGE_DATA } },
    ];
  }
  return [{ type: 'text', text }];
}

function anthropicBody(kind: ReplyKind, text: string): AnthropicBody {
  return {
    id: 'msg_parity',
    type: 'message',
    role: 'assistant',
    model: 'parity-test',
    content: anthropicBlocks(kind, text),
    stop_reason: kind === 'tools' ? 'tool_use' : 'end_turn',
    stop_sequence: null,
    usage: { input_tokens: 1, output_tokens: 1 },
  };
}

function anthropicReply(kind: ReplyKind, streaming: boolean, text: string): Reply {
  const body = anthropicBody(kind, text);
  if (!streaming) return jsonReply(body);
  if (kind === 'text') {
    return sseReply([
      [
        'message_start',
        {
          type: 'message_start',
          message: { ...body, content: [], stop_reason: null, usage: { input_tokens: 1, output_tokens: 0 } },
        },
      ],
      ['content_block_start', { type: 'content_block_start', index: 0, content_block: { type: 'text', text: '' } }],
      ['content_block_delta', { type: 'content_block_delta', index: 0, delta: { type: 'text_delta', text } }],
      ['content_block_stop', { type: 'content_block_stop', index: 0 }],
      [
        'message_delta',
        { type: 'message_delta', delta: { stop_reason: 'end_turn', stop_sequence: null }, usage: { output_tokens: 1 } },
      ],
      ['message_stop', { type: 'message_stop' }],
    ]);
  }
  const events: [string, unknown][] = [
    [
      'message_start',
      {
        type: 'message_start',
        message: { ...body, content: [], stop_reason: null, usage: { input_tokens: 1, output_tokens: 0 } },
      },
    ],
  ];
  body.content.forEach((block, index) => {
    events.push(['content_block_start', { type: 'content_block_start', index, content_block: block }]);
    events.push(['content_block_stop', { type: 'content_block_stop', index }]);
  });
  events.push(
    [
      'message_delta',
      {
        type: 'message_delta',
        delta: { stop_reason: body.stop_reason, stop_sequence: null },
        usage: { output_tokens: 1 },
      },
    ],
    ['message_stop', { type: 'message_stop' }],
  );
  return sseReply(events);
}

function okReply(kind: Family, streaming: boolean, replyKind: ReplyKind, text: string): Reply {
  if (kind === 'responses') return responsesReply(replyKind, streaming, text);
  if (kind === 'chat') return chatReply(replyKind, streaming, text);
  if (kind === 'anthropic') return anthropicReply(replyKind, streaming, text);
  return textReply(404, 'not found');
}

function errorReply(scenario: Scenario, kind: Family, text: string): Reply {
  if (scenario === 'rate-limit') {
    const body =
      kind === 'anthropic'
        ? { type: 'error', error: { type: 'rate_limit_error', message: 'rate limited' } }
        : { error: { message: 'rate limited', type: 'rate_limit_error', code: 'rate_limit_exceeded' } };
    return jsonReply(body, 429, [['retry-after', '1']]);
  }
  if (scenario === 'unauthorized') {
    const body =
      kind === 'anthropic'
        ? { type: 'error', error: { type: 'authentication_error', message: 'invalid api key' } }
        : { error: { message: 'invalid api key', type: 'invalid_request_error', code: 'invalid_api_key' } };
    return jsonReply(body, 401, [['www-authenticate', 'Bearer']]);
  }
  // mid-stream-error: 200, some valid events, then an error in the stream.
  if (kind === 'responses') {
    const started = responsesBody(responsesItems('text', text), 'in_progress');
    return sseReply([
      [
        'response.created',
        { type: 'response.created', sequence_number: 0, response: { ...started, output: [] } },
      ],
      [
        'response.output_text.delta',
        {
          type: 'response.output_text.delta',
          sequence_number: 1,
          item_id: 'msg_parity',
          output_index: 0,
          content_index: 0,
          delta: text,
        },
      ],
      [
        'response.failed',
        {
          type: 'response.failed',
          sequence_number: 2,
          response: {
            id: 'resp_parity',
            object: 'response',
            status: 'failed',
            error: { code: 'server_error', message: 'upstream failed' },
          },
        },
      ],
    ]);
  }
  if (kind === 'chat') {
    const chunk = { id: 'chatcmpl-parity', object: 'chat.completion.chunk', created: 1, model: 'parity-test' };
    return chatSseReply([
      { ...chunk, choices: [{ index: 0, delta: { content: text }, finish_reason: null }] },
      { error: { message: 'upstream failed', type: 'server_error' } },
    ]);
  }
  if (kind === 'anthropic') {
    return sseReply([
      [
        'message_start',
        {
          type: 'message_start',
          message: {
            ...anthropicBody('text', text),
            content: [],
            stop_reason: null,
            usage: { input_tokens: 1, output_tokens: 0 },
          },
        },
      ],
      ['content_block_delta', { type: 'content_block_delta', index: 0, delta: { type: 'text_delta', text } }],
      ['error', { type: 'error', error: { type: 'overloaded_error', message: 'upstream failed' } }],
    ]);
  }
  return textReply(500, 'upstream failed');
}

function isStreaming(body: Buffer): boolean {
  try {
    const parsed: unknown = JSON.parse(new TextDecoder('utf-8', { fatal: true }).decode(body));
    return typeof parsed === 'object' && parsed !== null && (parsed as { stream?: unknown }).stream === true;
  } catch {
    return false;
  }
}

function isReplyKind(value: unknown): value is ReplyKind {
  return value === 'text' || value === 'tools' || value === 'reasoning' || value === 'image';
}

/** body.metadata.parity or a model suffix picks the canned reply shape. */
export function replyKindFor(body: Buffer): ReplyKind {
  let parsed: Record<string, unknown> | undefined;
  try {
    const value: unknown = JSON.parse(new TextDecoder('utf-8', { fatal: true }).decode(body));
    if (typeof value === 'object' && value !== null) parsed = value as Record<string, unknown>;
  } catch {
    return 'text';
  }
  const metadata = parsed.metadata;
  if (typeof metadata === 'object' && metadata !== null) {
    const parity = (metadata as { parity?: unknown }).parity;
    if (isReplyKind(parity)) return parity;
  }
  const model = typeof parsed.model === 'string' ? parsed.model : '';
  const suffix = model.slice(model.lastIndexOf('-') + 1);
  return isReplyKind(suffix) ? suffix : 'text';
}

type ParsedRequest = {
  method: string;
  target: string;
  version: string;
  headers: RecordedHeaders;
  body: Buffer;
  keepAlive: boolean;
};

const MAX_HEAD_BYTES = 64 * 1024;

function headerValue(headers: RecordedHeaders, name: string): string | undefined {
  for (const [key, value] of headers) if (key === name) return value;
  return undefined;
}

function parseChunked(
  buffer: Buffer,
): { body: Buffer; consumed: number } | 'incomplete' | 'invalid' {
  const parts: Buffer[] = [];
  let offset = 0;
  for (;;) {
    const lineEnd = buffer.indexOf('\r\n', offset);
    if (lineEnd === -1) return 'incomplete';
    const size = Number.parseInt(buffer.subarray(offset, lineEnd).toString('latin1').split(';', 1)[0]!.trim(), 16);
    if (!Number.isInteger(size) || size < 0) return 'invalid';
    offset = lineEnd + 2;
    if (size === 0) {
      for (;;) {
        const trailerEnd = buffer.indexOf('\r\n', offset);
        if (trailerEnd === -1) return 'incomplete';
        const line = buffer.subarray(offset, trailerEnd).toString('latin1');
        offset = trailerEnd + 2;
        if (line === '') return { body: Buffer.concat(parts), consumed: offset };
      }
    }
    if (buffer.length < offset + size + 2) return 'incomplete';
    if (buffer[offset + size] !== 0x0d || buffer[offset + size + 1] !== 0x0a) return 'invalid';
    parts.push(buffer.subarray(offset, offset + size));
    offset += size + 2;
  }
}

function extractRequest(buffer: Buffer): { request: ParsedRequest; consumed: number } | 'incomplete' | 'invalid' {
  const headEnd = buffer.indexOf('\r\n\r\n');
  if (headEnd === -1) return buffer.length > MAX_HEAD_BYTES ? 'invalid' : 'incomplete';
  const [requestLine, ...headerLines] = buffer.subarray(0, headEnd).toString('latin1').split('\r\n');
  const match = /^([A-Z]+) (\S+) (HTTP\/\d\.\d)$/.exec(requestLine ?? '');
  if (!match) return 'invalid';
  const [, method, target, version] = match;
  const headers: RecordedHeaders = [];
  for (const line of headerLines) {
    const colon = line.indexOf(':');
    if (colon <= 0) return 'invalid';
    headers.push([line.slice(0, colon).trim().toLowerCase(), line.slice(colon + 1).trim()]);
  }
  const bodyStart = headEnd + 4;
  let body = Buffer.alloc(0);
  let consumed = bodyStart;
  const transferEncoding = headerValue(headers, 'transfer-encoding')?.toLowerCase() ?? '';
  if (transferEncoding.includes('chunked')) {
    const chunked = parseChunked(buffer.subarray(bodyStart));
    if (chunked === 'incomplete' || chunked === 'invalid') return chunked;
    body = chunked.body;
    consumed += chunked.consumed;
  } else {
    const contentLength = headerValue(headers, 'content-length');
    if (contentLength !== undefined) {
      const length = Number(contentLength);
      if (!Number.isInteger(length) || length < 0) return 'invalid';
      if (buffer.length < bodyStart + length) return 'incomplete';
      body = buffer.subarray(bodyStart, bodyStart + length);
      consumed += length;
    }
  }
  const connection = headerValue(headers, 'connection')?.toLowerCase() ?? '';
  const keepAlive = version === 'HTTP/1.0' ? connection.includes('keep-alive') : !connection.includes('close');
  return { request: { method: method!, target: target!, version: version!, headers, body, keepAlive }, consumed };
}

function requestUrl(target: string): string {
  try {
    const url = new URL(target, 'http://recorder');
    return url.pathname + url.search;
  } catch {
    return target;
  }
}

function handleRequest(request: ParsedRequest): Reply {
  const url = new URL(request.target, 'http://recorder');
  const { scenario, pathname } = splitScenario(url.pathname);
  const kind = family(pathname);
  if (kind === 'unknown') return textReply(404, 'not found');
  const replyKind = replyKindFor(request.body);
  if (scenario !== 'ok') return errorReply(scenario, kind, TEXT);
  if (kind === 'models') {
    return jsonReply({
      object: 'list',
      data: [{ id: 'parity-test', object: 'model', created: 1, owned_by: 'parity' }],
    });
  }
  return okReply(kind, isStreaming(request.body), replyKind, TEXT);
}

const REASON_PHRASES: Record<number, string> = {
  200: 'OK',
  400: 'Bad Request',
  401: 'Unauthorized',
  404: 'Not Found',
  429: 'Too Many Requests',
  500: 'Internal Server Error',
};

function serialize(status: number, headers: RecordedHeaders, body: Buffer): Buffer {
  const lines = [
    `HTTP/1.1 ${status} ${REASON_PHRASES[status] ?? 'Unknown'}`,
    ...headers.map(([name, value]) => `${name}: ${value}`),
  ];
  return Buffer.concat([Buffer.from(`${lines.join('\r\n')}\r\n\r\n`, 'latin1'), body]);
}

type Connection = {
  buffer: Buffer;
  pending: ParsedRequest[];
  processing: boolean;
  closed: boolean;
};

export type Recorder = {
  hostname: string;
  port: number;
  stop: (force?: boolean) => void;
};

export function startRecorder(options: RecorderOptions): Recorder {
  const { port, hostname = '127.0.0.1', logFile, maxBody = DEFAULT_MAX_BODY } = options;
  let prepared: Promise<void> | undefined;

  const log = async (entry: RecordedRequest): Promise<void> => {
    prepared ??= mkdir(dirname(logFile), { recursive: true }).then(() => undefined);
    await prepared;
    await appendFile(logFile, `${JSON.stringify(entry)}\n`);
  };

  async function pump(socket: Socket<Connection>): Promise<void> {
    const state = socket.data;
    if (state.processing) return;
    state.processing = true;
    try {
      while (state.pending.length > 0 && !state.closed) {
        const request = state.pending.shift()!;
        const reply = handleRequest(request);
        const body = Buffer.from(reply.body, 'utf8');
        const headers: RecordedHeaders = [
          ...reply.headers,
          ['content-length', String(body.length)],
          ['connection', request.keepAlive ? 'keep-alive' : 'close'],
        ];
        const entry: RecordedRequest = {
          method: request.method,
          url: requestUrl(request.target),
          headers: request.headers,
          ...decodeBody(request.body, maxBody),
          response: {
            status: reply.status,
            headers,
            ...decodeBody(body, maxBody),
          },
        };
        await log(entry);
        socket.write(serialize(reply.status, headers, body));
        if (!request.keepAlive) {
          socket.end();
          break;
        }
      }
    } finally {
      state.processing = false;
    }
  }

  function read(socket: Socket<Connection>): void {
    const state = socket.data;
    for (;;) {
      if (state.buffer.length === 0) break;
      const result = extractRequest(state.buffer);
      if (result === 'incomplete') break;
      if (result === 'invalid') {
        state.buffer = Buffer.alloc(0);
        const headers: RecordedHeaders = [
          ['content-type', 'text/plain; charset=utf-8'],
          ['content-length', '11'],
          ['connection', 'close'],
        ];
        socket.end(serialize(400, headers, Buffer.from('bad request')));
        return;
      }
      state.buffer = state.buffer.subarray(result.consumed);
      state.pending.push(result.request);
    }
    void pump(socket);
  }

  const listener = Bun.listen<Connection>({
    hostname,
    port,
    socket: {
      open(socket) {
        socket.data = { buffer: Buffer.alloc(0), pending: [], processing: false, closed: false };
      },
      data(socket, chunk) {
        const bytes = typeof chunk === 'string' ? Buffer.from(chunk, 'latin1') : Buffer.from(chunk);
        socket.data.buffer = Buffer.concat([socket.data.buffer, bytes]);
        read(socket);
      },
      close(socket) {
        socket.data.closed = true;
      },
      error(socket) {
        socket.data.closed = true;
      },
    },
  });

  return {
    hostname,
    port: listener.port,
    stop: () => listener.stop(true),
  };
}

export type RecorderArgs = {
  port: number;
  logFile: string;
  maxBody: number;
};

export function parseRecorderArgs(argv: string[]): RecorderArgs {
  const valued = new Set(['--port', '--out', '--max-body']);
  const values = new Map<string, string>();
  for (let i = 0; i < argv.length; i++) {
    const arg = argv[i]!;
    if (!valued.has(arg)) throw new Error(`unknown argument: ${arg}`);
    const value = argv[i + 1];
    if (value === undefined || value.startsWith('--')) throw new Error(`missing value for ${arg}`);
    values.set(arg, value);
    i++;
  }
  const port = Number(values.get('--port') ?? '10303');
  if (!Number.isInteger(port) || port < 0) throw new Error(`bad --port: ${values.get('--port')}`);
  const maxBody = Number(values.get('--max-body') ?? String(DEFAULT_MAX_BODY));
  if (!Number.isInteger(maxBody) || maxBody < 0) {
    throw new Error(`bad --max-body: ${values.get('--max-body')}`);
  }
  return { port, logFile: values.get('--out') ?? 'parity-requests.jsonl', maxBody };
}

if (import.meta.main) {
  let args: RecorderArgs;
  try {
    args = parseRecorderArgs(process.argv.slice(2));
  } catch (error) {
    console.error(error instanceof Error ? error.message : String(error));
    process.exit(2);
  }
  const server = startRecorder({ port: args.port, logFile: args.logFile, maxBody: args.maxBody });
  console.log(`parity recorder on http://${server.hostname}:${server.port} -> ${args.logFile}`);
}
