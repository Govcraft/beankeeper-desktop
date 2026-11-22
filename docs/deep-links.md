# Optional Desktop detail links

The agent answers in plain words and can draw its own chart from ledger data.
A Desktop link is an optional way to inspect the matching detail. Generating a
link never opens a tab, changes a view, highlights a row, or posts to the ledger.

`ui_get_link` is advertised as read-only on the managed ledger MCP connection.
It requires `ledger/read`, works under `--read-only`, and accepts `view_id` plus
an optional `state` object. It needs no browser pairing or UI credential. It
returns readable text and this structured shape:

```json
{
  "url": "http://127.0.0.1:8750/transactions?company=acme&account=5000&from=2026-10-01&to=2026-10-31&currency=USD",
  "path": "/transactions?company=acme&account=5000&from=2026-10-01&to=2026-10-31&currency=USD",
  "view_id": "transactions",
  "state": {
    "company": "acme",
    "account": "5000",
    "from": "2026-10-01",
    "to": "2026-10-31",
    "currency": "USD"
  }
}
```

Applicable optional state fields are returned as `null`; filters belonging to other
views are omitted. No credential is included.
`url` uses the actual listener port. `path` is local and URL-encoded, bounded to
4096 bytes. Unknown views, fields, mismatched filters, invalid dates or IDs and
oversized addresses return `isError: true` with `code: "VALIDATION"` and a
readable message. This never becomes a no-tab error. Navigation continues to
use its existing error names and browser acknowledgements.

The CLI returns exactly the same structured payload with `--json` or
`--format json`, and prints just the URL in text mode:

```sh
bk --json ui link --view-id transactions --company acme --account 5000 \
  --currency USD --from 2026-10-01 --to 2026-10-31
bk ui link --view-id transaction --company acme --transaction-id 42
bk ui link --view-id review_detail --company acme --proposal-id PROPOSAL_TYPE_ID
bk --db /path/to/demo.db ui link --view-id accounts --bind '[::1]:8751'
```

CLI flag names use hyphens, and MCP state keys use underscores. The CLI selects
the configured managed books by default when available. Supply `--db` or
`BEANKEEPER_DB` for other books. The CLI canonicalizes that path and confirms
the process serving it through a local health check against an owner-private
record. Link generation needs no passphrase, agent token or license activation.
The CLI makes no ledger reads. If a managed instance for the requested books is
live, it takes priority. `--bind` selects a specific listener and still checks
that it serves the requested books. It accepts canonical IPv4 and IPv6 loopback
addresses with a nonzero port.

`DESKTOP_BOOKS_NOT_SERVED` means no live instance matches the request.
`DESKTOP_MULTIPLE_INSTANCES` means that multiple foreground instances serve the
books and the caller can use `--bind` to select one. Answer in words if a detail
link is unavailable. If state omits a company, the CLI uses upstream's
`--company` or `BEANKEEPER_COMPANY` default for ledger views. JSON errors contain
`code` and `message`, exit nonzero, and also print a diagnostic to stderr.

## Stable views

| ID | URL path | Accepted filters |
| --- | --- | --- |
| `dashboard` | `/` | `company`, `currency` |
| `financial-summary` | `/summary` | `company`, `currency`, date and comparison bounds, `cash_account`, `receivable_account`, `payable_account` |
| `accounts` | `/accounts` | `company` |
| `transactions` | `/transactions` | `company`, `account`, `category`, `payee`, `from`, `to`, `compare_from`, `compare_to`, `currency`, `search`, `status`, `min_amount`, `max_amount`, `offset` |
| `transaction` | `/transactions/ID` | `transaction_id` (positive integer in the path), `company`, audit `offset` |
| `reports` | `/reports` | `company`, `currency`; or `report_kind` selecting one of the reports below with its filters |
| `income-statement` | `/reports/income-statement` | `company`, `currency`, `from`, `to`, comparison bounds |
| `balance-sheet` | `/reports/balance-sheet` | `company`, `currency`, `to`, as-of `compare_to` |
| `cash-flow` | `/reports/cash-flow` | `company`, `currency`, `from`, `to`, `cash_account`, comparison bounds |
| `trend` | `/reports/trend` | `company`, `currency`, `from`, `to`, `period`, `metric`, comparison bounds |
| `budget-variance` | `/reports/budget-variance` | `company`, `currency`, `year` or whole-month `from`/`to`, comparison bounds |
| `schedule-c` | `/reports/schedule-c` | `company`, `currency`, `from`, `to`, comparison bounds |
| `1099` | `/reports/1099` | `company`, `currency`, `from`, `to`, `threshold`, comparison bounds |
| `reconciliation` | `/reconciliation` | `company`, `currency`, `account`, `to`, `mode`, `statement`, `statement_balance`, `link_key` |
| `review` | `/review` | `company` |
| `review_detail` | `/review/ID` | `proposal_id` (proposal TypeID in the path), `company` |
| `settings` | `/settings` | None |

