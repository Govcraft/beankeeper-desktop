# Agent navigation

Desktop composes `ui_show_view`, `ui_highlight` and `ui_status` with the upstream
ledger MCP server at `/mcp`. It uses upstream `http_router_with_host_tools` and
`HostToolHandler`, pinned to `a81ed93` or later. Upstream authenticates each
request, filters discovery by signed grants, checks live expiry/revocation and
passes the request's verified identity to the Desktop callback. Desktop has no
parallel authentication or ledger tool implementation.

## Grant and client setup

The human owner issues a **separate** UI token using the existing encrypted
ledger and upstream identity commands:

```sh
bk --db /path/to/books.db --passphrase-file /private/passphrase --json \
  agent issue --name desktop_navigation --generate-key --caps ui/navigate --ttl 8h
```

Keep the returned token private. Configure a Streamable HTTP client with the
Desktop `/mcp` endpoint and that token as `Authorization: Bearer <token>`.
Clients that cannot change the bearer per request should configure a second MCP
server entry for UI tools. A second `bk mcp bridge` entry with its own private
UI-token file also works for stdio clients. Both entries reach the same Desktop
listener. Never use the human approval identity for either connection.

Upstream issues UI tokens under `agent://ROOT/ui/AGENT_ID`. Ledger tokens retain
`agent://ROOT/ledger/AGENT_ID`. Mixed `ledger/read,ui/navigate` grants are invalid;
ledger read, post and admin never imply navigation. UI tokens never imply ledger
read. With ledger policy disabled, upstream's existing policy-off ledger behavior
still applies. UI tool grants are always required, including with policy off.
Use `bk agent revoke` and `bk agent list` for the same token registry as usual.

The current setup presets remain ledger-only: read-only supplies `ledger/read`;
bookkeeper adds posting and proposals. Future onboarding may issue a second UI
token and client entry alongside these presets. It cannot add `ui/navigate` to
the existing ledger token. Existing setup credentials need no rotation for this
feature, and do not gain navigation rights automatically.

`--read-only` removes `ui_show_view` and `ui_highlight` from discovery and
dispatch even with a UI grant. `ui_status` remains available to UI credentials.
A credential without `ui/navigate` receives upstream's MCP `FORBIDDEN` result.
Explicit invalid credentials override fallback credentials and fail authentication.

## Pairing and tab selection

First pair a browser using the one-time terminal URL from `bk serve`. Call
`ui_status` with the UI token, open its association URL in that paired browser,
and confirm **Connect agent to this browser**. The association URL alone cannot
log a browser in. Pairing a browser opens its navigation channel but does not
associate an agent. The live notice asks for the agent connection link until
association, then says **Agent linked. Browser navigation ready.** This reflects
the browser's association and channel, not whether an agent process is running.
Already-open tabs update their notice when the association changes. A UI token
and a ledger token have separate associations even when used through the same
MCP transport session.

Each browser tab registers a public TypeID, its current URL/title and the IDs of
highlightable elements. It then opens a cookie-authenticated SSE connection.
Registration and acknowledgements require the existing D3 Origin and CSRF
checks; agent bearer headers are rejected. Public tab IDs grant no authority.
The server checks D3 session validity during every SSE tick. Logout, expiry or
replacement of the stream, or server shutdown stops delivery. Disconnected tabs stop being targets;
stale registrations expire after three seconds. Restart invalidates all state.

If one associated tab is connected, tools select it automatically. If more than
one is connected, call `ui_status` and pass an explicit `tab_id`. A tab belonging
to another browser association cannot be selected by guessing its ID. Pending
events carry their originating identity and are discarded if the human connects
a different agent to that browser. At most 128 tabs and 32 pending events per tab
are retained. A full queue produces a useful busy error.

## Tool catalog

| Tool | Arguments | Result |
| --- | --- | --- |
| `ui_show_view` | `view_id`, optional `state`, optional `tab_id` | Queues an addressable view, or describes invalid filters / the no-tab fallback. |
| `ui_highlight` | `target`, optional `tab_id` | Queues a highlight by exact DOM ID. Missing targets are a successful no-op. |
| `ui_status` | Optional `tab_id` | Pairing state, connected tabs, confirmed URLs/titles, highlighted IDs, available targets, pending event counts and an association URL. |

View IDs map to the real server-rendered pages:

| `view_id` | Route | Detail state |
| --- | --- | --- |
| `dashboard` | `/` | Optional company/currency. |
| `accounts` | `/accounts` | Optional company. |
| `transactions` | `/transactions` | Company, account and register filters. |
| `transaction` | `/transactions/ID` | Positive `transaction_id`, company. |
| `review` | `/review` | Optional company. |
| `review_detail` | `/review/ID` | `proposal_id`, company. |
| `reports` | `/reports`, or the report selected by `report_kind` | Optional company/currency; report-specific filters below. |
| `income-statement`, `balance-sheet`, `cash-flow`, `trend`, `budget-variance`, `schedule-c`, `1099` | `/reports/VIEW_ID` | Company/currency and the report's filters. |
| `reconciliation` | `/reconciliation` | Company/currency, account and reconciliation filters. |

