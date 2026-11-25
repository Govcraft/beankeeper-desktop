# Browser pairing and sessions

Ask your assistant to open Beankeeper Desktop or run `bk open`. The local
`service_open` tool and CLI request a fresh link from the running server and
open it in your default browser. The background service starts without a
pairing link in its logs. For foreground `bk serve`, the terminal's initial
link also works.

A link has a fresh 256-bit secret, expires after five minutes and can be
redeemed once. Selecting **Pair this browser** submits a same-origin POST.
A successful POST consumes the token, issues a cookie and opens the pending
destination or `/`. Opening or refreshing the link leaves it usable.
If the browser already sends a valid session cookie, the link opens the view
using that session. Health and static CSS remain public, and authenticated
ledger tools are available before any browser pairs.

The paired workspace provides **Pair another browser** and **Unpair this
browser** controls. Each new browser link is also single-use and expires after
five minutes. Run `bk open` again or ask your assistant to reopen Desktop when
a link expires. Keep pairing links private. The server keeps browser link
secrets and session cookies in memory. A link appears in the confirmation
page's address and hidden form field. The server disables
caching, restricts referrers to the same origin on the confirmation page so the
form can send its Origin, and disables referrers on errors and the redirect after
redemption. It installs no request URL logger. Do not put a proxy or
access logger in front of this local listener.

## Human session boundary

Cookies are host-only, `HttpOnly`, `SameSite=Strict`, `Path=/`, with a 12-hour
absolute lifetime. The cookie name includes the actual bound port; the server
also checks the session's original Host and port. Credentials are independent
256-bit secrets, separate from public browser routing IDs and CSRF tokens.
Cookies, links and associations are held in memory and invalidated by restart.
Each Desktop listener serves one books file. A separately hosted books file
needs an initial pairing with its listener. A paired browser can keep sessions
for multiple listeners at the same time and reuse each one until it expires.
Unpairing revokes the server-side session, even if a client keeps the cookie.
Expired entries are pruned on access. Browser links, agent association
invitations and active browser sessions each have a 64-entry limit per launch.

## Local owner authority

Each listener reachable through canonical loopback publishes a per-launch key
under the owner's private Desktop state directory. The CLI and local stdio
tool read it through a handle checked for ownership, permissions and symlink
safety. The issuing endpoint checks the socket peer is loopback and requires
the private key and the expected launch identity. Desktop rejects requests
with browser Origin or Cookie headers, ledger Authorization headers, or
duplicate owner credentials. The key is separate
from the one-time link, browser cookie and agent bearer credentials. It is
removed on normal shutdown and replaced on the next launch. The opener hands
the one-time link to the default browser. MCP returns only the launch outcome.

