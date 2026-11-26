# Beankeeper Desktop License Agreement and Source-Available Terms

**Licensor:** DIAM General Services LLC, doing business as Govcraft ("Govcraft," "we," "us"), a Texas limited liability company
**Address:** 615 N Upper Broadway St, Ste 639, Corpus Christi, TX 78401, United States
**Support:** support@beankeeper.app

---

## 1. Plain-language summary

This agreement covers **Beankeeper Desktop**—the background service, local browser views, AI extension packaging, and signed updates made by Govcraft.

- **Buy once, keep it.** A paid license is a one-time purchase of $79, not a subscription. One purchase covers every 1.x release of Beankeeper Desktop, forever. A 2.0 release would be a new purchase.
- **One computer, one set of books.** Each license covers one set of books and may be used on one computer at a time. If you keep books for more than one client, each client's books need their own license, even when they all live on the same computer. You may move a license to a replacement computer by deactivating it on the old computer and activating it on the new one.
- **Your books stay yours.** Your books live on your computer, encrypted with a recovery kit you control. Govcraft never holds your books and cannot lock you out of your own data.
- **Open-source core stays free.** The Beankeeper ledger engine (`Govcraft/beankeeper`, licensed under MIT and Apache-2.0) is separate open-source software. This agreement never limits or revokes your rights to the open-source ledger.
- **No phone-home.** Beankeeper Desktop checks your license on your own computer. It does not call home to keep working.
- **60-day automatic refund.** If you want a refund within 60 days of buying, ask your AI to return your license (or run `bk license return`). Your license is deactivated and revoked, and your refund is issued automatically.
- **Read-only mode without an active license.** On official non-rc 1.0.0 and later releases, after these terms are final (`#96`), a set of books with no active license—or whose license has been returned or revoked—switches to read-only mode. Release candidates are never enforced. You can always open those books, ask your AI questions about them, export everything (CSV or JSON), back up, restore, and use your recovery kit. Writing new entries in those books, and installing new official updates you are not licensed for, stops until you activate a license.
- **Source-available, permanently.** You may read and inspect the Beankeeper Desktop source code, and you may compile and modify it for personal, non-commercial use without a paid license. Commercial use of a self-built copy requires a paid license. You may not redistribute it, resell it, or offer it as a hosted service. Beankeeper Desktop stays source-available under these terms; it does not later convert to an open-source license.

---

## 2. Scope and relationship to other terms

1. **What this agreement covers.** This agreement governs your use of the **Beankeeper Desktop** software, installers, extension bundles, and source code (published at `https://github.com/Govcraft/beankeeper-desktop`).
2. **Open-source core (`Govcraft/beankeeper`).** The Beankeeper ledger library, `bk` ledger commands, double-entry engine, and core MCP tools in `https://github.com/Govcraft/beankeeper` are licensed under the **MIT License** and the **Apache License 2.0**. Those open-source licenses govern that code, not this agreement. Nothing here takes away your rights under MIT or Apache-2.0.
3. **Website terms and privacy policy.** Your use of `beankeeper.app` and our support channels is also governed by our Website Terms of Service (`https://beankeeper.app/terms/`) and Privacy Policy (`https://beankeeper.app/privacy/`). If this agreement and the Website Terms of Service conflict on a license, update, or refund question, this agreement controls, except that nothing in this agreement takes away the 60-day automatic refund right.
4. **When Read-Only Mode is enforced.** Release candidates (including `0.1.0-rc.*` and any later `-rc.*` builds) are free to install and use without a license; Read-Only Mode is **never** enforced on a release candidate. Read-Only Mode applies only to official, non-rc **1.0.0 and later** releases, and only after these terms are final (`#96`). Until then, official builds do not lock writes for lack of a license.

---

## 3. Purchase, perpetual license grant, and updates

