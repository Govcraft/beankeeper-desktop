# Optional Desktop detail links

The agent answers in plain words and can draw its own chart from ledger data.
A Desktop link is an optional way to inspect the matching detail. Generating a
link never opens a tab, changes a view, highlights a row, or posts to the ledger.

`ui_get_link` requires `ledger/read`, works under `--read-only`, and accepts
`view_id` plus an optional `state` object. It needs no browser pairing or UI
credential. It returns readable text and this structured shape:

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

Other optional state fields are returned as `null`. No credential is included.
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
bk ui link --view-id settings --bind '[::1]:8751'
```

CLI flag names use hyphens, MCP state keys use underscores. Link generation
needs no ledger, passphrase, license activation, network request or running
Desktop process. The caller supplies the listener with `--bind` (default
`127.0.0.1:8750`). Only canonical IPv4/IPv6 loopback addresses with a nonzero
port are accepted. If state omits a company, the CLI uses upstream's root
`--company`/`BEANKEEPER_COMPANY` default for ledger views. JSON validation errors
contain `code` and `message`, exit nonzero, and also print a diagnostic to stderr.

## Stable views

| ID | URL path | Accepted filters |
| --- | --- | --- |
| `dashboard` | `/` | `company`, `currency` |
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

A paired browser opens the requested view directly. An unpaired browser sees
the pairing explanation. It remembers the local destination for five minutes
while the human opens the terminal's one-time pairing URL in that same browser
and confirms **Pair this browser**. Confirmation returns to that destination,
including its encoded filters. Opening multiple detail links before pairing
keeps the most recently visited destination. After expiry or restart, reopen the
original detail link before pairing. The destination lookup cookie grants no
authority. See [pairing and recovery](browser-pairing.md#returning-to-an-optional-detail-link).

An existing paired browser arriving from chat may initially withhold its Strict
cookie. The same-origin **continue to your requested view** link restores that
session without changing cookie policy. No agent token, association invitation,
or terminal pairing secret belongs in a saved or shared detail URL.
