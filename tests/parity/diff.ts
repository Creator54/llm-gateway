// Compare two recordings request by request, ignoring volatile values.
// Run: bun tests/parity/diff.ts a.jsonl b.jsonl [--mask-header x-foo]
import { readFileSync } from 'node:fs';
import { normalizeRequest, type NormalizeOptions } from './normalize';
import type { RecordedRequest } from './recorder';

type FieldDifference = { field: string; expected: string; actual: string };

export type RequestDifference = {
  index: number;
  path: string;
  fields: FieldDifference[];
};

const MAX_SHOWN = 400;

function shorten(text: string): string {
  return text.length > MAX_SHOWN ? `${text.slice(0, MAX_SHOWN)}…` : text;
}

/** First differing line, so big bodies stay readable. */
function bodyDifference(expected: string, actual: string): FieldDifference {
  const left = expected.split('\n');
  const right = actual.split('\n');
  const lines = Math.max(left.length, right.length);
  for (let line = 0; line < lines; line++) {
    if (left[line] !== right[line]) {
      return {
        field: `body line ${line + 1}`,
        expected: shorten(left[line] ?? '<missing>'),
        actual: shorten(right[line] ?? '<missing>'),
      };
    }
  }
  return { field: 'body', expected: shorten(expected), actual: shorten(actual) };
}

function headerMap(headers: [string, string][]): Map<string, string[]> {
  const map = new Map<string, string[]>();
  for (const [name, value] of headers) {
    const values = map.get(name);
    if (values) values.push(value);
    else map.set(name, [value]);
  }
  return map;
}

function headerDifferences(expected: [string, string][], actual: [string, string][]): FieldDifference[] {
  const differences: FieldDifference[] = [];
  const left = headerMap(expected);
  const right = headerMap(actual);
  const names = [...new Set([...left.keys(), ...right.keys()])].sort();
  for (const name of names) {
    const a = left.get(name) ?? [];
    const b = right.get(name) ?? [];
    if (JSON.stringify(a) !== JSON.stringify(b)) {
      differences.push({ field: `header "${name}"`, expected: JSON.stringify(a), actual: JSON.stringify(b) });
    }
  }
  const orderA = expected.map(([name]) => name);
  const orderB = actual.map(([name]) => name);
  if (JSON.stringify(orderA) !== JSON.stringify(orderB)) {
    differences.push({ field: 'header order', expected: orderA.join(', '), actual: orderB.join(', ') });
  }
  return differences;
}

export function diffRequest(
  expected: RecordedRequest,
  actual: RecordedRequest,
  index: number,
  options: NormalizeOptions = {},
): RequestDifference {
  const a = normalizeRequest(expected, options);
  const b = normalizeRequest(actual, options);
  const fields: FieldDifference[] = [];
  if (a.method !== b.method) fields.push({ field: 'method', expected: a.method, actual: b.method });
  if (a.url !== b.url) fields.push({ field: 'url', expected: a.url, actual: b.url });
  if (a.bodyEncoding !== b.bodyEncoding) {
    fields.push({ field: 'body encoding', expected: a.bodyEncoding, actual: b.bodyEncoding });
  } else if (a.body !== b.body) {
    fields.push(bodyDifference(a.body, b.body));
  }
  fields.push(...headerDifferences(a.headers, b.headers));
  return { index, path: a.url, fields };
}

export function diffLogs(
  expected: RecordedRequest[],
  actual: RecordedRequest[],
  options: NormalizeOptions = {},
): RequestDifference[] {
  const differences: RequestDifference[] = [];
  const count = Math.max(expected.length, actual.length);
  for (let index = 0; index < count; index++) {
    const left = expected[index];
    const right = actual[index];
    if (!left || !right) {
      differences.push({
        index,
        path: (left ?? right)?.url ?? '?',
        fields: [
          {
            field: 'request',
            expected: left ? 'present' : '<missing>',
            actual: right ? 'present' : '<missing>',
          },
        ],
      });
      continue;
    }
    const difference = diffRequest(left, right, index, options);
    if (difference.fields.length > 0) differences.push(difference);
  }
  return differences;
}

export function parseLog(text: string): RecordedRequest[] {
  return text
    .split('\n')
    .map((line) => line.trim())
    .filter((line) => line.length > 0)
    .map((line, lineIndex) => {
      try {
        return JSON.parse(line) as RecordedRequest;
      } catch {
        throw new Error(`line ${lineIndex + 1}: not valid JSON`);
      }
    });
}

export function formatDifferences(differences: RequestDifference[]): string {
  if (differences.length === 0) return 'no differences';
  const blocks = differences.map((difference) => {
    const fields = difference.fields.map(
      (field) => `  ${field.field}\n    expected: ${field.expected}\n    actual:   ${field.actual}`,
    );
    return [`request ${difference.index} (${difference.path}) differs`, ...fields].join('\n');
  });
  return blocks.join('\n\n');
}

function parseArgs(argv: string[]): { files: string[]; masks: string[] } {
  const files: string[] = [];
  const masks: string[] = [];
  for (let i = 0; i < argv.length; i++) {
    const arg = argv[i]!;
    if (arg === '--mask-header') {
      const next = argv[++i];
      if (next) masks.push(next);
    } else if (!arg.startsWith('--')) {
      files.push(arg);
    }
  }
  return { files, masks };
}

if (import.meta.main) {
  const { files, masks } = parseArgs(process.argv.slice(2));
  if (files.length !== 2) {
    console.error('usage: bun tests/parity/diff.ts <expected.jsonl> <actual.jsonl> [--mask-header name]');
    process.exit(2);
  }
  const [expectedFile, actualFile] = files as [string, string];
  let expected: RecordedRequest[];
  let actual: RecordedRequest[];
  try {
    expected = parseLog(readFileSync(expectedFile, 'utf8'));
    actual = parseLog(readFileSync(actualFile, 'utf8'));
  } catch (error) {
    console.error(error instanceof Error ? error.message : String(error));
    process.exit(2);
  }
  const differences = diffLogs(expected, actual, { headers: masks });
  if (differences.length === 0) {
    console.log(`${expected.length} request(s) match`);
    process.exit(0);
  }
  console.log(formatDifferences(differences));
  console.log(`\n${differences.length} of ${Math.max(expected.length, actual.length)} request(s) differ`);
  process.exit(1);
}
