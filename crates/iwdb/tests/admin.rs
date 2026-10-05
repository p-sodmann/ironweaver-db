//! The operator's reads on the embedded store (step 16c, ADR 0051): they
//! answer while every worker is busy, and a cancel frees the worker.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;
use std::time::{Duration, Instant};

use iwdb::{Embedded, QueryConfig, Store};
use iwdb_query::audit::Audit;
use iwdb_query::auth::Operation;
use iwdb_query::exec::block_on;
use iwdb_query::{Admin, Authorized, Code, Database, Principal, QueryOptions};

mod support;

#[test]
fn the_operators_reads_answer_while_every_worker_is_busy() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path(), support::options(2)).unwrap();
    let config = QueryConfig { workers: 1, queue: 4, ..QueryConfig::default() };
    let db = Arc::new(Embedded::new(store, config).unwrap());
    let admin = Authorized::new(db, Arc::new(Principal::unauthenticated()), Audit::none());
    std::thread::scope(|s| {
        // The only worker waits for a seq no commit reaches
        let busy = s.spawn(|| {
            let options = QueryOptions { timeout: Some(Duration::from_secs(60)), ..QueryOptions::default() };
            block_on(admin.wait_for_seq("default", 1 << 40, options))
        });
        let start = Instant::now();
        let id = loop {
            let list = block_on(admin.active_requests(None, None)).unwrap();
            if let Some(r) = list.items.iter().find(|r| r.operation == Operation::WaitForSeq) {
                break r.id;
            }
            assert!(start.elapsed() < Duration::from_secs(10), "never listed");
            std::thread::sleep(Duration::from_millis(5));
        };
        // Give the worker time to pick it up: it is busy from here on
        std::thread::sleep(Duration::from_millis(50));
        let answered = Instant::now();
        block_on(admin.metrics()).unwrap();
        block_on(admin.consumers()).unwrap();
        block_on(admin.log(0, None)).unwrap();
        assert!(answered.elapsed() < Duration::from_secs(5), "the reads waited for the worker");
        block_on(admin.cancel_request(id, None)).unwrap();
        assert_eq!(busy.join().unwrap().unwrap_err().code(), Code::Cancelled);
    });
    // The worker is free again
    let status = block_on(admin.server_status()).unwrap();
    assert_eq!(status.requests.cancelled, 1);
    assert!(status.ready);
}