1. **One-time purchase.** Paid licenses are sold for a one-time fee of $79 (plus any applicable taxes). **Govcraft (DIAM General Services LLC) is the licensor.** You may buy a license on beankeeper.app or from an authorized reseller or merchant of record (for example Paddle, Polar, Etsy, Lemon Squeezy, or an app store). Where a merchant of record or store handles the payment, its checkout terms also apply to payment and tax. The license itself, the 60-day automatic refund on in-app return, and revocation are Govcraft's and apply no matter where you bought the license. A refund is issued through the original seller. Govcraft does not store your payment card number.
2. **Perpetual grant for eligible releases.** Once you activate a valid, signed license file issued for your purchase, Govcraft grants you a non-exclusive, worldwide, perpetual license (unless returned or terminated under Section 8) to install and run eligible releases of Beankeeper Desktop for your own personal, household, or internal business bookkeeping, or for a client's books you are engaged to keep, on the computer and ledger terms in Section 3.4.
3. **Eligible releases (major version).**
   - A license purchased for Beankeeper Desktop 1.x covers **every 1.x release, forever**. You may install and keep using any 1.x version, including versions released after you bought the license.
   - A 2.0 release (or any later major version) is a new product for licensing purposes and requires a new purchase at the price offered at that time. A 1.x license does not cover 2.0 or later major versions.
   - You may keep running any eligible release you have already installed, even if you never buy a later major version.
4. **One computer and one set of books per license.**
   - Each license covers **one (1) set of books** (one ledger) and may be used on **one (1) computer** you own or control at a time.
   - These limits work together, not as a single license per computer: if you keep several clients' books on the same computer, you hold several licenses, one for each client's ledger. One license does not cover every set of books on that computer.
   - You may move a license, with the books it covers, to a replacement computer you own or control by deactivating it on the computer you are leaving and activating it on the replacement. You may keep private backup copies of the installer and license file for disaster recovery.
   - You may not use the same license on two computers at once, apply one license to more than one set of books, or share a license with another person or a separate business.
   - *Technical note:* Issued license files record the computer limit as a signed `seats` value of `1`. Beankeeper Desktop does not lock licenses to hardware IDs or maintain an online activation counter. A purchaser may hold more than one license at the same time.

---

## 4. Source-available terms (`beankeeper-desktop`)

Beankeeper Desktop is **source-available** software and remains so under these terms permanently. It does not convert to an open-source license after a change date. Subject to your compliance with these terms:

1. **Inspection and auditing.** Anyone may view, read, and audit the Beankeeper Desktop source code in `Govcraft/beankeeper-desktop` to verify how it handles your books, encryption, updates, and offline licensing.
2. **Building and modifying from source.**
   - You may compile and modify the Beankeeper Desktop source code for your own **personal, non-commercial** use without a paid license.
   - **Commercial use** of a self-built copy — including use in a business, or to keep books for a business you operate — requires a paid license under Section 3.
   - These source-build permissions do not allow redistribution, resale, or hosting, which remain prohibited under Section 5.
3. **No outside contributions.** Beankeeper Desktop does not accept outside code contributions at this time. Please submit bug reports and feature requests through GitHub issues or `support@beankeeper.app`. Contributions to the open-source ledger belong in `Govcraft/beankeeper`.

---

## 5. Restrictions

Except as expressly allowed in Section 4 or by non-waivable law, you may **not**:

1. **Redistribute or resell:** distribute, publish, sell, resell, sublicense, rent, lease, lend, gift, or otherwise transfer Beankeeper Desktop binaries, installers, extension bundles, source code, or license files to third parties, except for the limited transfer rights in Section 8.
2. **Operate a hosted or competing service:** use Beankeeper Desktop to provide a hosted, cloud, multi-tenant, SaaS, or managed bookkeeping platform, or a competing product, for third parties. An independent bookkeeper may run Beankeeper Desktop on their own computer to keep books for clients, but each client's books must be covered by their own license. A bookkeeper with several clients on one computer holds several licenses, one per ledger. That permission does not allow offering Beankeeper as a hosted or online service.
3. **Bypass licensing:** circumvent, tamper with, forge, strip, or disable offline Ed25519 signature verification, signed revocation lists, or major-version eligibility checks.
4. **Unlawful use:** use Beankeeper Desktop for fraud or any unlawful purpose, or on books you are not authorized to access.

---

## 6. Offline verification and revocation (no phone-home)

