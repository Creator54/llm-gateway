// Golden fixtures: the wire form gw sent upstream, recorded once and checked in.
//
// These guard the egress contract per protocol: the path, the auth scheme, the
// required headers, and the body fields a provider needs. Regenerate by pointing
// gw at tests/parity/recorder.ts and copying the JSONL line into fixtures/.

import { describe, expect, test } from "bun:test";
import { readdirSync, readFileSync } from "node:fs";
import { join } from "node:path";

const FIXTURES = join(import.meta.dir, "fixtures");

type Recorded = {
  method: string;
  url: string;
  headers: [string, string][];
  body: string;
  bodyEncoding: string;
  bodyTruncated: boolean;
  response: { status: number };
};

function load(name: string): Recorded {
  const text = readFileSync(join(FIXTURES, name), "utf8").trim();
  const line = text.split("\n")[0]!;
  return JSON.parse(line) as Recorded;
}

function header(fixture: Recorded, name: string): string | undefined {
  const found = fixture.headers.find(
    ([key]) => key.toLowerCase() === name.toLowerCase(),
  );
  return found?.[1];
}

const names = readdirSync(FIXTURES).filter((name) => name.endsWith(".jsonl")).sort();

describe("golden fixtures", () => {
  test("every fixture parses and was a successful request", () => {
    expect(names.length).toBeGreaterThan(0);
    for (const name of names) {
      const fixture = load(name);
      expect(fixture.method).toBe("POST");
      expect(fixture.bodyTruncated).toBe(false);
      expect(fixture.response.status).toBe(200);
      expect(() => JSON.parse(fixture.body)).not.toThrow();
    }
  });

  test("chat fixtures hit the chat endpoint with a bearer token", () => {
    for (const name of names.filter((n) => n.startsWith("chat-"))) {
      const fixture = load(name);
      expect(fixture.url.endsWith("/chat/completions")).toBe(true);
      expect(header(fixture, "authorization")).toMatch(/^Bearer /);
      const body = JSON.parse(fixture.body);
      // gw routes on the `opencode/` prefix and must send the bare id upstream.
      expect(body.model).toBe("beam-free");
      expect(Array.isArray(body.messages)).toBe(true);
    }
  });

  test("the opencode headers are on every chat request", () => {
    for (const name of names.filter((n) => n.includes("chat") || n.includes("responses"))) {
      const fixture = load(name);
      // These are what the free tier keys its session on.
      expect(header(fixture, "x-opencode-client")).toBe("cli");
      expect(header(fixture, "x-opencode-session")).toMatch(/^ses_/);
      expect(header(fixture, "x-opencode-session-id")).toMatch(/^ses_/);
      expect(header(fixture, "x-opencode-request")).toMatch(/^msg_/);
      expect(header(fixture, "x-opencode-project")).toBeTruthy();
    }
  });

  test("tools survive the round trip when the client sends them", () => {
    const fixture = load("chat-tools.jsonl");
    const body = JSON.parse(fixture.body);
    expect(Array.isArray(body.tools)).toBe(true);
    expect(body.tools.length).toBeGreaterThan(0);
    expect(body.tools[0].function.name).toBe("lookup");
    // gw must not invent tools the client did not send.
    expect(body.tools.length).toBe(1);
  });

  test("the anthropic messages request passes through with a cache breakpoint", () => {
    const messages = load("messages-basic.jsonl");
    // Same protocol on both sides, so the path and shape are untouched.
    expect(messages.url.endsWith("/v1/messages")).toBe(true);
    const body = JSON.parse(messages.body);
    expect(body.max_tokens).toBe(32);
    expect(Array.isArray(body.system)).toBe(true);
    // Caching is the one thing gw adds by default.
    const breaks = JSON.stringify(body).match(/"cache_control"/g) ?? [];
    expect(breaks.length).toBeGreaterThan(0);
  });

  test("responses inbound is translated to the chat shape", () => {
    const responses = load("responses-basic.jsonl");
    expect(responses.url.endsWith("/chat/completions")).toBe(true);
    const body = JSON.parse(responses.body);
    // `input` is a Responses field; the chat shape has `messages`.
    expect(Array.isArray(body.messages)).toBe(true);
    expect(body.input).toBeUndefined();
  });
});
