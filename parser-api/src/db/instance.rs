//! Instance ownership — who runs this instance.
//!
//! The claim is its own row (`instance_ownership`, a singleton) and the account
//! scope is *derived* from it: an owner addresses every account on the
//! instance, everyone else only the accounts linked to their token. Claiming
//! writes no account links, so ownership and the account links are two
//! independent things — which is exactly what they are.
//!
//! The recovery story follows from that. Ownership is a cookie, so clearing
//! cookies loses the token; but nothing about the library was ever tied to that
//! cookie, so re-proving `[instance].owner_secret` re-points the claim at the
//! new token and the whole library is reachable again. Before this table the
//! same recovery had to be spelled "adopt every account that belongs to nobody",
//! which is why claiming used to hand the browser the entire instance.

use chrono::Utc;
use rusqlite::{OptionalExtension as _, params};

use super::open_db;

/// How a claim was established.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClaimMethod {
    /// The first `/session/bootstrap` on an instance with no
    /// `[instance].owner_secret` — trusted self-hosting, first-come-first-served.
    Bootstrap,
    /// `POST /session/enroll` after proving `[instance].owner_secret`.
    Secret,
}

impl ClaimMethod {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            ClaimMethod::Bootstrap => "bootstrap",
            ClaimMethod::Secret => "secret",
        }
    }
}

/// What a claim attempt did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClaimOutcome {
    /// The instance was unclaimed; it is now this token's.
    Claimed,
    /// This token already owned the instance.
    AlreadyOwned,
    /// Another token owned it and the caller proved the secret, so the claim
    /// moved to them. This is the recovery path after clearing cookies.
    Transferred,
    /// Another token owns the instance and the caller proved nothing
    /// (anonymous bootstrap). Anonymous visitors never take a claimed instance.
    Refused,
}

impl ClaimOutcome {
    /// Stable label for audit logs and API responses.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            ClaimOutcome::Claimed => "claimed",
            ClaimOutcome::AlreadyOwned => "already-owned",
            ClaimOutcome::Transferred => "transferred",
            ClaimOutcome::Refused => "refused",
        }
    }
}

/// The current claim, if the instance has one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstanceClaim {
    /// The raw owner cookie value that holds the claim. Not exposed over the
    /// API: a caller only ever learns *whether* they hold it.
    pub owner_token: String,
    pub claimed_at: String,
    /// `"bootstrap"` or `"secret"` (the `instance_ownership` CHECK).
    pub method: String,
}

/// Is `owner_token` the instance owner?
pub fn is_instance_owner(owner_token: &str) -> Result<bool, String> {
    let conn = open_db()?;
    is_instance_owner_conn(&conn, owner_token)
}

/// Body of [`is_instance_owner`], split out so it can be exercised against an
/// in-memory connection.
pub(crate) fn is_instance_owner_conn(
    conn: &rusqlite::Connection,
    owner_token: &str,
) -> Result<bool, String> {
    conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM instance_ownership WHERE id = 1 AND owner_token = ?1)",
        params![owner_token],
        |r| r.get(0),
    )
    .map_err(|e| format!("check instance ownership: {e}"))
}

/// Is `owner_token` an operator on this instance?
///
/// The administration tier. Ownership ([`is_instance_owner`]) is a claim on the
/// instance and widens what a browser may *read*; this is narrower and answers
/// "may this browser operate the machine?" — the local catalog and the console.
/// It is true exactly while this token holds a link to `[runtime].admin_user`
/// that carries a grant written with the *current* `[runtime].admin_api`, so
/// rotating the key retires every grant made under the old one.
pub fn is_instance_admin(owner_token: &str) -> Result<bool, String> {
    let conn = open_db()?;
    let fingerprint = crate::crypto::admin_key_fingerprint(&crate::models::cfg().admin_api);
    admin_grant_exists(&conn, owner_token, &fingerprint)
}

/// Does `owner_token` hold a grant written with `fingerprint`?
///
/// The fingerprint is a parameter rather than read from the config here so the
/// rule can be exercised against an in-memory connection without a loaded
/// config, and so "the grant matches the key" stays visible at the call site.
pub(crate) fn admin_grant_exists(
    conn: &rusqlite::Connection,
    owner_token: &str,
    fingerprint: &str,
) -> Result<bool, String> {
    conn.query_row(
        IS_INSTANCE_ADMIN_SQL,
        params![owner_token, fingerprint],
        |r| r.get(0),
    )
    .map_err(|e| format!("check instance administration: {e}"))
}