1. **Offline verification.** Your license file is cryptographically signed (Ed25519) and verified locally on your computer. Beankeeper Desktop never phones home to verify your license or keep working.
2. **Update checks.** If you check for or download updates (and do not opt out with `BEANKEEPER_NO_UPDATE_CHECK=1`), Desktop fetches signed release metadata and only sends the Desktop version and operating system—never your books or personal financial data.
3. **How revocations take effect without phone-home.** When a license is returned, refunded, or revoked (for example, on a lost payment dispute or breach), the license server marks it revoked so it cannot be re-downloaded or used for future updates. Releases of Beankeeper Desktop also carry a signed list of revoked license IDs, so updated copies reject revoked licenses locally without contacting any server.

---

## 7. Refunds, license returns, and Read-Only Mode

1. **60-day automatic refund via license return.**
   - Within **60 days** of your purchase date, you may return your license through the app by asking your AI to return your license (or running `bk license return`).
   - Desktop explains what will happen before you confirm. Once confirmed, Desktop removes that license from this computer, notifies the license server to revoke it, and triggers a **full refund automatically** through the original seller (the site, reseller, or merchant of record where you bought)—no manual approval from Govcraft is required.
   - If an automatic refund fails for a technical reason, contact `support@beankeeper.app` and we will process it promptly.
2. **Returns after 60 days.** You may still return a license through the app more than 60 days after purchase. Desktop will explain plainly before you confirm that returning after 60 days revokes and deactivates the license with **no refund**.
3. **Read-Only Mode.** On official non-rc 1.0.0 and later releases, after these terms are final (`#96`), a set of books with no active license—or whose license has been returned, refunded, deactivated, or revoked—is in **Read-Only Mode**. Release candidates are never placed in Read-Only Mode for lack of a license. Read-Only Mode is per ledger: other books on the same computer that have their own active license stay writable.
   - **What continues to work:** You can open, read, and ask your AI questions about those books; export all of your data (CSV and JSON); create and restore local backups; and use your recovery kit. You can also continue using the free, open-source `bk` core (`Govcraft/beankeeper`, MIT/Apache-2.0), which can never be revoked.
   - **What stops:** Writing to those books stops—including recording or modifying entries through your AI, statement imports, background service writes, and Claude/AI extension writes. Installing official updates you are not licensed for also stops. Desktop will tell you plainly that those books are in read-only mode, why, and how to buy or reactivate a license.
   - Personal, non-commercial use of a self-built copy, as allowed in Section 4, does not require a paid license and is not placed in Read-Only Mode for lack of one. Commercial use of a self-built copy requires a paid license for each set of books, and without one the same Read-Only Mode rules apply.

---

## 8. Transfer and termination

1. **Transfer.** You may move your license between computers you own or control, as described in Section 3.4 (one computer at a time). You may also transfer this license in connection with a sale or merger of the business whose books the license covers (business succession). You may not resell, gift, or otherwise assign this license to another person, or to a different business, except for that business succession. Govcraft may assign this agreement in connection with a reorganization or sale of its business.
2. **Termination.**
   - You may terminate your license at any time by returning it through the app or deleting all copies of your license file and Desktop binaries.
   - Your license terminates automatically if your purchase is refunded, reversed by chargeback or payment dispute, or if you materially breach Section 3.4 or Section 5 and fail to cure within 14 days of notice (or immediately for uncurable circumvention or redistribution).
3. **Effect of termination.** Upon termination or revocation, your rights to receive updates under that license and to write to the books it covered end immediately, and on an official non-rc 1.0.0 or later release those books enter Read-Only Mode. Termination never affects your ownership of your books, your ability to read/export/back up/restore them, or your rights under the MIT/Apache-2.0 licenses for `Govcraft/beankeeper`. Sections 2.2, 4, 5, 7.3, 9, 10, 11, 12, and 13 survive termination.

---

## 9. Not tax, accounting, or legal advice; your AI and third parties

1. **Not professional advice.** Beankeeper helps you keep records and ask questions about them. It is not a bookkeeper, CPA, accountant, tax preparer, or lawyer. Nothing in the software, on `beankeeper.app`, or from Govcraft is tax, accounting, or legal advice. You are solely responsible for your books, tax filings, and financial decisions.
2. **Your AI and banks.** You connect Beankeeper to an AI you choose (such as the Claude app or another compatible AI), under that provider's own terms and privacy policy. Govcraft is not responsible for what third-party AI apps or bank/statement connectors do, say, or store. Beankeeper itself does not connect directly to your bank.

