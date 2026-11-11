# 008 — AI providers and the MCP server

Status: open
Severity: medium
Area: `crates/bugle-ai/src/{providers.rs,lib.rs}`, `crates/bugle-api/src/mcp.rs`

## AI providers

1. **`truncate` panics on multi-byte text.** `&s[..600]` (providers.rs:345) slices at a byte offset; an error body or
   model reply with a multi-byte character across byte 600 panics the request thread.
2. `run_command` substitutes placeholders one after another over the whole argument (providers.rs:245-251), so a prompt
   that contains `{output}`, `{language}` or `{model}` gets those expanded too. Substitute in a single pass.
3. `run_command` has no timeout (`timeout_seconds` only configures the HTTP client) and does not kill the child when
   the request is dropped.
4. `Client::builder()...unwrap_or_else(|_| Client::new())` silently loses the timeout.
5. Whole audio files are read into memory for upload; stream the file instead (reqwest `stream` feature is enabled).
6. `supports()` says an `openai` provider can transcribe even with no model configured, and that `ollama`/`anthropic`
   providers with no `base_url`/`model` are usable; errors only appear at call time. `describe_all` should report
   a `"ready": false` plus a reason for providers that are missing required settings.
7. `parse_srt` drops cues separated by blank lines containing spaces and mis-parses timestamps with trailing cue
   settings (`00:01.000 --> 00:02.000 align:start`).
8. OpenAI-compatible chat: `content` may be an array of parts, or null with a `refusal`; today both produce
   "unexpected chat response".

## MCP server

9. **Messages without an `id` get a response.** JSON-RPC notifications must never be answered; mcp.rs only skips
   methods starting with `notifications/`. An id-less `ping` or unknown method gets a reply with `"id": null`.
10. `initialize` ignores the client's `protocolVersion` and always answers `2024-11-05`. Answer with the client's
    version when it is one this server supports, otherwise the server's latest.
11. One slow `tools/call` (a 60-second recording, a model call) blocks the read loop, so `ping` and every other
    request wait. Run calls concurrently and serialise writes.
12. Batch arrays and non-object messages are answered as if they were a single request; unknown tool names come back
    as a tool error rather than `-32602`.
13. `tools/call` puts the full JSON in both `content[0].text` and `structuredContent`, doubling every response;
    keep both (clients differ) but make the text compact rather than pretty-printed.

## Wanted

Fix each item, with unit tests. For the MCP loop, factor the per-message handling into a function that takes a
message and returns `Option<Value>` so it can be tested without stdio; test notifications, version negotiation,
unknown method, unknown tool and a batch. Tests must not need Screen Recording permission or a network.

## Out of scope

`actions.rs`, `http.rs`.