/// Has anybody been made an operator *under the current key*?
///
/// `false` means the administration tier is unclaimed, which is the only state
/// in which linking `admin_user` is worth offering — and the reason a key
/// rotation re-opens it.
pub fn admin_available() -> Result<bool, String> {
    let conn = open_db()?;
    let fingerprint = crate::crypto::admin_key_fingerprint(&crate::models::cfg().admin_api);
    admin_tier_available(&conn, &fingerprint)
}

pub(crate) fn admin_tier_available(
    conn: &rusqlite::Connection,
    fingerprint: &str,
) -> Result<bool, String> {
    conn.query_row(
        "SELECT NOT EXISTS(SELECT 1 FROM account_device_links \
         WHERE admin_granted_at IS NOT NULL AND admin_key_fingerprint = ?1)",
        params![fingerprint],
        |r| r.get(0),
    )
    .map_err(|e| format!("check administration availability: {e}"))
}

/// The claim row, or `None` while the instance is unclaimed.
pub fn instance_claim() -> Result<Option<InstanceClaim>, String> {
    let conn = open_db()?;
    instance_claim_conn(&conn)
}

fn instance_claim_conn(conn: &rusqlite::Connection) -> Result<Option<InstanceClaim>, String> {
    conn.query_row(
        "SELECT owner_token, claimed_at, claim_method FROM instance_ownership WHERE id = 1",
        [],
        |r| {
            Ok(InstanceClaim {
                owner_token: r.get(0)?,
                claimed_at: r.get(1)?,
                method: r.get(2)?,
            })
        },
    )
    .optional()
    .map_err(|e| format!("read instance claim: {e}"))
}

/// Claim the instance for `owner_token`, or explain why not.
///
/// `Bootstrap` may only fill an *empty* claim: it is anonymous, so letting it
/// overwrite a claim would be a one-request takeover. `Secret` may also move an
/// existing claim to the caller, because proving `[instance].owner_secret` is
/// the operator's own authority — that is what makes the secret a recovery
/// path rather than just a gate.
pub fn claim_instance(owner_token: &str, method: ClaimMethod) -> Result<ClaimOutcome, String> {
    let now = Utc::now().to_rfc3339();
    super::with_write_tx(|tx| claim_instance_tx(tx, owner_token, method, &now))
}

fn claim_instance_tx(
    tx: &rusqlite::Transaction<'_>,
    owner_token: &str,
    method: ClaimMethod,
    now: &str,
) -> Result<ClaimOutcome, String> {
    let existing: Option<String> = tx
        .query_row(
            "SELECT owner_token FROM instance_ownership WHERE id = 1",
            [],
            |r| r.get(0),
        )
        .optional()
        .map_err(|e| format!("read instance claim: {e}"))?;

    match existing {
        None => {
            tx.execute(
                "INSERT INTO instance_ownership (id, owner_token, claimed_at, claim_method)
                 VALUES (1, ?1, ?2, ?3)",
                params![owner_token, now, method.as_str()],
            )
            .map_err(|e| format!("claim instance: {e}"))?;
            Ok(ClaimOutcome::Claimed)
        }
        Some(current) if current == owner_token => Ok(ClaimOutcome::AlreadyOwned),
        Some(_) => match method {
            ClaimMethod::Bootstrap => Ok(ClaimOutcome::Refused),
            ClaimMethod::Secret => {
                tx.execute(
                    "UPDATE instance_ownership
                        SET owner_token = ?1, claimed_at = ?2, claim_method = ?3
                      WHERE id = 1",
                    params![owner_token, now, ClaimMethod::Secret.as_str()],
                )
                .map_err(|e| format!("transfer instance claim: {e}"))?;
                Ok(ClaimOutcome::Transferred)
            }
        },
    }
}

