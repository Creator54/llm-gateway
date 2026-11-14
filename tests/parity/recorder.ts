// Parity recorder: a fake upstream that logs exactly what a gateway sends it.
// Run: bun tests/parity/recorder.ts --port 10303 --out /tmp/requests.jsonl
import { appendFile, mkdir } from 'node:fs/promises';
import { dirname } from 'node:path';

export type RecordedRequest = {
  method: string;
  url: string;
  headers: [string, string][];
  bodyEncoding: 'utf8' | 'base64';
  body: string;
};

export type Scenario = 'ok' | 'rate-limit' | 'unauthorized' | 'mid-stream-error';

export type RecorderOptions = {
  port: number;
  hostname?: string;
  logFile: string;
};

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

const TEXT = 'parity-pong';

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

export function decodeBody(bytes: Uint8Array): { encoding: 'utf8' | 'base64'; value: string } {
  if (bytes.length === 0) return { encoding: 'utf8', value: '' };
  try {
    return { encoding: 'utf8', value: new TextDecoder('utf-8', { fatal: true }).decode(bytes) };
  } catch {
    return { encoding: 'base64', value: Buffer.from(bytes).toString('base64') };
  }
}

function sse(events: [string, unknown][]): Response {
  return new Response(
    events.map(([event, data]) => `event: ${event}\ndata: ${JSON.stringify(data)}\n\n`).join(''),
    { headers: { 'content-type': 'text/event-stream' } },
  );
}

function chatSse(chunks: unknown[]): Response {
  return new Response(
    chunks.map((chunk) => `data: ${JSON.stringify(chunk)}\n\n`).join('') + 'data: [DONE]\n\n',
    { headers: { 'content-type': 'text/event-stream' } },
  );
}

const responsesBody = (text: string, status = 'completed') => ({
  id: 'resp_parity',
  object: 'response',
  created_at: 1,
  status,
  model: 'parity-test',
  output: [
    {
      id: 'msg_parity',
      type: 'message',
      role: 'assistant',
      status,
      content: [{ type: 'output_text', text, annotations: [] }],
    },
  ],
  usage: { input_tokens: 1, output_tokens: 1, total_tokens: 2 },
});

const chatBody = (text: string) => ({
  id: 'chatcmpl-parity',
  object: 'chat.completion',
  created: 1,
  model: 'parity-test',
  choices: [{ index: 0, message: { role: 'assistant', content: text }, finish_reason: 'stop' }],
  usage: { prompt_tokens: 1, completion_tokens: 1, total_tokens: 2 },
});

const anthropicBody = (text: string) => ({
  id: 'msg_parity',
  type: 'message',
  role: 'assistant',
  model: 'parity-test',
  content: [{ type: 'text', text }],
  stop_reason: 'end_turn',
  stop_sequence: null,
  usage: { input_tokens: 1, output_tokens: 1 },
});

function okResponse(kind: Family, streaming: boolean, text = TEXT): Response {
  if (kind === 'responses') {
    const body = responsesBody(text);
    if (!streaming) return Response.json(body);
    return sse([
      ['response.created', { type: 'response.created', sequence_number: 0, response: { ...body, status: 'in_progress', output: [] } }],
      ['response.output_text.delta', { type: 'response.output_text.delta', sequence_number: 1, item_id: 'msg_parity', output_index: 0, content_index: 0, delta: text }],
      ['response.completed', { type: 'response.completed', sequence_number: 2, response: body }],
    ]);
  }
  if (kind === 'chat') {
    if (!streaming) return Response.json(chatBody(text));
    const chunk = { id: 'chatcmpl-parity', object: 'chat.completion.chunk', created: 1, model: 'parity-test' };
    return chatSse([
      { ...chunk, choices: [{ index: 0, delta: { role: 'assistant', content: text }, finish_reason: null }] },
      { ...chunk, choices: [{ index: 0, delta: {}, finish_reason: 'stop' }] },
    ]);
  }
  if (kind === 'anthropic') {
    const body = anthropicBody(text);
    if (!streaming) return Response.json(body);
    return sse([
      ['message_start', { type: 'message_start', message: { ...body, content: [], stop_reason: null, usage: { input_tokens: 1, output_tokens: 0 } } }],
      ['content_block_start', { type: 'content_block_start', index: 0, content_block: { type: 'text', text: '' } }],
      ['content_block_delta', { type: 'content_block_delta', index: 0, delta: { type: 'text_delta', text } }],
      ['content_block_stop', { type: 'content_block_stop', index: 0 }],
      ['message_delta', { type: 'message_delta', delta: { stop_reason: 'end_turn', stop_sequence: null }, usage: { output_tokens: 1 } }],
      ['message_stop', { type: 'message_stop' }],
    ]);
  }
  return new Response('not found', { status: 404 });
}

