-- Signups from /updates. One row per email; a repeat signup updates the answers (first-touch metadata is kept).
-- No raw IP is stored.
CREATE TABLE signups (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  email TEXT NOT NULL UNIQUE,          -- trimmed, lowercased
  company TEXT,                        -- optional
  why TEXT,                            -- "Why do you want to use blaze?", optional
  created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
  updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
  -- request.cf
  country TEXT,
  region TEXT,
  city TEXT,
  asn INTEGER,
  as_organization TEXT,
  timezone TEXT,
  -- headers
  user_agent TEXT,
  referer TEXT,
  accept_language TEXT,
  -- query params of the /updates page
  src TEXT,
  utm_source TEXT,
  utm_medium TEXT,
  utm_campaign TEXT,
  utm_term TEXT,
  utm_content TEXT
);

-- Rate limit: every POST that passes validation and Turnstile is logged here (the insert is skipped, and the post
-- refused, when the email or everyone is over the per-minute cap). Posts that fail Turnstile are not logged, so they
-- cannot use up the global cap. Rows older than a day are deleted on each signup.
CREATE TABLE signup_attempts (
  email TEXT NOT NULL,
  at INTEGER NOT NULL                  -- unix seconds
);
CREATE INDEX signup_attempts_email_at ON signup_attempts (email, at);
CREATE INDEX signup_attempts_at ON signup_attempts (at);
