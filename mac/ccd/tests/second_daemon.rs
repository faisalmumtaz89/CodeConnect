//! Two `ccd` processes in one home: the one already running keeps it.
//!
//! Real processes, because what is at stake is what survives one of them
//! exiting. Every daemon here runs in a home of its own ([`common::Home`]).

mod common;

use std::os::unix::net::UnixStream;
use std::path::Path;

use common::Home;

fn answers(socket: &Path) -> bool {
    UnixStream::connect(socket).is_ok()
}

#[test]
fn a_second_daemon_leaves_the_running_one_reachable_and_its_state_untouched() {
    let home = Home::new("second");
    let first = home.start("first.out");
    first.wait_until_serving();
    assert!(answers(&home.socket()));

    let mut second = home.start("second.out");
    let status = second.wait_for_exit();
    let said = second.output();

    assert!(
        answers(&home.socket()),
        "the running daemon's socket is gone:\n{said}"
    );
    assert!(
        !said.contains("recovery:"),
        "the second daemon ran recovery against the running one's database:\n{said}"
    );
    assert!(!status.success(), "the second daemon must fail:\n{said}");
    assert!(
        said.contains("another ccd is already running"),
        "the second daemon must say why:\n{said}"
    );
}

#[test]
fn a_daemon_killed_outright_leaves_nothing_that_stops_the_next_one() {
    let home = Home::new("stale");
    let mut first = home.start("first.out");
    first.wait_until_serving();
    first.child.kill().unwrap();
    first.child.wait().unwrap();
    assert!(home.socket().exists(), "SIGKILL leaves the socket behind");

    let next = home.start("next.out");
    next.wait_until_serving();
    assert!(answers(&home.socket()), "{}", next.output());
}
