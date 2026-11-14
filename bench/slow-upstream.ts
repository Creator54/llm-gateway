// Fake upstream that streams slowly, so a gateway in front of it holds open
// streams while we measure its memory. Speaks Responses and Chat SSE.
//
// bun run bench/slow-upstream.ts [--port 10399] [--interval 50] [--tokens 400]
//   --interval  ms between token deltas (default 50)
//   --tokens    number of deltas per stream (default 400 -> 20 s at 50 ms)

const args = Bun.argv.slice(2);
const opt = (name: string, fallback: number) => {
  const i = args.indexOf(`--${name}`);
  return i >= 0 && args[i + 1] ? Number(args[i + 1]) : fallback;
};
const port = opt('port', 10399);
const interval = opt('interval', 50);
const tokens = opt('tokens', 400);

const sleep = (ms: number) => new Promise((r) => setTimeout(r, ms));

const sse = (type: string, data: unknown) => {
  // Chat chunks carry no event name; Responses events do. A plain string is a raw data line.
  if (typeof data === 'string') return `data: ${data}\n\n`;
  return type
    ? `event: ${type}\ndata: ${JSON.stringify({ type, ...(data as object) })}\n\n`
    : `data: ${JSON.stringify(data)}\n\n`;
};

function responsesEvents(text: string, n: number): [string, unknown][] {
  const out: [string, unknown][] = [
    ['response.created', { response: { id: 'resp_bench', object: 'response', status: 'in_progress', model: 'bench', output: [] } }],
    ['response.output_item.added', { output_index: 0, item: { id: 'msg_bench', type: 'message', role: 'assistant', status: 'in_progress', content: [] } }],
    ['response.content_part.added', { item_id: 'msg_bench', output_index: 0, content_index: 0, part: { type: 'output_text', text: '', annotations: [] } }],
  ];
  for (let i = 0; i < n; i++) {
    out.push(['response.output_text.delta', { item_id: 'msg_bench', output_index: 0, content_index: 0, delta: text }]);
  }
  out.push(
    ['response.output_text.done', { item_id: 'msg_bench', output_index: 0, content_index: 0, text: text.repeat(n) }],
    ['response.output_item.done', { output_index: 0, item: { id: 'msg_bench', type: 'message', role: 'assistant', status: 'completed', content: [{ type: 'output_text', text: text.repeat(n), annotations: [] }] } }],
    ['response.completed', { response: { id: 'resp_bench', object: 'response', status: 'completed', model: 'bench', output: [], usage: { input_tokens: 1, output_tokens: n, total_tokens: n + 1 } } }],
  );
  return out;
}

function chatEvents(text: string, n: number): [string, unknown][] {
  const chunk = (delta: object, finish: string | null = null) => ({
    id: 'chatcmpl_bench', object: 'chat.completion.chunk', created: 1, model: 'bench',
    choices: [{ index: 0, delta, finish_reason: finish }],
  });
  const out: [string, unknown][] = [['', chunk({ role: 'assistant', content: '' })]];
  for (let i = 0; i < n; i++) out.push(['', chunk({ content: text })]);
  out.push(['', chunk({}, 'stop')], ['', '[DONE]']);
  return out;
}

Bun.serve({
  port,
  hostname: '127.0.0.1',
  async fetch(req) {
    const path = new URL(req.url).pathname;
    if (path.endsWith('/models')) return Response.json({ object: 'list', data: [] });
    const isResponses = path.endsWith('/responses');
    if (!isResponses && !path.endsWith('/chat/completions')) {
      return new Response('not found', { status: 404 });
    }
    const body = (await req.json().catch(() => ({}))) as { stream?: boolean };
    const events = isResponses ? responsesEvents('x', tokens) : chatEvents('x', tokens);

    if (!body.stream) {
      const text = 'x'.repeat(tokens);
      const json = isResponses
        ? { id: 'resp_bench', object: 'response', status: 'completed', model: 'bench', output: [{ id: 'msg_bench', type: 'message', role: 'assistant', status: 'completed', content: [{ type: 'output_text', text, annotations: [] }] }], usage: { input_tokens: 1, output_tokens: tokens, total_tokens: tokens + 1 } }
        : { id: 'chatcmpl_bench', object: 'chat.completion', created: 1, model: 'bench', choices: [{ index: 0, message: { role: 'assistant', content: text }, finish_reason: 'stop' }] };
      return Response.json(json);
    }

    const stream = new ReadableStream({
      async start(controller) {
        const enc = new TextEncoder();
        for (const [type, data] of events) {
          controller.enqueue(enc.encode(sse(type, data)));
          await sleep(interval);
        }
        controller.close();
      },
    });
    return new Response(stream, {
      headers: { 'Content-Type': 'text/event-stream', 'Cache-Control': 'no-cache' },
    });
  },
});

console.log(`slow upstream on http://127.0.0.1:${port} (${tokens} deltas, ${interval} ms apart, ~${Math.round((tokens * interval) / 1000)} s per stream)`);