/// How many accounts exist on this instance.
///
/// Not scoped by token: it is the denominator the settings card needs to say
/// what being the owner covers.
pub fn instance_account_count() -> Result<i64, String> {
    let conn = open_db()?;
    conn.query_row("SELECT COUNT(*) FROM accounts", [], |r| r.get(0))
        .map_err(|e| format!("count instance accounts: {e}"))
}

/// SQL fragment: subquery over the account ids bound parameter `?1` may
/// address.
///
/// One definition, because "which accounts can this token see?" is asked by the
/// account list, the console and the session card, and three copies of the rule
/// are three chances for them to disagree. The owner branch is a correlation on
/// the singleton claim row, so this stays a plain subquery.
pub(crate) const VISIBLE_ACCOUNTS_SQL: &str = "(
    SELECT account_id FROM account_device_links WHERE owner_token = ?1
    UNION
    SELECT id FROM accounts WHERE EXISTS(
        SELECT 1 FROM instance_ownership io WHERE io.id = 1 AND io.owner_token = ?1)
)";

/// SQL fragment: subquery over the account ids the *console* may show, for bound
/// parameter `?1` (token) and `?2` (administration key fingerprint).
///
/// A deliberate superset of [`VISIBLE_ACCOUNTS_SQL`]: an operator sees every
/// account on the instance while still *addressing* only its own links. That
/// asymmetry is the point — the console is a view of the machine, whereas the
/// account picker is the set of accounts this browser may act as, and widening
/// the picker to the instance would hand an operator every library on it.
///
/// Composed from [`VISIBLE_ACCOUNTS_SQL`] rather than spelled out again, so the
/// owner and link branches cannot drift between the two scopes.
pub(crate) fn console_accounts_sql() -> String {
    // `VISIBLE_ACCOUNTS_SQL` ships parenthesised so callers can paste it straight
    // after `IN`. The first arm of a compound select must not carry its own pair,
    // though — SQLite answers `IN ((SELECT …) UNION …)` with "syntax error near
    // UNION" — so exactly that outer pair is stripped here rather than the rule
    // being spelled out a second time.
    let visible = VISIBLE_ACCOUNTS_SQL
        .strip_prefix('(')
        .and_then(|sql| sql.strip_suffix(')'))
        .expect("VISIBLE_ACCOUNTS_SQL is parenthesised");
    format!(
        "({visible}
         UNION
         SELECT id FROM accounts WHERE EXISTS(
             SELECT 1 FROM account_device_links adl
              WHERE adl.owner_token = ?1 AND adl.admin_granted_at IS NOT NULL
                AND adl.admin_key_fingerprint = ?2))"
    )
}

/// SQL predicate: `?1` is the instance owner.
///
/// Kept as one string because it appears inside several hand-written queries
/// (the per-account access check, the account list, the account-by-id lookup)
/// and a second spelling of it is a second chance to get the rule wrong.
pub(crate) const IS_OWNER_SQL: &str =
    "EXISTS(SELECT 1 FROM instance_ownership WHERE id = 1 AND owner_token = ?1)";

/// SQL predicate: `?1` may address account `?2`.
///
/// The hot per-request form — two indexed point lookups — rather than
/// [`VISIBLE_ACCOUNTS_SQL`]'s set scan. Used by the interaction and account
/// routes, which ask about exactly one (token, account) pair.
pub(crate) const MAY_ADDRESS_SQL: &str = "(EXISTS(SELECT 1 FROM account_device_links \
     WHERE owner_token = ?1 AND account_id = ?2) OR EXISTS(SELECT 1 FROM instance_ownership \
     WHERE id = 1 AND owner_token = ?1))";

/// SQL query: is `?1` an operator, holding a grant made with fingerprint `?2`?
///
/// A whole `SELECT` rather than a predicate fragment: unlike the ownership
/// rules, nothing else embeds it — the console and the catalog controls all go
/// through [`is_instance_admin`] — so there is no second spelling to keep in
/// sync. The fingerprint parameter is what binds the grant to the configured
/// key; see [`crate::crypto::admin_key_fingerprint`].
pub(crate) const IS_INSTANCE_ADMIN_SQL: &str = "SELECT EXISTS(SELECT 1 FROM account_device_links \
     WHERE owner_token = ?1 AND admin_granted_at IS NOT NULL \
       AND admin_key_fingerprint = ?2)";

