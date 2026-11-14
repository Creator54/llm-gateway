# Parity recorder

Records exactly what a gateway sends a provider, so the old gateway (opencodex)
and the new one (gw) can be compared request by request.

Run the fake upstream; it appends one JSON line per request to the file:

    bun tests/parity/recorder.ts --port 10303 --out /tmp/gw.jsonl

Point a gateway's provider base URL at `http://127.0.0.1:10303/v1`, send the
same request through both gateways, and keep one file per run. The fake replies
with small canned bodies for OpenAI Responses, Chat Completions, Anthropic
Messages (streaming and not) and `/models`.

For failures, put a word in front of the path: `/429` gives 429 with
`Retry-After: 1`, `/401` gives 401, `/stream-error` breaks mid-stream.

Compare two recordings; uuids, request ids, timestamps, dates, `content-length`
and auth headers are ignored (add more with `--mask-header x-name`). Exit code 1
means they differ, and it prints the request index and what changed:

    bun tests/parity/diff.ts /tmp/old.jsonl /tmp/new.jsonl