function errorResponse(scenario: Scenario, kind: Family, text = TEXT): Response {
  if (scenario === 'rate-limit') {
    const body =
      kind === 'anthropic'
        ? { type: 'error', error: { type: 'rate_limit_error', message: 'rate limited' } }
        : { error: { message: 'rate limited', type: 'rate_limit_error', code: 'rate_limit_exceeded' } };
    return Response.json(body, { status: 429, headers: { 'retry-after': '1' } });
  }
  if (scenario === 'unauthorized') {
    const body =
      kind === 'anthropic'
        ? { type: 'error', error: { type: 'authentication_error', message: 'invalid api key' } }
        : { error: { message: 'invalid api key', type: 'invalid_request_error', code: 'invalid_api_key' } };
    return Response.json(body, { status: 401, headers: { 'www-authenticate': 'Bearer' } });
  }
  // mid-stream-error: 200, some valid events, then an error in the stream.
  if (kind === 'responses') {
    return sse([
      ['response.created', { type: 'response.created', sequence_number: 0, response: { ...responsesBody(text), status: 'in_progress', output: [] } }],
      ['response.output_text.delta', { type: 'response.output_text.delta', sequence_number: 1, item_id: 'msg_parity', output_index: 0, content_index: 0, delta: text }],
      ['response.failed', { type: 'response.failed', sequence_number: 2, response: { id: 'resp_parity', object: 'response', status: 'failed', error: { code: 'server_error', message: 'upstream failed' } } }],
    ]);
  }
  if (kind === 'chat') {
    const chunk = { id: 'chatcmpl-parity', object: 'chat.completion.chunk', created: 1, model: 'parity-test' };
    return new Response(
      [
        { ...chunk, choices: [{ index: 0, delta: { content: text }, finish_reason: null }] },
        { error: { message: 'upstream failed', type: 'server_error' } },
      ]
        .map((part) => `data: ${JSON.stringify(part)}\n\n`)
        .join(''),
      { headers: { 'content-type': 'text/event-stream' } },
    );
  }
  if (kind === 'anthropic') {
    return sse([
      ['message_start', { type: 'message_start', message: { ...anthropicBody(text), content: [], stop_reason: null } }],
      ['content_block_delta', { type: 'content_block_delta', index: 0, delta: { type: 'text_delta', text } }],
      ['error', { type: 'error', error: { type: 'overloaded_error', message: 'upstream failed' } }],
    ]);
  }
  return new Response('upstream failed', { status: 500 });
}

function isStreaming(body: { encoding: 'utf8' | 'base64'; value: string }): boolean {
  if (body.encoding !== 'utf8') return false;
  try {
    const parsed: unknown = JSON.parse(body.value);
    return typeof parsed === 'object' && parsed !== null && (parsed as { stream?: unknown }).stream === true;
  } catch {
    return false;
  }
}

export function startRecorder(options: RecorderOptions) {
  const { port, hostname = '127.0.0.1', logFile } = options;
  let prepared: Promise<void> | undefined;

  return Bun.serve({
    port,
    hostname,
    async fetch(req: Request): Promise<Response> {
      const url = new URL(req.url);
      const body = decodeBody(new Uint8Array(await req.arrayBuffer()));

      prepared ??= mkdir(dirname(logFile), { recursive: true }).then(() => undefined);
      await prepared;
      const entry: RecordedRequest = {
        method: req.method,
        url: url.pathname + url.search,
        headers: [...req.headers] as [string, string][],
        bodyEncoding: body.encoding,
        body: body.value,
      };
      await appendFile(logFile, `${JSON.stringify(entry)}\n`);

      const { scenario, pathname } = splitScenario(url.pathname);
      const kind = family(pathname);
      if (scenario !== 'ok') return errorResponse(scenario, kind, TEXT);
      if (kind === 'unknown') return new Response('not found', { status: 404 });
      if (kind === 'models') {
        return Response.json({
          object: 'list',
          data: [{ id: 'parity-test', object: 'model', created: 1, owned_by: 'parity' }],
        });
      }
      return okResponse(kind, isStreaming(body), TEXT);
    },
  });
}

function parseArgs(argv: string[]): Record<string, string> {
  const args: Record<string, string> = {};
  for (let i = 0; i < argv.length; i++) {
    const arg = argv[i]!;
    if (!arg.startsWith('--')) continue;
    const next = argv[i + 1];
    if (next !== undefined && !next.startsWith('--')) {
      args[arg.slice(2)] = next;
      i++;
    } else {
      args[arg.slice(2)] = '';
    }
  }
  return args;
}

if (import.meta.main) {
  const args = parseArgs(process.argv.slice(2));
  const port = Number(args.port ?? '10303');
  const logFile = args.out ?? 'parity-requests.jsonl';
  if (!Number.isInteger(port) || port < 0) {
    console.error(`bad --port: ${args.port}`);
    process.exit(2);
  }
  const server = startRecorder({ port, logFile });
  console.log(`parity recorder on http://${server.hostname}:${server.port} -> ${logFile}`);
}
