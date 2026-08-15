//! One worker per device, and the order it starts work in.
//!
//! **Why the ordering is the daemon's and not a transport's.** APNs keeps a
//! single offline notification per app, so which push a reader is holding when
//! they pick the phone up is settled by the order the pushes were submitted in
//! — and that order is a statement about what the daemon decided, not about how
//! the bytes reached Apple. A direct HTTP/2 POST and a relayed hop carry the
//! same obligation to preserve it, so what preserves it lives here rather than
//! inside either of them, where it would have to be written twice and could
//! then disagree with itself.
//!
//! **What a transport supplies is one closure**: attempt this delivery, and say
//! whether Apple took it. `serve_with` owns everything around that — whose turn
//! it is, what waits and what is dropped while it waits, and when a device has
//! left for good. What a transport supplies beyond the closure is how it
//! reaches the far end; the bounds it runs under are the same either way,
//! because they are answers to "how long may a phone's next notification wait
//! behind this one", which has nothing to do with who is being dialled.

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::Result;
use protocol::secret::Redacted;
use push_core::{ApnsEnvironment, DeviceGone};

use crate::apns::TestDelivery;

/// How long one delivery may take in total, retry included.
///
/// **The legs are bounded individually and that is not enough.** A TLS
/// handshake and the read of an error body have no deadline of their own, and a
/// device's deliveries run one after another — so a single stalled attempt
/// would hold up every later push to that phone, and any test notification
/// waiting behind it, for as long as the peer stayed silent. This bounds the
/// whole attempt rather than adding a deadline to each leg one bug at a time.
///
/// One number for both transports: the relay answers only once Apple has, so it
/// is spending the same budget from the same instant, and two constants here
/// would be two chances to move only one of them.
pub(crate) const DELIVERY_DEADLINE: std::time::Duration = std::time::Duration::from_secs(45);

/// The bound on any single leg — the connect, the handshake, the wait for a
/// response.
///
/// **Measured, not assumed.** A third-party network filter that cannot identify
/// a freshly built binary holds its connections open and silent rather than
/// refusing them, which produced a delivery that never completed and never
/// logged. A push is best-effort; a push that hangs is worse than one that
/// fails, because only the failure is visible.
pub(crate) const CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// Where a push is going.
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct PushTarget {
    /// The device's APNs token, hex, as the phone reported it.
    pub(crate) token: String,
    pub(crate) environment: ApnsEnvironment,
    /// The CodeConnect device id, so an unregistered token can be cleared from
    /// the right row.
    pub(crate) device_id: String,
    /// The relay bearer for this token, for a daemon in relay mode. `None` in
    /// direct mode, where there is no third party to authorise.
    ///
    /// **The type is the guard.** [`Redacted`] cannot print its value, so the
    /// bearer survives no `Debug` — not the one derived on this struct's
    /// callers, not the field printed on its own, not the `anyhow` chain three
    /// layers away that nobody was thinking about secrets while writing. A bare
    /// `String` here made the redaction a promise about a hand-written `Debug`
    /// that the next carrier to hold this field could quietly break.
    pub(crate) credential: Option<Redacted>,
}

/// **Hand-written so the token is abbreviated.** The credential renders itself
/// safely — it is a [`Redacted`] — but the token is a bare `String`, and a
/// derived `Debug` would print it whole; enough of it to recognise across two
/// log lines is all a reader needs and all this shows.
impl std::fmt::Debug for PushTarget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PushTarget")
            .field("token", &crate::store::abbreviated(&self.token))
            .field("environment", &self.environment)
            .field("device_id", &self.device_id)
            .field("credential", &self.credential)
            .finish()
    }
}

/// Whether this push is for this device.
///
/// **A function, so the seen-filter can be tested.** A device whose live socket
/// already carried the fact must not also be rung, and that is the whole of the
/// gate — worth asserting directly rather than inferring from a payload.
///
/// **The list, not a predicate.** `send` iterates exactly what this returns, so
/// a test of this function is a test of who gets rung — which a per-target
/// predicate could only be if every call site were also read.
pub(crate) fn recipients(targets: Vec<PushTarget>, excluded: &[String]) -> Vec<PushTarget> {
    targets
        .into_iter()
        .filter(|target| {
            if excluded.contains(&target.device_id) {
                // This device's live socket already carried the fact. Ringing
                // it again is the noise the gate exists to stop.
                crate::log_debug!("push: {} already saw this live; skipping", target.device_id);
                return false;
            }
            true
        })
        .collect()
}