Report details and defaults follow the [report contract](reports.md). A link
selects a view, not a download or an approval action. Record links do not verify
existence while generating the URL; an absent record produces the view's normal
not-found state when opened. Existing dashboard links remain supported for
compatibility, but agents should offer the matching detail view after answering.

`financial_summary` returns ready-to-use arguments in `data.desktop_view.link`.
Pass its `view_id` and `state` to `ui_get_link` after answering the question.

## Address types and URL compatibility

CLI flags, MCP `ui_get_link`, `ui_show_view`, and saved-URL parsing all deserialize
into the same `ViewAddress` enum, tagged by `view_id`. Each variant owns only its
supported filter set. The wire envelope remains `view_id` plus optional `state`;
`ui_show_view` also accepts `tab_id`. The generated MCP schema describes each
variant separately, including required detail IDs and rejecting unknown filters.
`reports` with `state.report_kind` remains a compatibility spelling, represented
by a second tagged enum with the same per-report filter types.

Existing paths, query names, escaping, filter units, default selections, and
positive integer transaction IDs remain supported. Proposal IDs reuse the upstream
proposal TypeID. Report aliases canonicalize to the direct report path, including
`/reconciliation`. Decimal values and numeric strings can canonicalize their sign
or leading zeros; fractional precision is preserved. An unsupported filter key is
now rejected even when its value is `null`; it cannot be carried into an unrelated
view. Empty date and amount strings are invalid; omit an unused filter or use
`null`. These changes tighten input validation without introducing a URL version.

Register amount bounds are inclusive integer minor units. Include `currency`
when the company records transactions in multiple currencies.

Compile-time invariants (tenet 2): the enum and per-view filter structs prevent
attaching register filters to reports, report-specific filters to another report,
or record IDs to another detail view. Dates, amounts, clearance, currency, account
codes, years and IDs use types, including upstream ledger types where available.
Validated filter sets and scalar wrappers have private fields and no unchecked
constructors, so callers cannot mutate a validated value into an invalid one.

Runtime invariants: serde parses untrusted values once into those types and checks
relationships between values, such as chronological dates, paired comparison
bounds, amount ordering, matching budget years and whole calendar months. These
depend on the supplied values and cannot be proved by the compiler. Exact decimal
precision and SQLite bounds are checked against an explicit currency at this
boundary; when currency is omitted, the view resolves the ledger currency and
upstream performs its currency-dependent check. Record existence also requires a
ledger read. URL serialization only encodes the typed values and enforces the
4096-byte resource limit; it has no view/filter validation matrix. CLI and MCP
retain the named `VALIDATION` error for invalid link requests.

## Shared upstream identifiers