---

## 10. No warranty

TO THE FULLEST EXTENT ALLOWED BY APPLICABLE LAW, BEANKEEPER DESKTOP, ITS UPDATES, AND ITS SOURCE CODE ARE PROVIDED **"AS IS" AND "AS AVAILABLE,"** WITHOUT WARRANTIES OF ANY KIND, EXPRESS, IMPLIED, OR STATUTORY, INCLUDING IMPLIED WARRANTIES OF MERCHANTABILITY, FITNESS FOR A PARTICULAR PURPOSE, TITLE, AND NON-INFRINGEMENT, OR ANY WARRANTY THAT THE SOFTWARE WILL BE ERROR-FREE, UNINTERRUPTED, OR SUITED TO ANY TAX FILING OR AUDIT.

YOU ARE SOLELY RESPONSIBLE FOR KEEPING BACKUPS OF YOUR BOOKS AND YOUR RECOVERY KIT. GOVCRAFT DOES NOT HOLD YOUR BOOKS AND CANNOT RECOVER THEM IF YOU LOSE YOUR FILES OR RECOVERY KIT.

Some jurisdictions do not allow the exclusion of certain warranties; where required by law, those exclusions do not apply to you, and you may have additional statutory rights.

---

## 11. Limitation of liability

TO THE FULLEST EXTENT ALLOWED BY APPLICABLE LAW:

1. GOVCRAFT AND ITS OWNERS, MEMBERS, EMPLOYEES, AND AGENTS WILL NOT BE LIABLE FOR ANY INDIRECT, INCIDENTAL, SPECIAL, CONSEQUENTIAL, EXEMPLARY, OR PUNITIVE DAMAGES, OR FOR ANY LOSS OF PROFITS, REVENUE, DATA, OR BOOKS, ARISING OUT OF OR RELATED TO BEANKEEPER DESKTOP OR THIS AGREEMENT, EVEN IF ADVISED OF THE POSSIBILITY OF SUCH DAMAGES.
2. GOVCRAFT'S TOTAL AGGREGATE LIABILITY FOR ALL CLAIMS ARISING OUT OF OR RELATED TO BEANKEEPER DESKTOP OR THIS AGREEMENT WILL NOT EXCEED THE AMOUNT YOU ACTUALLY PAID TO GOVCRAFT FOR THE LICENSE GIVING RISE TO THE CLAIM (OR TEN U.S. DOLLARS ($10.00) IF YOU HAVE NOT PAID FOR A LICENSE).

Nothing in this section limits liability that cannot be limited under applicable law (such as fraud or, where forbidden, death or personal injury caused by negligence), or limits your 60-day automatic refund right under Section 7.

---

## 12. Export and trade compliance

Beankeeper Desktop uses standard encryption and is subject to United States export control and economic sanctions laws. You represent and warrant that you are not located in, organized under the laws of, or ordinarily resident in any country or territory subject to comprehensive U.S. trade sanctions, and that you are not on any U.S. government restricted-party list (including the OFAC Specially Designated Nationals list or BIS Denied Persons/Entity lists). You agree not to export, re-export, or transfer Beankeeper Desktop in violation of applicable U.S. export or sanctions laws.

---

## 13. Changes, governing law, and general terms

1. **Changes to these terms.** Govcraft may update these terms for future versions and future purchases. A change will never retroactively reduce the perpetual use rights, major-version update coverage, or 60-day automatic refund right of a license you already bought, except as required by law.
2. **Governing law and venue.** This agreement is governed by the laws of the **State of Texas**, without regard to conflict-of-law rules. Any dispute arising out of or relating to this agreement or Beankeeper Desktop that goes to court must be brought in the **state or federal courts located in Texas**, and you and Govcraft consent to the personal jurisdiction and exclusive venue of those courts.
3. **General.** If any provision of this agreement is held unenforceable, the remaining provisions remain in effect. Failure to enforce a provision is not a waiver. Together with your checkout receipt, the Website Terms of Service (`https://beankeeper.app/terms/`), and the Privacy Policy (`https://beankeeper.app/privacy/`), this agreement is the entire agreement between you and Govcraft regarding Beankeeper Desktop.
