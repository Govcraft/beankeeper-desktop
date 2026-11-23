-- Beankeeper's initial schema (beankeeper-cli/src/db/schema.rs, apply_v1).
-- Keep this historical fixture fixed when upstream adds migrations.
CREATE TABLE schema_version (
    version INTEGER NOT NULL
);

CREATE TABLE companies (
    slug TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    description TEXT,
    created_at TEXT NOT NULL DEFAULT (datetime('now'))
);

CREATE TABLE accounts (
    company_slug TEXT NOT NULL REFERENCES companies(slug),
    code TEXT NOT NULL,
    name TEXT NOT NULL,
    type TEXT NOT NULL CHECK(type IN ('asset','liability','equity','revenue','expense')),
    created_at TEXT NOT NULL DEFAULT (datetime('now')),
    PRIMARY KEY (company_slug, code)
);

CREATE TABLE transactions (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    company_slug TEXT NOT NULL REFERENCES companies(slug),
    description TEXT NOT NULL,
    metadata TEXT,
    currency TEXT NOT NULL DEFAULT 'USD',
    date TEXT NOT NULL,
    posted_at TEXT NOT NULL DEFAULT (datetime('now'))
);

CREATE TABLE entries (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    transaction_id INTEGER NOT NULL REFERENCES transactions(id),
    account_code TEXT NOT NULL,
    company_slug TEXT NOT NULL,
    direction TEXT NOT NULL CHECK(direction IN ('debit','credit')),
    amount INTEGER NOT NULL CHECK(amount > 0),
    memo TEXT,
    FOREIGN KEY (company_slug, account_code) REFERENCES accounts(company_slug, code)
);

CREATE INDEX idx_entries_transaction_id ON entries(transaction_id);
CREATE INDEX idx_entries_company_account ON entries(company_slug, account_code);
CREATE INDEX idx_transactions_company ON transactions(company_slug);
CREATE INDEX idx_transactions_company_date ON transactions(company_slug, date);

INSERT INTO schema_version (version) VALUES (1);
INSERT INTO companies (slug, name) VALUES ('acme', 'Acme');
INSERT INTO accounts (company_slug, code, name, type)
    VALUES ('acme', '1000', 'Legacy Checking', 'asset');
