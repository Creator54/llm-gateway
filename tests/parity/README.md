# Parity recorder

Records exactly what a gateway sends a provider, so the old gateway (opencodex)
and the new one (gw) can be compared request by request.

Run the fake upstream; it appends one JSON line per request to the file:

    bun tests/parity/recorder.ts --port 10303 --out /tmp/gw.jsonl

It parses HTTP/1.1 itself over raw TCP, so request header order and duplicate
headers are kept as sent. Each line has the request (method, url, headers, body)
and the response it sent back (status, headers, body). Bodies are cut at
8 MiB; use `--max-body BYTES` to change that. A cut body has
`bodyTruncated: true`.

Point a gateway's provider base URL at `http://127.0.0.1:10303/v1`, send the
same request through both gateways, and keep one file per run. The fake replies
with small canned bodies for OpenAI Responses, Chat Completions, Anthropic
Messages (streaming and not) and `/models`. `metadata.parity` in the request
body or a `-text`, `-tools`, `-reasoning` or `-image` model suffix picks a
different canned reply.

For failures, put a word in front of the path: `/429` gives 429 with
`Retry-After: 1`, `/401` gives 401, `/stream-error` breaks mid-stream. An
unknown path is always a 404, whatever the prefix.

Compare two recordings; uuids, request ids, timestamps, dates and auth headers
are ignored (add more with `--mask-header x-name`). `content-length` is
compared, not masked. Headers are compared as a name-to-values multiset; add
`--check-order` to compare wire order too. Exit code 1 means they differ, and it
prints the request index and what changed:

    bun tests/parity/diff.ts /tmp/old.jsonl /tmp/new.jsonl
