# DEAD AIR and Perennos Tempo

[Perennos Tempo](https://github.com/Perennos/perennos-tempo) is a local AI router.
DEAD AIR uses it in two optional, independent ways:

1. **Client:** `dead-air explain INCIDENT_ID` asks Tempo for a short brief about
   one incident.
2. **Server:** `dead-air mcp` lets Tempo (or any MCP host) read DEAD AIR's
   persisted state through read-only tools.

Neither is needed for monitoring. With Tempo absent, stopped or misconfigured,
`check`, `watch`, `status` and alerts behave exactly as without it.

## What never touches Tempo

Alerts are deterministic. Detection, incident opening and recovery, and alert
delivery never call Tempo, never wait on it and never depend on its answer.
`check`, `watch` and `status` do not contact Tempo at all; only `explain`,
`tempo status`, `doctor` (when `[tempo]` is enabled) and the page's Tempo button
do, and only on the loopback URL.

## Configure the client

```toml
[tempo]
enabled = true                  # opt in; false or absent means off
url = "http://127.0.0.1:8765"   # http on 127.0.0.1, [::1] or localhost only
home = ""                       # Tempo data folder; empty uses TEMPO_HOME
model = "tempo/auto"            # tempo/<route>: auto, fast, heavy, research, writer
timeout_seconds = 120           # 1..600
```

Only `enabled` is required inside the table. The gateway token comes from the
`TEMPO_GATEWAY_TOKEN` environment variable, or else from
`<home>/ledger/gateway-token` (trimmed, at least 32 characters, never a symbolic
link). DEAD AIR never prints, logs, stores or forwards the token; diagnostics only
say `token: present` or `token: missing`.

```sh
dead-air tempo status --config dead-air.toml
```

prints whether Tempo is enabled, the URL, `GET /health`, whether a token is
present and how many models `GET /v1/models` lists. Exit 0 means all three
worked; 2 means something is missing; nothing else in DEAD AIR changes.

## Incident briefs

```sh
dead-air status --config dead-air.toml          # shows open incident ids
dead-air explain 50ac21bc0fa04fb484e5afceac07a45b --config dead-air.toml
```

`explain` sends one `POST /v1/chat/completions` with `task_class`
`deadair-incident-brief`, `privacy_class` `private` and `workflow` `deadair`,
`max_tokens` 1024 and `temperature` 0.2. The request carries incident metadata
only:

- the source **label** (anything URL-shaped is replaced by `[link removed]`) and
  its kind (`direct` or `youtube`);
- the condition, open/close times, duration and whether it is ongoing;
- a metadata summary of each transition's evidence: reason codes, sample counts
  and spans, sampled versus decoded seconds, coverage completeness, the longest
  detector interval;
- delivery counts (pending/sent/failed) and the stream's detector settings.

It never sends source URLs or hosts, stream ids, destination ids or endpoints,
environment variable names or values, tokens, or media. The brief, Tempo's
`X-Tempo-Receipt-Id` and `X-Tempo-Job-Id`, and `X-Tempo-Review-Required` are stored
in the state database's `tempo_briefs` table (older releases ignore it) and shown
by `status`, `incidents`, the MCP `deadair_incident` tool and the page. A brief is
advisory ("review before use") unless Tempo explicitly says review is not
required. Look the receipt up in Tempo's hash-chained receipts to audit the route.

Errors are plain: `Perennos Tempo is not running at <url>`, `Tempo rejected the
token; rerun Tempo setup` (401), `Tempo has no qualified local engine or GPU lease
right now` (503). A timeout or reset after the request was sent is never retried:
DEAD AIR says the outcome is unknown and points at Tempo receipts.

On the page, each open incident gets an **Ask Tempo for a brief** button only
while `[tempo]` is enabled. It uses the same path and the page's per-run token.

## MCP server

```sh
dead-air mcp --config /absolute/path/dead-air.toml
```

JSON-RPC 2.0 over stdio, one JSON object per line; diagnostics go to stderr;
messages over 1 MiB are rejected with -32700. It answers `initialize` (protocol
`2025-11-25`, `2025-06-18`, `2025-03-26` or `2024-11-05`, else `2025-06-18`),
`ping`, `tools/list` and `tools/call`; notifications get no reply. Every tool has
the same guarantees as `status`: no network, no writer lock, no state writes, no
source URLs or secrets. Tool annotations say `readOnlyHint: true`,
`destructiveHint: false`, `idempotentHint: true`, `openWorldHint: false`.

| Tool | Arguments | Returns |
| --- | --- | --- |
| `deadair_status` | none | the `status --json` snapshot, including stored briefs |
| `deadair_incidents` | `state`: `open` (default) or `all`; `limit` 1–100 | incidents with ids, conditions, times, brief receipt ids |
| `deadair_incident` | `incident_id` | metadata-only evidence summary, delivery counts, stored brief |
| `deadair_deliveries` | `status`: `pending`, `sent`, `failed` or `all`; `limit` 1–100 | event and destination ids, status, attempts, last error code |
| `deadair_sources` | none | stream ids, labels, kinds and detector settings; destination ids and types |

A failing tool (for example an unknown incident id) returns a result with
`isError: true`. An unknown tool or malformed parameters are JSON-RPC -32602;
an unknown method is -32601.

### Example session

```text
→ {"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18"}}
← {"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2025-06-18","capabilities":{"tools":{"listChanged":false}},"serverInfo":{"name":"dead-air","title":"Perennos DEAD AIR","version":"0.3.0"},"instructions":"..."}}
→ {"jsonrpc":"2.0","method":"notifications/initialized"}
→ {"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"deadair_incidents","arguments":{"state":"open"}}}
← {"jsonrpc":"2.0","id":2,"result":{"content":[{"type":"text","text":"{\"incidents\":[{\"condition\":\"audio_silence\",...}],\"state_available\":true}"}],"structuredContent":{"incidents":[...],"state_available":true},"isError":false}}
```

## Wire it into Tempo

```sh
dead-air tempo mcp-config --config dead-air.toml
```

prints a secret-free MCP client entry with absolute paths:

```json
{"mcpServers": {"deadair": {"command": "/home/you/.local/bin/dead-air", "args": ["mcp", "--config", "/home/you/dead-air/dead-air.toml"]}}}
```

```sh
dead-air tempo policy
```

prints a Tempo tool-policy fragment: profile `deadair`, `max_calls_per_request` 2,
and each tool as risk `observe`, locality `local`, with its description and
parameter schema. Merge it into Tempo's policy and add your own `callers`; DEAD AIR
never generates or prints tokens. No DEAD AIR tool moves, deletes, sends or
contacts third parties, so none is `act-reversible` or `act-risky`.