/// Supplies the devices to push to, so the sender does not reach into the
/// database and the tests do not need one.
pub(crate) trait PushRegistry: Send + Sync {
    fn targets(&self) -> Vec<PushTarget>;
    /// Apple said this token is dead (`410 Unregistered`) — the app is gone
    /// from that device, which never recovers.
    ///
    /// A `400 BadDeviceToken` is deliberately not this: it is far more often
    /// the *wrong host* for a perfectly good token, which is why the delivery
    /// path retries against the other environment and records a correction
    /// before it would ever give up on a device.
    /// `refused` names the token *and* the credential the attempt was made
    /// with, so a late refusal cannot erase a tuple registered since. Returns
    /// whether it *was* the registered tuple: `false` means the phone has
    /// re-registered and this answer is about a device that is, as far as
    /// anything now cares, still there.
    ///
    /// **Both halves, because a rotation moves either.** A relay reissues a
    /// credential for the same token; a phone reinstalls and registers a new
    /// token. An attempt snapshots one `(token, credential)` tuple and answers
    /// against whatever the row holds now — so a `410` about `(T, C1)` must be
    /// inert once the row reads `(T, C2)`, exactly as it is inert once the row
    /// reads `(T2, …)`. Comparing the token alone would let `C1`'s late
    /// departure clear a live `C2`.
    fn forget(
        &self,
        device_id: &str,
        refused_token: &str,
        refused_credential: Option<&Redacted>,
        reason: &str,
    ) -> bool;
    /// Apple refused the token on the host we chose but the *other* host is
    /// plausible. Records the correction so the next push goes straight there.
    /// Scoped to the whole `(token, credential)` tuple, for the same reason as
    /// `forget`: a correction snapshotted under `(T, C1)` must not move the
    /// environment of a row the phone has since rotated to `(T, C2)`.
    fn correct_environment(
        &self,
        device_id: &str,
        token: &str,
        credential: Option<&Redacted>,
        environment: ApnsEnvironment,
    );
}

/// One delivery, waiting its turn.
pub(crate) struct Delivery {
    pub(crate) target: PushTarget,
    pub(crate) payload: String,
    /// Which slot on the phone this replaces — see `COLLAPSE_ID`.
    pub(crate) collapse: &'static str,
    /// Set only for the test push, which reports back to whoever asked for it.
    pub(crate) respond: Option<tokio::sync::oneshot::Sender<TestDelivery>>,
}

/// **One device's pushes, in the order the daemon decided them.**
///
/// APNs keeps a single offline notification per app and does not say which, so
/// which push a reader ends up holding is decided by arrival order. Delivering
/// each push on its own task made that order the network's to choose: an
/// approval could reach Apple before the weaker notice it supersedes, and the
/// notice would be the one kept. A device's deliveries therefore go through one
/// worker that finishes each attempt — retry included — before starting the
/// next, so the last push the daemon decided is the last one submitted.
///
/// Submission order is all a sender controls: Apple does not promise to keep
/// the newest of several stored notifications. What makes the newest *win* is
/// the collapse id on the request; ordering here is what makes "newest" mean
/// what the daemon meant by it.
///
/// Ordering is per device and nothing more: two devices never wait on each
/// other.
pub(crate) struct DeviceQueue {
    waiting: std::sync::Mutex<std::collections::VecDeque<Delivery>>,
    wake: tokio::sync::Notify,
    /// Set when the device is gone, so the worker and its watcher end rather
    /// than waiting forever on a phone that will never be pushed to again.
    closed: std::sync::atomic::AtomicBool,
}

/// How many pushes may wait behind an in-flight one before the oldest is
/// dropped.
///
/// A doorbell is worth ringing about the present. If deliveries to a device are
/// backing up — Apple unreachable, a network holding connections open — the
/// pushes at the front describe a state the run has long since left, and
/// keeping them would mean a reader eventually receives the *oldest* stale
/// fact. The drop is logged; it is never silent.
const MAX_WAITING: usize = 8;