`state` accepts `company`, `account`, `from`, `to`, `min_amount`, `max_amount`,
`search`, `status`, `offset`, `currency`, `transaction_id`, `proposal_id`,
`report_kind`, `period`, `metric`, `year`, `threshold`, `cash_account`, `mode`,
`statement`, `statement_balance`, and `link_key`. Date, amount, clearance and
offset validation reuses the real view validator. Amount bounds are strings containing nonnegative whole
minor units. Reports accept the stable [report IDs and URL filters](reports.md),
either directly as `view_id` or through `view_id: "reports"` with `report_kind`.
`period` and `metric` apply to trends, `year` to budget variance, `threshold` to
1099, and `cash_account` to cash flow. Reconciliation accepts `account`, `to`,
`mode`, `statement`, `statement_balance` and `link_key`. These values restore
through the view's ordinary GET form. Unsupported filter/view combinations fail
rather than dropping filters. Report option validation reuses the report query
contract without reading ledger data. Initial budget and reconciliation pages
can open without a year or account. Navigation opens HTML views; download formats
remain in the browser's download links. Register periods use inclusive
`from`/`to` dates. Review views accept company and
their detail ID only. Unknown fields and unsupported IDs fail clearly. State is
encoded into a local URL of at most 4096 bytes; tools accept no arbitrary URL,
script, CSS selector or approval action.

Example calls:

```json
{"name":"ui_show_view","arguments":{"view_id":"transactions","state":{"company":"acme","account":"1000","from":"2026-01-01","to":"2026-12-31","search":"rent"}}}
{"name":"ui_show_view","arguments":{"view_id":"reports","state":{"report_kind":"trend","company":"acme","metric":"net-worth","period":"quarter","from":"2026-01-01","to":"2026-12-31"}}}
{"name":"ui_show_view","arguments":{"view_id":"reconciliation","state":{"company":"acme","account":"1000","to":"2026-03-31","statement":"S1","statement_balance":"1234.56"}}}
{"name":"ui_status","arguments":{}}
{"name":"ui_highlight","arguments":{"target":"main","tab_id":"TAB_ID_FROM_STATUS"}}
```

Navigation tool results include readable text as well as structured content.
With no connected matching tab, `ui_show_view` and `ui_highlight` return
`isError: true` with structured `code: "NO_CONNECTED_TAB"`, `message`,
`queued: false` and `pairing_url`. This applies before association, when an
associated browser has no live tab, and when an explicit `tab_id` cannot be
reached by that credential. Open the association link in a paired browser,
confirm the connection, keep a tab open and retry. Requests are not saved for
a browser that opens later. `ui_status` remains successful without tabs and
reports `paired`, an empty `tabs` list and an association link. A missing
highlight target in a connected tab remains a successful no-op. Other error
results explain invalid filters or unknown views and include a connection link.
No ledger counts or totals are fetched with a navigation grant; the text describes the requested view and its exact filters.
Use the separate ledger connection for financial queries.

A queued event is not a claim that the browser displayed it. The browser
acknowledges its loaded URL/title and applied highlight separately. `ui_status`
reports those acknowledgements. A view's own missing-record or empty-data state
continues to render normally. Known highlight IDs include `main`,
`account-CODE`, `transaction-ID`, review's `verification`, and reports'
`report-filters` and `report-results` regions; `ui_status` lists
only IDs the current page reported. If a target disappears before delivery, the
browser clears the highlight and reports a no-op. Highlighting never clicks or
submits controls. Browser approval retains D7's separate human boundary.

## Browser implementation and verification

A small embedded `/assets/navigation.js` enhances the existing HTML with
same-origin `EventSource` and form-encoded acknowledgements. There is no Node or
frontend build dependency. Without JavaScript, links, filters, pairing and review
forms continue to work, but the tab is not a live navigation target. CSP permits
same-origin scripts and connections while continuing to forbid inline scripts,
external scripts, objects and framing. The server sends an SSE tick every 250 ms.
Each event includes an `agent_associated` boolean from the revalidated browser
session, so opening the stream alone never claims an agent is linked. Events
are transient and are not replayed as human actions on reconnect.

Tests use a real listener and temporary encrypted ledger. They cover discovery,
policy-on/off grants, credential switching and revocation, fallback and read-only
ceilings, readable no-tab results, report/reconciliation routes and restored
filters, report region highlights, invalid report options, filter encoding, SSE
navigation/highlights, acknowledgements, multiple tabs, agent isolation, logout,
and the cookie/Origin/CSRF boundary. Shutdown drains open UI streams.
No schema migration is introduced.

An optional real Chromium regression check exercises the embedded receiver,
filters, stable tab IDs across full page loads, acknowledgement, highlights,
two-tab isolation, accurate pre-association notices, explicit no-tab errors,
live association updates in already-open tabs and error-page reconnection.
It requires Python with `requests` and `playwright`, plus Chromium on PATH or a Playwright browser.
After building the Desktop binary, run:

```sh
python3 scripts/test-navigation-browser.py --binary /path/to/target/debug/bk
# Optionally select a browser: --chromium /path/to/chromium
```

The script creates and removes its own temporary encrypted demo ledger beneath
this repository's ignored `target/` directory. It uses only synthetic credentials
and never connects to an existing Desktop or ledger.