This is the Desktop mapping for the summary, comparison and chartable-series
work in Govcraft/beankeeper issues [#116](https://github.com/Govcraft/beankeeper/issues/116),
[#117](https://github.com/Govcraft/beankeeper/issues/117) and
[#118](https://github.com/Govcraft/beankeeper/issues/118). Those outputs should
carry identifiers, not rendered labels. Ledger and schema changes stay upstream.

| Upstream fact or identifier | Desktop state | Meaning |
| --- | --- | --- |
| Company slug | `company` | Exact ledger company, never its display name |
| Expense/category account code | `account` | The account code, such as `5000`, not “Dining” |
| Entry tax-category tag | `category` | Exact tag such as `sched-c:24b`; register only |
| Transaction metadata `payee` | `payee` | Exact string, case and punctuation preserved; register only |
| Resolved current period | `from`, `to` | Inclusive calendar dates in `YYYY-MM-DD` |
| Resolved comparison period | `compare_from`, `compare_to` | Both inclusive calendar date bounds |
| Currency code | `currency` | Exact upstream currency; preserve it for both periods |
| Trend aggregation period | `period` | `month`, `quarter`, `year`; not a date range |
| Transaction / proposal identifier | `transaction_id` / `proposal_id` | Existing upstream identifier types, placed in the detail path |

Spending categories represented by expense accounts use `account`. Do not map
their display labels to `category`; that key specifically means an entry tax
tag. `search` remains a separate literal, case-insensitive substring match on
description/reference and does not substitute for an exact payee identifier.
Account, tax category, clearance and amount constraints match the same entry.
Missing or non-string `metadata.payee` cannot match a supplied payee. These
filters reuse upstream reads and never change classifications or ledger data.

Use the summary result's `desktop_view.link` where available. For older summary
results, use `view_id: "financial-summary"` and copy the company, currency and
four date bounds into `state`. Map resolved `cash_accounts`,
`receivable_accounts`, and `payable_accounts` arrays to `cash_account`,
`receivable_account`, and `payable_account`, respectively. The summary page
uses the same method and selected accounts as `financial_summary`.

Cash-flow chart targets can pass the full `cash_account` list to
`ui_get_link`. Summary and cash-flow links accept a single account code or a
list of exact codes. In URLs and the corresponding CLI flags, supply a
comma-separated list, for example `cash_account=1000%2C1100` or
`--cash-account 1000,1100`.

## Two-period detail

```json
{"name":"ui_get_link","arguments":{"view_id":"income-statement","state":{"company":"acme","currency":"USD","from":"2026-10-01","to":"2026-10-31","compare_from":"2026-09-01","compare_to":"2026-09-30"}}}
```

For period views, both comparison bounds are required, valid and chronological. The register and
period reports show a separately labeled comparison section using the same
company, currency and other supported filters. Register comparison starts from
its first page; its continuation links drill into that period. Budget comparison
must cover whole months in one year; its year is derived from the comparison
range. Balance sheets accept `compare_to` alone for a second as-of snapshot and
reject `compare_from`. The report catalog and reconciliation reject comparison
ranges because they do not represent two period reports.

Desktop executes the existing upstream report independently for each period;
it does not invent deltas, percentage changes or chart-series calculations.
Those belong to the upstream comparison/summary/chart work. A two-period JSON
download wraps the unchanged upstream payloads as `current` and `comparison`,
with `compare_from` and `compare_to`. A single-period download retains its
existing schema. Tax CSV exports remain single-period and explicitly reject
comparison bounds; use JSON for both periods.

## Opening a saved link

A paired browser opens views on that listener directly. Each listener hosts one
books file and has its own browser session. A different listener needs an initial
pairing. Visiting it leaves the original session available. Regenerate a saved
link if the associated listener address changes.

An unpaired browser sees
the pairing explanation. It remembers the local destination for five minutes
while the human opens the terminal's one-time pairing URL in that same browser
and confirms **Pair this browser**. Confirmation returns to that destination,
including its encoded filters. Opening multiple detail links before pairing
keeps the most recently visited destination. After expiry or restart, reopen the
original detail link before pairing. At the 64-entry limit, a new destination
evicts the oldest remembered entry and the pairing page still loads. The owner
whose entry was evicted can reopen the original link. The destination lookup
cookie grants no authority. See [pairing and recovery](browser-pairing.md#returning-to-an-optional-detail-link).

An existing paired browser arriving from chat may initially withhold its Strict
cookie. The same-origin **continue to your requested view** link restores that
session without changing cookie policy. No agent token, association invitation,
or terminal pairing secret belongs in a saved or shared detail URL.
