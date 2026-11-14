// Compare two recordings request by request, ignoring volatile values.
// Run: bun tests/parity/diff.ts a.jsonl b.jsonl [--mask-header x-foo] [--check-order]
import { readFileSync } from 'node:fs';
import { normalizeRequest, type NormalizeOptions } from './normalize';
import type { RecordedHeaders, RecordedRequest, RecordedResponse } from './recorder';

type FieldDifference = { field: string; expected: string; actual: string };

export type RequestDifference = {
  index: number;
  path: string;
  fields: FieldDifference[];
};

export type DiffOptions = NormalizeOptions & {
  /** Compare header name order too; off by default because order is not semantic. */
  checkOrder?: boolean;
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

/** Header name to sorted values, so duplicates are compared as a multiset. */
function headerMultiset(headers: RecordedHeaders): Map<string, string[]> {
  const map = new Map<string, string[]>();
  for (const [name, value] of headers) {
    const values = map.get(name);
    if (values) values.push(value);
    else map.set(name, [value]);
  }
  for (const values of map.values()) values.sort();
  return map;
}

function headerDifferences(
  expected: RecordedHeaders,
  actual: RecordedHeaders,
  checkOrder: boolean,
): FieldDifference[] {
  const differences: FieldDifference[] = [];
  const left = headerMultiset(expected);
  const right = headerMultiset(actual);
  const names = [...new Set([...left.keys(), ...right.keys()])].sort();
  for (const name of names) {
    const a = left.get(name);
    const b = right.get(name);
    if (!a) {
      differences.push({ field: `header "${name}" (extra)`, expected: '<none>', actual: JSON.stringify(b) });
    } else if (!b) {
      differences.push({ field: `header "${name}" (missing)`, expected: JSON.stringify(a), actual: '<none>' });
    } else if (JSON.stringify(a) !== JSON.stringify(b)) {
      differences.push({ field: `header "${name}"`, expected: JSON.stringify(a), actual: JSON.stringify(b) });
    }
  }
  if (checkOrder) {
    const orderA = expected.map(([name]) => name);
    const orderB = actual.map(([name]) => name);
    if (JSON.stringify(orderA) !== JSON.stringify(orderB)) {
      differences.push({ field: 'header order', expected: orderA.join(', '), actual: orderB.join(', ') });
    }
  }
  return differences;
}

function bodyDifferences(
  left: { bodyEncoding: string; body: string; bodyTruncated: boolean },
  right: { bodyEncoding: string; body: string; bodyTruncated: boolean },
): FieldDifference[] {
  const fields: FieldDifference[] = [];
  if (left.bodyTruncated !== right.bodyTruncated) {
    fields.push({
      field: 'body truncated',
      expected: String(left.bodyTruncated),
      actual: String(right.bodyTruncated),
    });
  }
  if (left.bodyEncoding !== right.bodyEncoding) {
    fields.push({ field: 'body encoding', expected: left.bodyEncoding, actual: right.bodyEncoding });
  } else if (left.body !== right.body) {
    fields.push(bodyDifference(left.body, right.body));
  }
  return fields;
}

function responseDifferences(
  expected: RecordedResponse,
  actual: RecordedResponse,
  checkOrder: boolean,
): FieldDifference[] {
  const fields: FieldDifference[] = [];
  if (expected.status !== actual.status) {
    fields.push({ field: 'response status', expected: String(expected.status), actual: String(actual.status) });
  }
  fields.push(...bodyDifferences(expected, actual).map((field) => ({ ...field, field: `response ${field.field}` })));
  fields.push(
    ...headerDifferences(expected.headers, actual.headers, checkOrder).map((field) => ({
      ...field,
      field: `response ${field.field}`,
    })),
  );
  return fields;
}

export function diffRequest(
  expected: RecordedRequest,
  actual: RecordedRequest,
  index: number,
  options: DiffOptions = {},
): RequestDifference {
  const a = normalizeRequest(expected, options);
  const b = normalizeRequest(actual, options);
  const checkOrder = options.checkOrder === true;
  const fields: FieldDifference[] = [];
  if (a.method !== b.method) fields.push({ field: 'method', expected: a.method, actual: b.method });
  if (a.url !== b.url) fields.push({ field: 'url', expected: a.url, actual: b.url });
  fields.push(...bodyDifferences(a, b));
  fields.push(...headerDifferences(a.headers, b.headers, checkOrder));
  if (a.response && b.response) fields.push(...responseDifferences(a.response, b.response, checkOrder));
  return { index, path: a.url, fields };
}

export function diffLogs(
  expected: RecordedRequest[],
  actual: RecordedRequest[],
  options: DiffOptions = {},
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

export type DiffArgs = {
  files: string[];
  masks: string[];
  checkOrder: boolean;
};

export function parseDiffArgs(argv: string[]): DiffArgs {
  const files: string[] = [];
  const masks: string[] = [];
  let checkOrder = false;
  for (let i = 0; i < argv.length; i++) {
    const arg = argv[i]!;
    if (arg === '--check-order') {
      checkOrder = true;
      continue;
    }
    if (arg === '--mask-header') {
      const value = argv[i + 1];
      if (value === undefined || value.startsWith('--')) throw new Error('missing value for --mask-header');
      masks.push(value);
      i++;
      continue;
    }
    if (arg.startsWith('--')) throw new Error(`unknown argument: ${arg}`);
    files.push(arg);
  }
  return { files, masks, checkOrder };
}

if (import.meta.main) {
  let args: DiffArgs;
  try {
    args = parseDiffArgs(process.argv.slice(2));
  } catch (error) {
    console.error(error instanceof Error ? error.message : String(error));
    process.exit(2);
  }
  if (args.files.length !== 2) {
    console.error(
      'usage: bun tests/parity/diff.ts <expected.jsonl> <actual.jsonl> [--mask-header name] [--check-order]',
    );
    process.exit(2);
  }
  const [expectedFile, actualFile] = args.files as [string, string];
  let expected: RecordedRequest[];
  let actual: RecordedRequest[];
  try {
    expected = parseLog(readFileSync(expectedFile, 'utf8'));
    actual = parseLog(readFileSync(actualFile, 'utf8'));
  } catch (error) {
    console.error(error instanceof Error ? error.message : String(error));
    process.exit(2);
  }
  const differences = diffLogs(expected, actual, { headers: args.masks, checkOrder: args.checkOrder });
  if (differences.length === 0) {
    console.log(`${expected.length} request(s) match`);
    process.exit(0);
  }
  console.log(formatDifferences(differences));
  console.log(`\n${differences.length} of ${Math.max(expected.length, actual.length)} request(s) differ`);
  process.exit(1);
}
