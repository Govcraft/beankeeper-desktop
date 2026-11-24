# Changes since you last looked

Ask your agent what it changed. It can tell you who made each change, when it
happened, what changed, and which source it came from. A grouped receipt keeps
an audited reversal or proposal approval together with the posting it produced.
Receipts include before and after details where the ledger records them.

## Read and acknowledge changes

| Purpose | CLI | MCP |
| --- | --- | --- |
| Read the next page | `bk --db BOOKS --company COMPANY --json audit since` | `changes_since` with `company` |
| Acknowledge the delivered page | `bk --db BOOKS --company COMPANY --json audit mark-seen --through CURSOR` | `changes_mark_seen` with the returned `company` and `through` |

Use the usual ledger read credential. On a ledger with one book, the query
can infer the book. On a ledger with several books, select the one to report.
Use the receipt data for the summary and any chart. After informing the owner,
acknowledge the page with its exact `through` value. If `has_more` is true,
repeat for the next page. The CLI puts the same result inside its `data` envelope.

The first read starts at the beginning of the book's history. The cursor is
local to the Desktop user and book at that ledger path. It persists across
restarts. Reading a report or opening the browser never changes the cursor.
Acknowledging it changes only Desktop state. Agent writes and audited undo work
independently of the cursor. A read-only MCP listener offers `changes_since`
and hides `changes_mark_seen`.

## Undo a receipt

Ask your bookkeeper agent to undo its work in chat. With an eligible receipt,
the agent calls `audit_undo` or `bk audit undo RECEIPT` to undo the whole
operation. With a transaction ID, it uses `txn_reverse` or `bk txn reverse ID`.
The ledger retains the original history. Undoing an import keeps the postings
it matched that were already in your books. Repeating a completed receipt
undo returns its earlier result.

In propose-only mode, the agent reports the correction as pending sign-off.
If a rule blocks the correction, the agent explains why and follows the next
authorized step named by the error. If the agent suggests undoing an earlier
hand-made offset and the original entry, it waits for you to ask it to undo
both in chat.

Open the optional `desktop_url` to see the full page or the returned `undo_url`
to view the individual receipt.

The CLI includes links when it finds a running Desktop instance for the
selected books. The receipts and summary are also available while Desktop is
stopped.
