# Browser pairing and sessions

Start `bk serve` and open the one-time URL it prints to the terminal. The URL
contains a fresh 256-bit secret from the operating system. It expires after five
minutes and can be redeemed only once. Opening or refreshing the URL displays
a confirmation page without consuming the secret or issuing a cookie. Select
**Pair this browser** before the link expires to submit its token by POST.
Only a successful POST consumes the token, issues a cookie and redirects to `/`.
Duplicate or speculative GET/HEAD requests leave the link usable; confirmation
requires an explicit click and is never submitted automatically.
Opening `/` directly, including through `bk open`, displays a pairing screen
until the browser has paired. Health and static CSS remain public; ledger tools
remain available through authenticated MCP even before any browser pairs.

The paired workspace provides **Pair another browser** and **Unpair this
browser** controls. Each new browser link is also single-use and expires after
five minutes. If all links expire and no browser is paired, restart `bk serve`.
Pairing links are secrets: keep terminal output private. No pairing secret,
cookie credential or agent bearer token is stored on disk or placed in the
public shell. Secrets are redacted from Debug output. A link necessarily appears
in the confirmation page's address and hidden form field. The server disables
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
Unpairing revokes the server-side session, even if a client keeps the cookie.
Expired entries are pruned on access. There are at most 64 pending links and
64 active browser sessions per launch.

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
unpaired browser instructs the human to pair from the terminal first, then reopen
the invite. If Strict cookies were withheld when following a link from an
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