impl DeviceQueue {
    pub(crate) fn push(&self, delivery: Delivery, device_id: &str) {
        let mut waiting = self.waiting.lock().unwrap();
        // **Checked under the same lock the drain holds.** A queue can be
        // retired between being handed out and being pushed to, and appending
        // after the drain would leave the work with no worker — and, for a test
        // push, whoever asked for it waiting on a channel with nothing coming.
        // `close` sets this before it drains, so one of the two orders holds:
        // this push lands first and the drain answers it, or the drain has
        // happened and this sees it here.
        if self.is_closed() {
            crate::log_info!("push: {device_id} was retired; dropping a delivery filed for it");
            if let Some(respond) = delivery.respond {
                let _ = respond.send(TestDelivery::NoToken);
            }
            return;
        }
        // **One doorbell waiting at a time, and it is the newest.** A phone
        // holds one, and a later one replaces it — so a queue of them is a
        // queue of facts that will each be overwritten by the next, with only
        // the last surviving. Keeping a backlog would mean a delivery that
        // waited out a slow one describing a fleet that had moved on minutes
        // ago. A test push is not a doorbell and is never coalesced: somebody
        // asked for it and is waiting to be told what happened.
        if delivery.respond.is_none() {
            // **Removed, not overwritten in place.** Replacing it where it sat
            // would send the newer doorbell ahead of a test push filed between
            // them, and the queue's whole promise is that work leaves in the
            // order it was decided.
            if let Some(at) = waiting.iter().position(|d| d.respond.is_none()) {
                crate::log_debug!(
                    "push: a newer doorbell replaced the one waiting for {device_id}"
                );
                waiting.remove(at);
            }
        }
        if waiting.len() >= MAX_WAITING {
            // **The oldest *test*.** Doorbells coalesce, so at most one is ever
            // waiting and it is the only ordinary notification this device is
            // going to get; discarding it to admit a test would trade the
            // message that matters for one somebody can ask for again.
            if let Some(at) = waiting.iter().position(|d| d.respond.is_some()) {
                crate::log_warn!(
                    "push: {device_id} is {MAX_WAITING} deliveries behind; dropping the oldest test",
                );
                // Told, rather than left waiting on a channel nobody answers.
                if let Some(respond) = waiting.remove(at).and_then(|d| d.respond) {
                    let _ = respond.send(TestDelivery::Failed(
                        "the queue for this device is backed up; nothing was sent".into(),
                    ));
                }
            }
        }
        waiting.push_back(delivery);
        self.wake.notify_one();
    }

    fn take(&self) -> Option<Delivery> {
        self.waiting.lock().unwrap().pop_front()
    }

    /// This device is gone. Whatever was waiting for it is dropped, and the
    /// worker is told to stop.
    ///
    /// **Not an abort.** Cancelling the tasks would leave a delivery half sent
    /// and a test push unanswered; ending the loop lets it finish what it is
    /// doing and return, and the watcher then sees a worker that stopped on
    /// purpose rather than one that fell over.
    fn close(&self, device_id: &str) {
        self.closed
            .store(true, std::sync::atomic::Ordering::Release);
        for stranded in self.waiting.lock().unwrap().drain(..) {
            if let Some(respond) = stranded.respond {
                let _ = respond.send(TestDelivery::NoToken);
            }
        }
        crate::log_info!("push: {device_id} is no longer registered; its queue is closed");
        self.wake.notify_one();
    }

    fn is_closed(&self) -> bool {
        self.closed.load(std::sync::atomic::Ordering::Acquire)
    }
}

/// This device's queue, or a fresh one.
///
/// **A closed queue is never handed back.** Its worker has returned, so work
/// pushed onto it would sit there for the life of the process — and a device id
/// outlives the token behind it: a phone that reinstalls registers a new token
/// under the same row, and it has to start ringing again.
pub(crate) fn queue_for(
    queues: &mut HashMap<String, Arc<DeviceQueue>>,
    device_id: &str,
) -> (Arc<DeviceQueue>, bool) {
    if let Some(queue) = queues.get(device_id) {
        if !queue.is_closed() {
            return (Arc::clone(queue), false);
        }
    }
    let queue = Arc::new(DeviceQueue {
        waiting: std::sync::Mutex::new(std::collections::VecDeque::new()),
        wake: tokio::sync::Notify::new(),
        closed: std::sync::atomic::AtomicBool::new(false),
    });
    queues.insert(device_id.to_string(), Arc::clone(&queue));
    (queue, true)
}