#[cfg(test)]
mod tests {
    use super::*;

    /// The three tables the ownership rule reads: one account linked to the
    /// seed token, one to somebody else, one to nobody.
    fn fixture() -> rusqlite::Connection {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE accounts (id INTEGER PRIMARY KEY, name TEXT);
             CREATE TABLE account_device_links (
                 owner_token TEXT NOT NULL,
                 account_id INTEGER NOT NULL,
                 linked_at TEXT NOT NULL,
                 last_seen_at TEXT NOT NULL,
                 admin_granted_at TEXT,
                 admin_key_fingerprint TEXT,
                 PRIMARY KEY(owner_token, account_id)
             );
             CREATE TABLE instance_ownership (
                 id INTEGER PRIMARY KEY CHECK (id = 1),
                 owner_token TEXT NOT NULL,
                 claimed_at TEXT NOT NULL,
                 claim_method TEXT NOT NULL CHECK (claim_method IN ('bootstrap','secret'))
             );
             INSERT INTO accounts (id, name) VALUES (1,'unlinked'), (2,'seeded'), (3,'someone');
             INSERT INTO account_device_links (owner_token, account_id, linked_at, last_seen_at)
             VALUES
                 ('calibration-seed', 2, 't', 't'),
                 ('someone-else',     3, 't', 't');",
        )
        .unwrap();
        conn
    }

    /// Claim, then commit. `claim_instance_tx` writes through a transaction, and
    /// a test that let it drop un-committed would roll the claim back and then
    /// pass for the wrong reason.
    fn claim(
        conn: &mut rusqlite::Connection,
        token: &str,
        method: ClaimMethod,
        now: &str,
    ) -> ClaimOutcome {
        let tx = conn.transaction().unwrap();
        let outcome = claim_instance_tx(&tx, token, method, now).unwrap();
        tx.commit().unwrap();
        outcome
    }

    /// Does `token` address `account`? Runs the generated predicate, so the test
    /// exercises the same SQL the routes do rather than a paraphrase of it.
    fn scope(conn: &rusqlite::Connection, token: &str, account: i64) -> bool {
        conn.query_row(
            &format!("SELECT {}", MAY_ADDRESS_SQL),
            params![token, account],
            |r| r.get(0),
        )
        .unwrap()
    }

    fn visible(conn: &rusqlite::Connection, token: &str) -> Vec<i64> {
        let mut stmt = conn
            .prepare(&format!(
                "SELECT id FROM accounts WHERE id IN {} ORDER BY id",
                VISIBLE_ACCOUNTS_SQL
            ))
            .unwrap();
        stmt.query_map(params![token], |r| r.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap()
    }

    /// The console scope, run against the same generated SQL the route uses.
    fn console(conn: &rusqlite::Connection, token: &str, fingerprint: &str) -> Vec<i64> {
        let mut stmt = conn
            .prepare(&format!(
                "SELECT id FROM accounts WHERE id IN {} ORDER BY id",
                console_accounts_sql()
            ))
            .unwrap();
        stmt.query_map(params![token, fingerprint], |r| r.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap()
    }

    /// Write a grant onto one link, the way `create_account` does once the
    /// operator has proved the configured key.
    fn grant(conn: &rusqlite::Connection, token: &str, account: i64, fingerprint: &str) {
        let n = conn
            .execute(
                "UPDATE account_device_links SET admin_granted_at = 't', admin_key_fingerprint = ?1 \
                 WHERE owner_token = ?2 AND account_id = ?3",
                params![fingerprint, token, account],
            )
            .unwrap();
        assert_eq!(n, 1, "the link must exist before it can be granted");
    }

    /// The whole point of the tier: the claim on the instance widens what a
    /// browser *reads*, not what it *operates*. If owning it were enough, the
    /// console would be no narrower than the catalog.
    #[test]
    fn owning_the_instance_does_not_make_anyone_an_operator() {
        let mut conn = fixture();
        claim(&mut conn, "owner", ClaimMethod::Secret, "t");

        assert!(is_instance_owner_conn(&conn, "owner").unwrap());
        assert!(
            !admin_grant_exists(&conn, "owner", "fp").unwrap(),
            "the owner operates only after proving [runtime].admin_api"
        );
        assert!(
            admin_tier_available(&conn, "fp").unwrap(),
            "and the tier stays open for whoever does"
        );
    }

    /// A grant is bound to the key that wrote it. Without this, rotating
    /// `admin_api` would leave the previous holder an operator forever.
    #[test]
    fn a_grant_is_bound_to_the_key_that_wrote_it() {
        let conn = fixture();
        grant(&conn, "calibration-seed", 2, "fp-old");

        assert!(admin_grant_exists(&conn, "calibration-seed", "fp-old").unwrap());
        assert!(
            !admin_grant_exists(&conn, "calibration-seed", "fp-new").unwrap(),
            "rotating [runtime].admin_api must retire the grant written under the old one"
        );
        assert!(
            !admin_grant_exists(&conn, "someone-else", "fp-old").unwrap(),
            "the grant belongs to the device link that proved the key, not to the account"
        );
    }

    /// A link with no grant is not an operator, however many accounts it holds —
    /// the same mistake the ownership rules were split out to prevent.
    #[test]
    fn linking_accounts_grants_nothing() {
        let conn = fixture();
        assert!(
            !admin_grant_exists(&conn, "calibration-seed", "fp").unwrap(),
            "a plain account link carries no administration"
        );
        assert_eq!(visible(&conn, "calibration-seed"), vec![2]);
    }

    /// The tier re-opens exactly when the configured key changes, which is what
    /// makes a rotation the recovery path for a lost console.
    #[test]
    fn rotating_the_key_reopens_the_administration_tier() {
        let conn = fixture();
        assert!(admin_tier_available(&conn, "fp-old").unwrap());

        grant(&conn, "calibration-seed", 2, "fp-old");
        assert!(!admin_tier_available(&conn, "fp-old").unwrap());

        assert!(
            admin_tier_available(&conn, "fp-new").unwrap(),
            "a rotated key must re-open the tier, or the instance would be stuck"
        );
    }

    /// The whole point of the table: claiming records the instance and does not
    /// touch a single account link.
    #[test]
    fn claiming_an_instance_writes_no_account_links() {
        let mut conn = fixture();
        let before: i64 = conn
            .query_row("SELECT COUNT(*) FROM account_device_links", [], |r| {
                r.get(0)
            })
            .unwrap();

        let outcome = claim(
            &mut conn,
            "owner",
            ClaimMethod::Bootstrap,
            "2026-01-01T00:00:00Z",
        );

        assert_eq!(outcome, ClaimOutcome::Claimed);
        assert!(is_instance_owner_conn(&conn, "owner").unwrap());
        let after: i64 = conn
            .query_row("SELECT COUNT(*) FROM account_device_links", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(before, after, "a claim must not link accounts");
    }

    /// Anonymous bootstrap is the one claim path nobody authenticates, so it must
    /// never overwrite an existing claim.
    #[test]
    fn an_anonymous_bootstrap_cannot_take_a_claimed_instance() {
        let mut conn = fixture();
        claim(
            &mut conn,
            "owner",
            ClaimMethod::Secret,
            "2026-01-01T00:00:00Z",
        );

        let outcome = claim(
            &mut conn,
            "stranger",
            ClaimMethod::Bootstrap,
            "2026-02-02T00:00:00Z",
        );

        assert_eq!(outcome, ClaimOutcome::Refused);
        assert!(is_instance_owner_conn(&conn, "owner").unwrap());
        assert!(!is_instance_owner_conn(&conn, "stranger").unwrap());
    }

    /// Proving the secret moves the claim — the recovery path after clearing
    /// cookies, and the reason the library survives it.
    #[test]
    fn proving_the_secret_moves_the_claim_to_the_new_cookie() {
        let mut conn = fixture();
        claim(
            &mut conn,
            "old-cookie",
            ClaimMethod::Bootstrap,
            "2026-01-01T00:00:00Z",
        );

        let outcome = claim(
            &mut conn,
            "new-cookie",
            ClaimMethod::Secret,
            "2026-02-02T00:00:00Z",
        );

        assert_eq!(outcome, ClaimOutcome::Transferred);
        assert!(!is_instance_owner_conn(&conn, "old-cookie").unwrap());
        assert!(is_instance_owner_conn(&conn, "new-cookie").unwrap());
        let claim = instance_claim_conn(&conn).unwrap().unwrap();
        assert_eq!(claim.method, "secret");
        assert_eq!(claim.claimed_at, "2026-02-02T00:00:00Z");
    }

    #[test]
    fn claiming_twice_is_idempotent() {
        let mut conn = fixture();
        assert_eq!(
            claim(
                &mut conn,
                "owner",
                ClaimMethod::Bootstrap,
                "2026-01-01T00:00:00Z"
            ),
            ClaimOutcome::Claimed
        );
        assert_eq!(
            claim(
                &mut conn,
                "owner",
                ClaimMethod::Bootstrap,
                "2026-01-02T00:00:00Z"
            ),
            ClaimOutcome::AlreadyOwned
        );
        let rows: i64 = conn
            .query_row("SELECT COUNT(*) FROM instance_ownership", [], |r| r.get(0))
            .unwrap();
        assert_eq!(rows, 1, "the claim is a singleton");
    }

    /// The scope rule, stated once: the owner addresses every account, everyone
    /// else only their own links — and the claim grants nothing to them.
    #[test]
    fn the_owner_addresses_every_account_and_others_only_their_links() {
        let mut conn = fixture();

        assert!(scope(&conn, "calibration-seed", 2));
        assert!(!scope(&conn, "calibration-seed", 1));
        assert!(!scope(&conn, "calibration-seed", 3));
        assert!(!scope(&conn, "someone-else", 2));
        assert!(!scope(&conn, "nobody", 1));

        claim(
            &mut conn,
            "owner",
            ClaimMethod::Secret,
            "2026-01-01T00:00:00Z",
        );

        // The owner holds no link at all and still addresses every account.
        assert!(scope(&conn, "owner", 1));
        assert!(scope(&conn, "owner", 2));
        assert!(scope(&conn, "owner", 3));
        // Everyone else is unchanged.
        assert!(scope(&conn, "calibration-seed", 2));
        assert!(!scope(&conn, "calibration-seed", 1));
        assert!(scope(&conn, "someone-else", 3));
        assert!(!scope(&conn, "someone-else", 1));
    }

    #[test]
    fn the_visible_account_set_follows_the_claim() {
        let mut conn = fixture();

        assert_eq!(visible(&conn, "calibration-seed"), vec![2]);
        assert_eq!(visible(&conn, "nobody"), Vec::<i64>::new());

        claim(
            &mut conn,
            "owner",
            ClaimMethod::Secret,
            "2026-01-01T00:00:00Z",
        );

        assert_eq!(visible(&conn, "owner"), vec![1, 2, 3]);
        assert_eq!(visible(&conn, "calibration-seed"), vec![2]);
    }

    /// The console scope is a superset of the picker's, and the difference is
    /// the whole point: an operator sees every account on the instance while
    /// still *addressing* only its own links. Collapsing the two is what would
    /// put every library on the instance into the account picker.
    #[test]
    fn the_console_sees_the_instance_while_the_picker_stays_on_the_links() {
        let conn = fixture();
        grant(&conn, "calibration-seed", 2, "fp");

        assert_eq!(visible(&conn, "calibration-seed"), vec![2]);
        assert_eq!(console(&conn, "calibration-seed", "fp"), vec![1, 2, 3]);
        assert!(
            !scope(&conn, "calibration-seed", 1),
            "seeing an account in the console must not make it addressable"
        );

        // A grant written under another key is not a grant.
        assert_eq!(console(&conn, "calibration-seed", "other"), vec![2]);

        // And a browser that is not an operator gains nothing.
        assert_eq!(console(&conn, "someone-else", "fp"), vec![3]);
    }

    /// The console scope folds in the owner branch, so an owner that never
    /// proved a key keeps the instance-wide view the claim already gives it.
    #[test]
    fn the_console_scope_keeps_the_owner_branch() {
        let mut conn = fixture();
        claim(&mut conn, "owner", ClaimMethod::Secret, "t");

        assert_eq!(console(&conn, "owner", "fp"), vec![1, 2, 3]);
    }
}