Desktop uses plain HTTP exclusively on its local UI origin. It omits the
`Secure` attribute for compatibility with browsers' HTTP loopback behavior,
including IP literals; this is the local HTTP equivalent allowed by D3. W3C
classifies loopback origins as [potentially trustworthy](https://www.w3.org/TR/secure-contexts/#is-origin-trustworthy).
This is not TLS and must not be used for a remote browser deployment. Explicit
`--allow-remote` remains an advanced development option with local Host/Origin
checks; it does not make cookies safe on a remote network.

Every cookie-authenticated write must pass the shared human-session middleware:

- A valid, unexpired cookie for the exact Host and port.
- Exactly one Origin, equal to `http://HOST:PORT`; missing Origin is rejected.
- The session's unpredictable synchronizer token, in `X-CSRF-Token` or a
  URL-encoded `csrf` form field. Forms without a header are limited to 8 KiB.
- No Authorization header. Bearer credentials cannot act as human credentials,
  including when a request also carries a valid cookie and CSRF token.

The initial `POST /pair` has no session yet. It requires the unpredictable
one-use token in a URL-encoded form, exactly one Origin matching the validated
Host and port, and no Authorization header. Missing, null, cross-origin or
duplicate Origins cannot redeem a link. Expired or consumed tokens return 410
without issuing a cookie, including when they expire after the page was opened.

The server uses an Origin check plus a synchronizer token as described by the
[OWASP CSRF guidance](https://cheatsheetseries.owasp.org/cheatsheets/Cross-Site_Request_Forgery_Prevention_Cheat_Sheet.html).
Browser forms work without JavaScript. CSP forbids scripts and framing and
restricts form submissions to the same origin. D7 approval handlers must use
this same middleware, and must never be added to the MCP router as human writes.

Pairing protects the browser HTTP boundary. It does not establish physical
human presence against software that can read the user's terminal, control the
browser, or steal its cookie. Agents should receive only their MCP credentials,
not terminal pairing links or browser credentials.

## Connect an agent to a paired browser

The upstream MCP router continues to verify bearer tokens, revocation and
capabilities on every request. Desktop neither copies those checks nor resolves
the ledger passphrase again. Cookie headers are removed before MCP dispatch.

After a successful authenticated MCP response, Desktop returns a
`Beankeeper-Pairing-Url` response header when that credential has no associated
browser. This is an agent association invite, not a browser login link. It
expires in five minutes and is single-use. A human opens it in an already paired
browser and selects **Connect agent to this browser**. Opening the invite in an
unpaired browser instructs the human to ask their assistant to open Desktop or
run `bk open`, then reopen the invite. If Strict cookies were withheld when following a link from an
external site, **Continue in this browser** starts a same-origin navigation and
shows the confirmation form. An agent cannot exchange its own invite for a
human session. A paired browser with an open event stream still needs this
confirmation before agent navigation is available. Its live notice asks for the
agent connection link until association, and updates in already-open tabs when
the connection is confirmed. Navigation and highlight calls without a matching
live associated tab return an explicit `NO_CONNECTED_TAB` tool error with a
message and association link; `ui_status` remains available for checking readiness.

Desktop keys associations by SHA-256 of the upstream-verified identity URI and
token ID, never by a client-supplied MCP session ID or agent name. This lets the
D4 host callback use its request-scoped `VerifiedAgent` without receiving a raw
credential. The response-header helper reuses upstream's public verifier after
successful MCP responses to obtain the same routing key; it adds no authentication
or grant decisions. Separate transport sessions using the same credential share
browser targets. Changing bearer credentials never inherits another agent's
pairing. Fallback credentials follow the same rules. Failed authentication and
revoked credentials never receive an invite.

`PairingState::paired_sessions` and `agent_pairing_url` remain the association
APIs. Derive an `AgentKey` with `from_verified_identity`. D4 registers individual
public tab TypeIDs within each `BrowserSessionId`; these confer no cookie
authority. Multiple independently paired browsers can be associated using
separate invites. Each associated browser has one current agent identity; a
human confirmation can replace it. Pending events from the previous identity
are discarded. See [navigation](navigation.md) for tool arguments, SSE delivery,
tab selection and acknowledgement behavior.

## Returning to an optional detail link

An anonymous GET/HEAD to a valid view shows the pairing explanation and saves
its canonical local destination in a separate bounded registry. A short-lived,
HttpOnly, SameSite=Strict `bk_desktop_PORT_return` cookie holds only a random
lookup value, never the URL, bearer token, pairing secret or session credential.
It does not authorize any browser read or write. The registry has at most 64
entries and expires each entry five minutes after its first creation. Repeated
visits in the same browser replace that browser's pending destination; the most
recent view wins. A server restart clears the registry.

The owner opens a fresh pairing link in the same browser and confirms its
form. Only a valid, exact-Origin pairing POST creates the session,
consumes the pending destination and redirects there with HTTP 303. The response
clears the return cookie. Failed pairing attempts leave the destination intact.
Expired or forged lookup values fall back to `/`; reopening the original detail
URL saves a fresh destination. No anonymous request can mint a pairing link.

Destinations use the shared view selector: only known local HTML views with
valid, supported filters are accepted. Absolute URLs, protocol-relative paths,
backslashes, fragments, controls, route traversal, unknown fields and duplicate
query keys cannot become redirect destinations. Browser writes and requests
carrying bearer credentials never set a return destination.

When an external navigation withholds an existing Strict session cookie, the
pairing explanation offers an ordinary same-origin continuation link. That
follow-up can send the existing cookie and open the requested view without
pairing again. This flow needs no JavaScript and grants no navigation or approval
rights to the agent which generated the detail link.
