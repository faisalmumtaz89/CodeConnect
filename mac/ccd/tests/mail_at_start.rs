//! What a starting `ccd` does with the mail it finds: a message a stopped daemon
//! was handing to an agent is unread again, and one past its day is expired by the
//! sweep the daemon starts. A real process, because both are the start-up's own
//! doing ([`common::Home`]).

// Each test file builds the helpers for itself and uses only some of them.
#[allow(dead_code)]
mod common;

use std::time::{Duration, Instant};

use common::Home;

/// One `mail` row for run `run`, at `state`, taken at `accepted_ms` and unread
/// until `expires_ms`.
fn leave(
    db: &rusqlite::Connection,
    id: &str,
    run: &str,
    state: &str,
    accepted_ms: i64,
    expires_ms: i64,
) {
    db.execute(
        "INSERT INTO mail(message_id, context_id, session_uid, sender_computer, sender_uid,
                          sender, sender_label, sender_agent, sender_project, text,
                          accepted_ms, expires_ms, state, offer, sender_told)
         VALUES (?1, ?1, ?2, '', 'sender', 'sender@studio', 'cc-2@studio', 'Codex', 'api',
                 'hi', ?3, ?4, ?5, CASE WHEN ?5 = 'offered' THEN 'o1' END, 1)",
        rusqlite::params![id, run, accepted_ms, expires_ms, state],
    )
    .unwrap();
}

fn state(db: &rusqlite::Connection, id: &str) -> String {
    db.query_row(
        "SELECT state FROM mail WHERE message_id = ?1",
        [id],
        |row| row.get(0),
    )
    .unwrap()
}

#[test]
fn a_starting_daemon_reoffers_mail_left_mid_read_and_sweeps_expired_mail() {
    let home = Home::new("mail");
    // The first start writes the schema; the second finds the mail.
    let first = home.start("first.out");
    first.wait_until_serving();
    drop(first);

    let db = rusqlite::Connection::open(home.0.join("events.db")).unwrap();
    let run = "01K1B3XQ8ZC0DE5FGH7JKMNPQR";
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;
    let hour = 60 * 60 * 1000;
    leave(&db, "mid-read", run, "offered", now, now + 24 * hour);
    leave(
        &db,
        "past-its-day",
        run,
        "waiting",
        now - 25 * hour,
        now - hour,
    );

    let second = home.start("second.out");
    second.wait_until_serving();
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let (left, past) = (state(&db, "mid-read"), state(&db, "past-its-day"));
        if left == "waiting" && past == "expired" {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "after start: mid-read is {left}, past-its-day is {past}\n{}",
            second.output()
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}
