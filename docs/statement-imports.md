# Import statements and historical journals

Share a CSV, OFX or QFX statement with your agent and identify the book and bank
account. Beankeeper applies matching categorization rules and uses the selected
suspense account for entries that need a category. The import reports what it
recorded, which rows were duplicates, and how many new or pending rows use
suspense. A direct import returns an audit receipt and a Desktop link for
undoing the newly recorded entries as one operation.

## Attach a file in a compatible client

The local Desktop `/mcp` endpoint accepts attachments for the books it started with through the draft
[MCP File Uploads extension](https://github.com/modelcontextprotocol/experimental-ext-file-uploads/blob/main/specification/draft/file-uploads.md)
over Streamable HTTP. The compatible client fills the tool's `file` argument and
sends the bytes directly to Beankeeper in a separate authenticated request.
The server temporarily holds the file in memory. A valid attachment can be up
to 1 MiB and 5,000 rows, the same limits as the text import.

The tool is `ledger_setup_statement_import`. Send the same company, accounts,
currency, format and optional CSV mapping as a text import. Reuse the same
stable import reference and identical source when retrying. With
[MCP Tasks](https://modelcontextprotocol.io/extensions/tasks/overview),
imports of 64 KiB or more return a task that the client can poll using
`tasks/get`. The task reports progress and returns the usual tool result
with `receipt_id`, `desktop_path`, and `counts`. Task results and upload
targets are retained for 15 minutes and expire when the process exits. If a
connection drops after submission, check the receipt or retry with the same
reference and statement. The ledger can return the original receipt.

The local stdio bridge supports Tasks for text imports. Use a local file via
`bk import` or the existing tool's `content` field with a client that does not
offer file transfer. Anthropic currently documents file transfer for directory
connectors with approved public hosts. Its hosted connector rollout does not
enable attachments on a local Desktop connection. [Claude file transfer requirements](https://claude.com/docs/connectors/building/files)

## Import a file from the CLI

Run the same audited import against the running Desktop service.

```sh
bk --json import ~/Downloads/statement.csv --book household \
  --bank 1010 --suspense 3150 --currency USD --statement-format csv \
  --reference checking-2026-05
```

Use `-` for standard input or `--statement-format ofx` for OFX and QFX. For
CSV, pass `--csv '{"date_col":"Posted"}'` to override the default date column. The
command uses the managed agent credential by default. For separately served
books, select the books with `--db` and supply the corresponding agent
credential through `BK_AGENT_TOKEN`. Use `--bind` if more than one listener
serves the selected books. `bk import` waits for the task result and reports
phase changes on stderr. `--json` produces one result on stdout.

The existing `bk setup statement-import` command can still read statement text
from stdin directly against the ledger. It also accepts custom CSV mappings.
The receipt and transaction history remain available after Desktop restarts.

## Import historical journals

When migrating a general ledger, map its source accounts and group the exported
lines into balanced journals. Agree on the migration date and avoid overlapping
opening balances or repeating journals created by separately issued invoices and
bills. Use a stable source reference for each journal, such as
`old-ledger:journal-42`, even when splitting the export across batches.

Send the mapped journals to `txn_import` using a Direct bookkeeper connection:

```json
{
  "company": "acme",
  "dry_run": true,
  "transactions": [
    {
      "reference": "old-ledger:journal-42",
      "date": "2026-03-31",
      "description": "Consulting sale",
      "currency": "USD",
      "entries": [
        {"account_code": "1000", "direction": "debit", "amount": "120.00"},
        {"account_code": "4000", "direction": "credit", "amount": "120.00"}
      ]
    }
  ]
}
```

Review the preview, then submit the same journals with `dry_run: false`. A
failed batch leaves the ledger unchanged. A replay skips duplicate references by
default. Send `on_conflict: "error"` to reject the entire batch on a duplicate.
MCP accepts up to 500 journals, 5,000 entries and 1 MiB of encoded journal JSON.
Use the same transaction array with the CLI's dry run:

```sh
bk --company acme --json txn import --file journals.json --dry-run
```

Journal imports carry accounting entries and their metadata. To migrate source
contacts, open invoices or bills and payments, use the corresponding
counterparty and document tools. Desktop's file attachment and task workflow
above handles bank and card statements.
