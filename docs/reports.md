# Reports and reconciliation

Open `/reports` after pairing your browser to choose a report. All report views
and downloads require D3's human cookie, with the same Host/Origin boundary as
the register. Agent bearer tokens cannot read these URLs. Every view is read-only,
including reconciliation; there are no clearance, reconciliation or approval
writes. Ledger reads use upstream's existing CLI report implementation, without
new accounting logic or schema migrations.

## Stable view IDs and URLs

These IDs are also available in Rust as `beankeeper_desktop::reports::VIEW_IDS`.
D4 can target these URLs when its navigation tools are present. This change does
not add navigation tools or browser events itself.

| View ID | URL | Additional filters |
| --- | --- | --- |
| `income-statement` | `/reports/income-statement` | `from`, `to` |
| `balance-sheet` | `/reports/balance-sheet` | `to` (as of) |
| `cash-flow` | `/reports/cash-flow` | `from`, `to`, `cash_account` |
| `trend` | `/reports/trend` | `from`, `to`, `metric`, `period` |
| `budget-variance` | `/reports/budget-variance` | `year`, or `from` and `to` |
| `schedule-c` | `/reports/schedule-c` | `from`, `to` |
| `1099` | `/reports/1099` | `from`, `to`, `threshold` |
| `reconciliation` | `/reconciliation` | `account`, `to`, `mode`, `statement`, `statement_balance`, `link_key` |

`/reports/reconciliation` is an alias for `/reconciliation`. An unknown report ID
returns 404. Every URL accepts `company` and `currency`. An omitted company uses
`bk --company` / `BEANKEEPER_COMPANY`, then the first listed company, visibly
selected in the form. Companies are never silently combined. Currency is inferred
by upstream when a company's books use one currency (USD for empty books).
Multi-currency books require an explicit code. Integer amount fields use minor units, including budget and tax totals. The UI
labels upstream formatted decimal fields as major units; JSON retains their
original `_display` field names.

## Date and report filters

Dates use inclusive `YYYY-MM-DD` bounds. As in the CLI, an omitted `to` means
today in the server's local timezone. Period reports default `from` to January 1
of the effective end year. Balance sheet and reconciliation include all history
through `to` and reject a start date. Date defaults are recalculated on reload;
use explicit bounds for a fixed reporting window.

Budget variance follows the CLI's whole-month model. Enter `year` for the full
year, or provide both date bounds covering whole calendar months within one year.
For example, `from=2026-02-01&to=2026-03-31` maps to CLI `--year 2026 --from 2 --to
3`. Partial months and cross-year ranges return 400 rather than silently changing
the period. If supplied alongside dates, `year` must match their year. Accounts
without a budget are included, matching CLI `--include-unbudgeted`. The initial
page asks for a year or range before computing a report.

Trends support `metric=spending` (default) or `metric=net-worth`, and
`period=month` (default), `quarter` or `year`. Cash flow uses upstream's cash-name
inference, or a single explicit `cash_account` code. The 1099 minimum `threshold`
is in major units, defaults to `600`, and accepts `0` to include every payee.

Reconciliation requires an `account` code, selected from the company's accounts.
`mode=bank` is the default; `mode=settlement` requires a nonempty `statement` and
an asset or liability account. `statement_balance` is a signed amount in major
units, and `link_key` defaults to `applies_to` for settlement matching. Upstream
controls partial clearance, statement attribution, historical clearing dates,
settlement matching and difference calculations. The view includes book,
cleared, reconciled and outstanding balances and the associated item details.
The initial page asks for an account before computing a report.

## Downloads

Add `format=json` to any report URL to download the CLI report envelope's `data`
payload. Tax reports also accept `format=csv`, with the same columns, integer
amounts and row order as CLI CSV. Download links preserve the company, currency
and report-specific filters. Formats other than HTML (default), JSON and tax CSV
return 400. Downloads have attachment filenames and `Cache-Control: no-store`.
They contain private ledger data and must be handled accordingly.

Schedule C and 1099 screens explicitly state: **Bookkeeping summary only. Not
tax advice; not a filed form.** They do not generate or file IRS forms. Schedule C
includes mapped lines, unmapped tags and untagged revenue/expenses. Vendor output
retains upstream's payee metadata and threshold rules, including errors for
conflicting metadata.

Examples after pairing:

```text
/reports/income-statement?company=acme&currency=USD&from=2026-01-01&to=2026-03-31
/reports/schedule-c?company=acme&currency=USD&from=2026-01-01&to=2026-12-31&format=csv
/reports/1099?company=acme&currency=USD&to=2026-12-31&threshold=0&format=json
/reconciliation?company=acme&account=1000&currency=USD&to=2026-03-31&statement=S1&statement_balance=1234.56
/reconciliation?company=acme&account=1210&mode=settlement&statement=S1&currency=USD&to=2026-03-31
```