/// Retire this device's queue: its worker ends and whatever was waiting is
/// answered rather than abandoned.
pub(crate) fn retire(queues: &mut HashMap<String, Arc<DeviceQueue>>, device_id: &str) {
    if let Some(queue) = queues.remove(device_id) {
        queue.close(device_id);
    }
}

/// The worker loop, over a world it is handed.
///
/// **Each attempt is awaited whole**, retry included: spawning it and
/// moving on would put the deliveries back on the network's schedule, which
/// is the reordering this exists to prevent. The loop is written against
/// two closures so that ordering, serialisation and the revocation check
/// can be proven without a network — the thing they guard is *when* work
/// starts, which no test of a queue's contents can see.
pub(crate) async fn serve_with<A, Fut>(queue: Arc<DeviceQueue>, attempt: A)
where
    A: Fn(PushTarget, String, &'static str) -> Fut,
    Fut: std::future::Future<Output = Result<Option<String>>>,
{
    loop {
        if queue.is_closed() {
            return;
        }
        let Some(delivery) = queue.take() else {
            // A `notify_one` with nobody waiting leaves a permit, so a push
            // filed in the instant between the take above and this line
            // wakes it immediately rather than sitting until the next one.
            queue.wake.notified().await;
            continue;
        };
        let device = delivery.target.device_id.clone();
        let outcome = attempt(delivery.target, delivery.payload, delivery.collapse).await;
        // **Terminal for this worker.** A device leaves once, and a loop
        // that merely logged the failure would park forever on a phone that
        // is never coming back. Closing answers whatever else was waiting
        // rather than abandoning it, and `enqueue` starts a fresh queue if
        // the same device id ever registers a new token.
        let gone = matches!(&outcome, Err(err) if err.chain().any(|e| e.is::<DeviceGone>()));
        match (outcome, delivery.respond) {
            (Ok(apns_id), Some(respond)) => {
                crate::log_info!("push: test accepted for device {device}");
                let _ = respond.send(TestDelivery::Accepted { apns_id });
            }
            (Err(err), Some(respond)) => {
                crate::log_warn!("push: test to {device} failed: {err:#}");
                // **Classified, not stringified.** A refused credential and an
                // unreachable relay are one `Err` here and two different things
                // to do about it on the phone, so the typed markers a transport
                // attached are read rather than flattened into prose the phone
                // would have to match on.
                let _ = respond.send(TestDelivery::from_error(&err));
            }
            (Ok(_), None) => crate::log_info!("push: delivered to device {device}"),
            (Err(err), None) => crate::log_warn!("push: {device}: {err:#}"),
        }
        if gone {
            queue.close(&device);
            return;
        }
    }
}

/// Keep a task running until it stops on purpose.
///
/// A worker returns normally exactly twice: when its queue is retired, and when
/// Apple says the app is gone from the device. Anything else — a panic — leaves
/// a phone that quietly stops ringing while its queue goes on accepting pushes
/// nothing will ever take, so that ending starts another worker and this one
/// does not.
pub(crate) async fn keep_running<F>(mut start: F, device_id: &str)
where
    F: FnMut() -> tokio::task::JoinHandle<()>,
{
    loop {
        match start().await {
            // Stopped on purpose: the device is retired, or Apple has
            // disowned it. There is nothing left to serve.
            Ok(()) => return,
            Err(err) if err.is_panic() => {
                crate::log_warn!("push: the worker for {device_id} died; starting another");
            }
            // Cancelled, which is a shutdown, not a fault.
            Err(_) => return,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use push_core::{COLLAPSE_ID, TEST_COLLAPSE_ID};

    fn target(device: &str) -> PushTarget {
        PushTarget {
            token: "aa".into(),
            environment: ApnsEnvironment::Sandbox,
            device_id: device.into(),
            credential: None,
        }
    }

    /// **The carrier cannot print its bearer, and the type is why.** Every
    /// struct that holds a `PushTarget` derives `Debug`, and a bare `String`
    /// credential would ride into a log the first time any of them was printed
    /// in an error path nobody was thinking about secrets while writing. The
    /// bearer is a [`Redacted`], so the field renders `<redacted>` no matter who
    /// prints the target.
    ///
    /// **The negative check:** make the `credential` field a bare `String` and
    /// this fails — the value appears in the rendering.
    #[test]
    fn a_targets_debug_never_prints_its_bearer() {
        let mut with_bearer = target("phone");
        with_bearer.credential = Some(Redacted::from("s3cret-bearer-nobody-may-log"));
        let rendered = format!("{with_bearer:?}");
        assert!(
            !rendered.contains("s3cret-bearer-nobody-may-log"),
            "the bearer reached a Debug rendering: {rendered}"
        );
        assert!(
            rendered.contains("<redacted>"),
            "and its presence is still visible, which an operator needs: {rendered}"
        );
        // The token is a bare string and is shown, abbreviated — a reader has to
        // be able to tell two registrations apart.
        assert!(
            rendered.contains("aa"),
            "the token still prints: {rendered}"
        );
    }

    /// Await something a regression would never deliver, without hanging.
    ///
    /// **Every wait in these tests is bounded.** The failures they guard
    /// against are answers that never come, so an unbounded await turns a red
    /// test into a stuck one — and CI runs plain `cargo test`, with no
    /// per-test deadline to catch it.
    async fn within<F: std::future::Future>(what: &str, f: F) -> F::Output {
        tokio::time::timeout(std::time::Duration::from_secs(5), f)
            .await
            .unwrap_or_else(|_| panic!("{what} never arrived"))
    }

    fn queue() -> DeviceQueue {
        DeviceQueue {
            waiting: std::sync::Mutex::new(std::collections::VecDeque::new()),
            wake: tokio::sync::Notify::new(),
            closed: std::sync::atomic::AtomicBool::new(false),
        }
    }

    fn delivery(body: &str) -> Delivery {
        Delivery {
            target: target("phone"),
            payload: body.to_string(),
            collapse: COLLAPSE_ID,
            respond: None,
        }
    }

    fn drained(queue: &DeviceQueue) -> Vec<String> {
        std::iter::from_fn(|| queue.take())
            .map(|d| d.payload)
            .collect()
    }

    /// **The worker starts work in order, one at a time, and only the newest
    /// doorbell is ever waiting.**
    ///
    /// The queue's contents cannot show the first two: a loop that spawned
    /// every delivery would drain it in the same order and still let the
    /// network decide who reaches Apple first. What is asserted here is the
    /// order work *started* in, that each attempt finished before the next
    /// began, and that a doorbell filed while one is in flight replaces the one
    /// waiting rather than joining a backlog.
    ///
    /// Nothing here waits out a delay: each attempt announces itself and then
    /// blocks until the test lets it go, so the interleaving is one the test
    /// chose rather than one it hoped for.
    #[tokio::test]
    async fn deliveries_start_in_order_and_only_the_newest_waits() {
        let queue = Arc::new(queue());
        let log = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let in_flight = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let release = Arc::new(tokio::sync::Notify::new());
        let (started, mut starts) = tokio::sync::mpsc::unbounded_channel();

        let (seen, busy, held) = (
            Arc::clone(&log),
            Arc::clone(&in_flight),
            Arc::clone(&release),
        );
        let worker = tokio::spawn(serve_with(
            Arc::clone(&queue),
            move |_target, payload, _collapse| {
                let (seen, busy, held, started) = (
                    Arc::clone(&seen),
                    Arc::clone(&busy),
                    Arc::clone(&held),
                    started.clone(),
                );
                async move {
                    assert_eq!(
                        busy.fetch_add(1, std::sync::atomic::Ordering::SeqCst),
                        0,
                        "a second delivery began while one was still in flight"
                    );
                    let _ = started.send(payload.clone());
                    held.notified().await;
                    seen.lock().unwrap().push(payload);
                    busy.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
                    Ok(None)
                }
            },
        ));

        queue.push(delivery("first"), "phone");
        assert_eq!(
            within("the first delivery's start", starts.recv())
                .await
                .unwrap(),
            "first",
            "the first one starts"
        );

        // Both arrive while the first is still in flight. The second never
        // reaches Apple: the third is the state of the world by then.
        queue.push(delivery("second"), "phone");
        queue.push(delivery("third"), "phone");
        release.notify_one();
        assert_eq!(
            within("the replacement's start", starts.recv())
                .await
                .unwrap(),
            "third",
            "the newest doorbell replaced the one waiting"
        );
        release.notify_one();

        for _ in 0..200 {
            if log.lock().unwrap().len() == 2 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        worker.abort();
        assert_eq!(*log.lock().unwrap(), ["first", "third"]);
    }

    /// Retiring a device ends its worker and answers what it was holding.
    #[tokio::test]
    async fn retiring_a_device_closes_its_queue_and_answers_its_work() {
        let mut queues: HashMap<String, Arc<DeviceQueue>> = HashMap::new();
        let (queue, _) = queue_for(&mut queues, "phone");
        let (respond, rx) = tokio::sync::oneshot::channel();
        queue.push(
            Delivery {
                target: target("phone"),
                payload: "test".into(),
                collapse: TEST_COLLAPSE_ID,
                respond: Some(respond),
            },
            "phone",
        );

        retire(&mut queues, "phone");
        assert!(queue.is_closed(), "the worker is told to stop");
        assert!(queues.is_empty(), "and the device is forgotten");
        assert!(
            matches!(
                within("the retired work's answer", rx).await,
                Ok(TestDelivery::NoToken)
            ),
            "whoever was waiting is told, rather than left on a dead channel"
        );
    }

    /// **A delivery filed for a queue that has just been retired is answered,
    /// not stranded.** The queue is handed out while open and pushed to a
    /// moment later; a worker can retire it in between. Appending after the
    /// drain would leave a test push waiting on a channel with nothing coming,
    /// and the socket handler that asked for it waiting with it.
    #[tokio::test]
    async fn a_delivery_filed_after_retirement_is_answered_rather_than_stranded() {
        let queue = queue();
        // Handed out open, retired before the push lands.
        queue.close("phone");

        let (respond, rx) = tokio::sync::oneshot::channel();
        queue.push(
            Delivery {
                target: target("phone"),
                payload: "test".into(),
                collapse: TEST_COLLAPSE_ID,
                respond: Some(respond),
            },
            "phone",
        );
        // **Bounded.** The failure this guards against is an answer that never
        // comes, so an unbounded await would hang here rather than fail — and a
        // suite that hangs is worse than one that goes red.
        assert!(
            matches!(within("an answer", rx).await, Ok(TestDelivery::NoToken)),
            "whoever asked is told, rather than left waiting on a dead channel"
        );

        queue.push(delivery("doorbell"), "phone");
        assert!(
            drained(&queue).is_empty(),
            "and nothing is left behind for a worker that is gone"
        );
    }

    /// **A device that comes back gets a working queue.** A device id outlives
    /// the token behind it — a phone that reinstalls registers a new one under
    /// the same row — and handing back the retired queue would file its pushes
    /// somewhere nothing drains.
    #[test]
    fn a_device_that_registers_again_is_not_handed_its_retired_queue() {
        let mut queues: HashMap<String, Arc<DeviceQueue>> = HashMap::new();

        let (first, is_new) = queue_for(&mut queues, "phone");
        assert!(is_new, "the first ask starts one");
        let (again, is_new) = queue_for(&mut queues, "phone");
        assert!(!is_new, "and the second is the same queue, still running");
        assert!(Arc::ptr_eq(&first, &again));

        first.close("phone");
        let (fresh, is_new) = queue_for(&mut queues, "phone");
        assert!(is_new, "a retired queue is replaced, not reused");
        assert!(!Arc::ptr_eq(&first, &fresh));
        assert!(!fresh.is_closed());
    }

    /// **Apple saying the app is gone retires the worker.**
    ///
    /// A device leaves once, and a loop that only logged the refusal would park
    /// forever on a phone that is never coming back. What is left waiting is
    /// answered rather than abandoned, and nothing more is sent.
    #[tokio::test]
    async fn a_device_apple_says_is_gone_retires_its_worker() {
        let queue = Arc::new(queue());
        queue.push(delivery("doorbell"), "phone");
        let (respond, rx) = tokio::sync::oneshot::channel();
        queue.push(
            Delivery {
                target: target("phone"),
                payload: "test".into(),
                collapse: TEST_COLLAPSE_ID,
                respond: Some(respond),
            },
            "phone",
        );

        let sent = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let seen = Arc::clone(&sent);
        let worker = tokio::spawn(serve_with(
            Arc::clone(&queue),
            move |_target, payload, _collapse| {
                let seen = Arc::clone(&seen);
                async move {
                    seen.lock().unwrap().push(payload);
                    Err(anyhow::Error::new(DeviceGone).context("410 Unregistered"))
                }
            },
        ));

        // The worker ends rather than parking: awaiting it is the assertion.
        tokio::time::timeout(std::time::Duration::from_secs(5), worker)
            .await
            .expect("the worker for a departed device ends")
            .expect("and does so without panicking");
        assert_eq!(
            *sent.lock().unwrap(),
            ["doorbell"],
            "it stops at the refusal rather than working through the queue"
        );
        assert!(
            matches!(
                within("the test push's answer", rx).await,
                Ok(TestDelivery::NoToken)
            ),
            "and the test push behind it is answered, not abandoned"
        );
        assert!(queue.is_closed(), "the queue is retired with the worker");
    }

    /// **A worker that dies is replaced without waiting for the next push.**
    ///
    /// The queue would go on accepting deliveries that nothing took, which on
    /// a phone is indistinguishable from a daemon that has stopped noticing —
    /// and the next push that would have revealed it might be hours away.
    #[tokio::test]
    async fn a_worker_that_dies_is_started_again() {
        let attempts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counted = Arc::clone(&attempts);
        // Bounded like every other wait here: the regression this guards
        // against is a supervisor that never returns, which would hang the
        // suite rather than fail it.
        within(
            "the supervisor's last worker",
            keep_running(
                move || {
                    let counted = Arc::clone(&counted);
                    tokio::spawn(async move {
                        // The first two die the way a worker must not: a panic
                        // inside the loop. The third returns the way a retired
                        // one does, which is how this test ends.
                        if counted.fetch_add(1, std::sync::atomic::Ordering::SeqCst) < 2 {
                            panic!("the worker fell over");
                        }
                    })
                },
                "phone",
            ),
        )
        .await;
        assert_eq!(
            attempts.load(std::sync::atomic::Ordering::SeqCst),
            3,
            "each death is followed by another worker"
        );
    }

    /// **The bound is for test pushes, which are the only thing that can queue
    /// up.** Doorbells replace each other, so they cannot accumulate; a test
    /// push is somebody's explicit request and is never coalesced away. If a
    /// device falls far enough behind that even those pile up, what goes is the
    /// *oldest* — and whoever asked for it is told, rather than left waiting on
    /// a channel with nothing coming.
    #[tokio::test]
    async fn a_backed_up_device_loses_its_stalest_test_push_and_says_so() {
        let queue = queue();
        let mut waiting = Vec::new();
        for n in 0..MAX_WAITING + 2 {
            let (respond, rx) = tokio::sync::oneshot::channel();
            waiting.push(rx);
            queue.push(
                Delivery {
                    target: target("phone"),
                    payload: format!("test-{n}"),
                    collapse: TEST_COLLAPSE_ID,
                    respond: Some(respond),
                },
                "phone",
            );
        }

        for (n, rx) in waiting.drain(..2).enumerate() {
            assert!(
                matches!(
                    within("the dropped test's answer", rx).await,
                    Ok(TestDelivery::Failed(_))
                ),
                "test-{n} was dropped, so it is answered rather than abandoned"
            );
        }
        let left = drained(&queue);
        assert_eq!(left.len(), MAX_WAITING, "the queue is bounded");
        assert_eq!(left.first().unwrap(), "test-2", "the two stalest are gone");
        assert_eq!(
            left.last().unwrap(),
            &format!("test-{}", MAX_WAITING + 1),
            "the newest is always kept"
        );
    }
}
