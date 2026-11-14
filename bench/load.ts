// Load generator for the memory bench: N concurrent streaming requests against
// a gateway, drained to the end, reporting successes, errors and first-byte
// percentiles. No dependencies, Bun only.
//
// bun bench/load.ts [--url URL] [--requests N] [--chat] [--key KEY] [--timeout MS]
//   --chat      use /v1/chat/completions instead of /v1/responses
//   GW_URL, GW_KEY, GW_REQUESTS are read when the flags are absent.

type Result = { ok: boolean; firstByteMs: number | null; totalMs: number; error?: string };

const args = Bun.argv.slice(2);
const flag = (name: string) => args.includes(`--${name}`);
const value = (name: string): string | undefined => {
  const i = args.indexOf(`--${name}`);
  return i >= 0 ? args[i + 1] : undefined;
};

const base = (value('url') ?? process.env.GW_URL ?? 'http://127.0.0.1:10204').replace(/\/$/, '');
const key = value('key') ?? process.env.GW_KEY;
const count = Number(value('requests') ?? process.env.GW_REQUESTS ?? 50);
const timeoutMs = Number(value('timeout') ?? 120_000);
const chat = flag('chat');
const path = chat ? '/v1/chat/completions' : '/v1/responses';

if (!Number.isInteger(count) || count < 1) {
  console.error('--requests must be a positive integer');
  process.exit(2);
}

const headers: Record<string, string> = { 'Content-Type': 'application/json' };
if (key) headers.Authorization = `Bearer ${key}`;

const payload = chat
  ? { model: value('model') ?? 'bench', messages: [{ role: 'user', content: 'bench' }], stream: true }
  : { model: value('model') ?? 'bench', input: 'bench', stream: true };

async function one(): Promise<Result> {
  const started = performance.now();
  let firstByteMs: number | null = null;
  try {
    const res = await fetch(base + path, {
      method: 'POST',
      headers,
      body: JSON.stringify(payload),
      signal: AbortSignal.timeout(timeoutMs),
    });
    if (!res.body) throw new Error(`HTTP ${res.status} with no body`);
    const reader = res.body.getReader();
    for (;;) {
      const { done, value: chunk } = await reader.read();
      if (firstByteMs === null && (chunk?.length ?? 0) > 0) firstByteMs = performance.now() - started;
      if (done) break;
    }
    return {
      ok: res.ok && firstByteMs !== null,
      firstByteMs,
      totalMs: performance.now() - started,
      error: res.ok ? undefined : `HTTP ${res.status}`,
    };
  } catch (e) {
    return {
      ok: false,
      firstByteMs,
      totalMs: performance.now() - started,
      error: e instanceof Error ? e.message : String(e),
    };
  }
}

function pct(values: number[], p: number): number | null {
  if (values.length === 0) return null;
  const sorted = [...values].sort((a, b) => a - b);
  return sorted[Math.min(sorted.length - 1, Math.max(0, Math.ceil(p * sorted.length) - 1))];
}

const ms = (v: number | null) => (v === null ? 'n/a' : `${Math.round(v)} ms`);

console.log(`load: ${count} streams -> ${base}${path}`);
const started = performance.now();
const results = await Promise.all(Array.from({ length: count }, one));
const elapsed = performance.now() - started;

const ok = results.filter((r) => r.ok);
const firstBytes = ok.map((r) => r.firstByteMs!).filter((v) => v !== null);
const totals = ok.map((r) => r.totalMs);
console.log(`successes: ${ok.length}/${count}`);
console.log(`errors:    ${count - ok.length}`);
console.log(`first byte: p50 ${ms(pct(firstBytes, 0.5))}  p95 ${ms(pct(firstBytes, 0.95))}`);
console.log(`total:      p50 ${ms(pct(totals, 0.5))}  p95 ${ms(pct(totals, 0.95))}  wall ${(elapsed / 1000).toFixed(1)} s`);

for (const r of results.filter((r) => !r.ok).slice(0, 3)) {
  console.error(`  failed: ${r.error ?? 'no first byte'}`);
}
process.exit(ok.length === count ? 0 : 1);
