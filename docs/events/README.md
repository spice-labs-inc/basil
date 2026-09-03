# Event payloads (samples)

These are **real GitHub webhook payloads** (from
[octokit/webhooks](https://github.com/octokit/webhooks/tree/main/payload-examples),
the canonical public fixture corpus), one per event token basil supports. Each
is a full valid JSON payload, exactly as GitHub would deliver it to a workflow
(`$GITHUB_EVENT_PATH`) or webhook.

| file | `ON` token | distinguishes |
|---|---|---|
| `push.json` | `push` (bare or `push(master)`) | `ref: refs/heads/master` |
| `tag-push.json` | `tag` | `ref: refs/tags/simple-tag` |
| `pull_request-opened.json` | `pr-opened` | `action: opened` |
| `pull_request-synchronize.json` | `pr-synchronized` | `action: synchronize` |
| `pull_request-merged.json` | `pr-merged` | `action: closed` + `pull_request.merged: true` |
| `pull_request-closed.json` | (unmerged close) | `action: closed` + `merged: false` — *not* a merge |

Notes:

- `pr-merged` requires `action == "closed"` **and** `pull_request.merged == true`.
  octokit's stock `closed` example is a close-without-merge, so
  `pull_request-merged.json` is the closed payload with `merged: true` and a
  `merged_at` set (the merged form GitHub sends). `pull_request-closed.json`
  is the un-merged close and deliberately does **not** match `pr-merged`.
- `push` with a tag ref (`refs/tags/…`) is the `tag` token's domain, not
  `push`; a bare `ON push` never matches a tag push.

Feed any of these to basil directly:

```sh
basil handle --event docs/events/pull_request-merged.json
# or as stdin
basil handle < docs/events/push.json
```

The test suite asserts each sample matches its intended
token (see `tests/event.rs::docs_event_fixtures_match_their_tokens`).