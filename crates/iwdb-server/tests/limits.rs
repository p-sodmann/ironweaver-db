//! Resource limits per client and namespace (step 15d, ADR 0059), through
//! a server with authentication on.
//!
//! **The stress test** (the step's first criterion): one user floods the
//! server with expensive reads (PageRanks over the namespace), far over its
//! limits, from four connections, retrying every refusal at once; another
//! user makes cheap reads (one node) within its limits, every 20 ms. The
//! server has two query workers, so without limits the calm user's reads
//! queue behind the flood's. With them, the flood holds at most one worker
//! (`max_running` 1, 20 calls a second), and:
//!
//! - **none of the calm user's calls fails**;
//! - **each takes less than [`CALM_BOUND`]** (150 ms), from call to answer
//!   over gRPC, under the flood;
//! - the flood is admitted no more than its bucket allows, and its
//!   refusals are counted per user in the metrics.
//!
//! Measured on the development machine (debug build, the CI size): limited,
//! the calm user's slowest call took about 1 ms; without limits (a control
//! run, not part of the CI test, since its times depend on the machine) its
//! median was 434 ms and its slowest 811 ms. The bound sits between the two
//! with room on both sides, so the test fails if the flood can make the
//! calm user wait behind it.
//!
//! The CI run lasts [`CI_SECONDS`]; `stress_measured` (ignored) runs longer
//! on a larger graph, with the control run, and prints the latencies:
//!
//! ```text
//! cargo test --release -p iwdb-server --test limits -- --ignored --nocapture
//! ```
//!
//! The other tests: query budgets clamp reads and say so, a namespace's
//! concurrency limit counts every user, cancelling isn't limited, a watch
//! takes one token, the defaults skip server admins, and REST refuses with
//! `resource_exhausted` too.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use ironweaver_core::Expr;
use ironweaver_core::algo::PageRank;
use iwdb::{Embedded, QueryConfig, Store};
use iwdb_engine::Mutation;
use iwdb_query::exec::block_on;
use iwdb_query::metrics::{LIMIT_REFUSALS, NAMESPACE_LIMIT_REFUSALS};
use iwdb_query::{
    Accounts, Admin, AnalyticsRequest, ChangesRequest, ClientLimits, Code, Database, FindRequest, Job, MatchRequest,
    NamespaceLimits, ProjectionSpec, QueryOptions, Secret,
};
use iwdb_server::client::Remote;
use support::{FAST, Running, auth_settings, options};

mod support;

const NS: &str = "t";
/// The iterations of each of the flood's PageRanks: enough that, without
/// limits, the calm user's reads wait behind them well past [`CALM_BOUND`]
/// (a PageRank takes about 200 ms in a debug build at the CI size).
const FLOOD_ITERATIONS: usize = 200;
/// How long the CI run of the stress test floods.
const CI_SECONDS: u64 = 3;
/// What each of the calm user's calls must take less than, under the flood:
/// about 150 times what they took, and a third of what they took without
/// limits (see the module docs).
const CALM_BOUND: Duration = Duration::from_millis(150);
/// The flood's limits: 20 calls a second after a burst of 5, one at a time.
const FLOOD: ClientLimits = ClientLimits {
    requests_per_second: Some(20),
    burst: Some(5),
    max_running: Some(1),
    max_results: None,
    max_visited: None,
    timeout_ms: None,
};
/// The defaults (`[limits.client]`), which the calm user stays within.
const DEFAULTS: ClientLimits = ClientLimits {
    requests_per_second: Some(200),
    burst: Some(50),
    max_running: Some(4),
    max_results: None,
    max_visited: None,
    timeout_ms: None,
};

/// A server with a namespace of `nodes` nodes, two query workers, the
/// [`DEFAULTS`] if `limited`, and the users `root` (admin), `flood` (with
/// [`FLOOD`] if `limited`) and `calm`; and each one's client.
struct World {
    server: Running<Embedded>,
    _dir: tempfile::TempDir,
    root: Remote,
    flood: Secret,
    calm: Secret,
}

impl World {
    fn new(nodes: usize, limited: bool) -> World {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path(), options()).unwrap();
        store.create_namespace(NS, None).unwrap();
        let ns = store.namespace(NS).unwrap();
        for chunk in (0..nodes).collect::<Vec<_>>().chunks(5_000) {
            let batch: Vec<Mutation> = chunk.iter().map(|i| node(&format!("n{}", i))).collect();
            ns.commit(&batch).unwrap();
        }
        // Two edges a node, for the flood's PageRank
        for chunk in (0..nodes).collect::<Vec<_>>().chunks(5_000) {
            let edge = |from: usize, to: usize| Mutation::AddEdge {
                from: format!("n{}", from),
                to: format!("n{}", to % nodes),
                ty: None,
                attr: Default::default(),
                meta: Default::default(),
            };
            let batch: Vec<Mutation> = chunk.iter().flat_map(|&i| [edge(i, i + 1), edge(i, i * 7 + 3)]).collect();
            ns.commit(&batch).unwrap();
        }
        let users = store.users().with_params(FAST);
        let mut tokens = Vec::new();
        for (name, admin) in [("root", true), ("flood", false), ("calm", false)] {
            users.create(name, &Secret::new(format!("{}-password", name)), admin).unwrap();
            if !admin {
                users.grant(name, NS, iwdb::Role::Write).unwrap();
            }
            tokens.push(users.create_token(name, "t", None).unwrap().token);
        }
        if limited {
            users.set_limits("flood", FLOOD).unwrap();
        }
        let client_limits = if limited { DEFAULTS } else { ClientLimits::default() };
        let config = QueryConfig { workers: 2, queue: 64, client_limits, ..QueryConfig::default() };
        let db = Embedded::new(store, config).unwrap().with_auth(auth_settings());
        let server = Running::start_auth(db);
        let root = server.client().with_token(tokens[0].clone());
        World { root, flood: tokens[1].clone(), calm: tokens[2].clone(), server, _dir: dir }
    }

    fn client(&self, token: &Secret) -> Remote {
        self.server.client().with_token(token.clone())
    }
}

fn node(id: &str) -> Mutation {
    Mutation::UpsertNode {
        id: id.into(),
        labels: vec!["P".into()],
        attr: [("n".to_owned(), ironweaver_core::Value::Int(1))].into(),
        meta: Default::default(),
        expected_version: None,
    }
}

/// What a stress run saw.
#[derive(Debug, Default)]
struct Report {
    /// The calm user's latencies, sorted.
    calm: Vec<Duration>,
    calm_failures: Vec<String>,
    flood_admitted: u64,
    flood_refused: u64,
    flood_failed: u64,
    seconds: f64,
}

impl Report {
    fn percentile(&self, p: f64) -> Duration {
        let i = ((self.calm.len() as f64 * p).ceil() as usize).saturating_sub(1).min(self.calm.len() - 1);
        self.calm[i]
    }

    fn print(&self, what: &str) {
        println!(
            "{}: calm {} calls, p50 {:?}, p99 {:?}, max {:?}, {} failed; flood {} admitted, {} refused, {} other errors in {:.1} s",
            what,
            self.calm.len(),
            self.percentile(0.5),
            self.percentile(0.99),
            self.calm.last().unwrap(),
            self.calm_failures.len(),
            self.flood_admitted,
            self.flood_refused,
            self.flood_failed,
            self.seconds
        );
    }
}

/// The flood's expensive read: a PageRank over the whole namespace, of
/// [`FLOOD_ITERATIONS`] iterations.
fn pagerank() -> AnalyticsRequest {
    let job = Job::PageRank(PageRank { tol: 0.0, max_iter: FLOOD_ITERATIONS, ..PageRank::default() });
    AnalyticsRequest { projection: ProjectionSpec::default(), job }
}

/// Flood with 4 connections of `flood` (PageRanks, each refusal retried at
/// once) for `seconds`, while `calm` reads one node every 20 ms.
fn stress(world: &World, seconds: u64) -> Report {
    let stop = Arc::new(AtomicBool::new(false));
    let (admitted, refused, failed) =
        (Arc::new(AtomicU64::new(0)), Arc::new(AtomicU64::new(0)), Arc::new(AtomicU64::new(0)));
    let flooders: Vec<_> = (0..4)
        .map(|_| {
            let client = world.client(&world.flood);
            let (stop, admitted, refused, failed) = (stop.clone(), admitted.clone(), refused.clone(), failed.clone());
            std::thread::spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    match block_on(client.analyze(NS, pagerank(), QueryOptions::default())) {
                        Ok(_) => admitted.fetch_add(1, Ordering::Relaxed),
                        Err(e) if e.code() == Code::ResourceExhausted => refused.fetch_add(1, Ordering::Relaxed),
                        Err(_) => failed.fetch_add(1, Ordering::Relaxed),
                    };
                }
            })
        })
        .collect();
    // Let the flood fill whatever it can first
    std::thread::sleep(Duration::from_millis(200));
    let calm = world.client(&world.calm);
    let mut report = Report::default();
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(seconds) {
        let t = Instant::now();
        match block_on(calm.get_nodes(NS, vec!["n1".into()], QueryOptions::default())) {
            Ok(answer) => assert!(answer.value[0].is_some()),
            Err(e) => report.calm_failures.push(format!("{}: {}", e.code(), e)),
        }
        report.calm.push(t.elapsed());
        std::thread::sleep(Duration::from_millis(20));
    }
    report.seconds = start.elapsed().as_secs_f64() + 0.2;
    stop.store(true, Ordering::Relaxed);
    for f in flooders {
        f.join().unwrap();
    }
    report.calm.sort();
    report.flood_admitted = admitted.load(Ordering::Relaxed);
    report.flood_refused = refused.load(Ordering::Relaxed);
    report.flood_failed = failed.load(Ordering::Relaxed);
    report
}

/// Check a limited run: see the module docs.
fn check(world: &World, report: &Report) {
    assert!(report.calm_failures.is_empty(), "the calm user's calls failed: {:?}", report.calm_failures);
    let max = *report.calm.last().unwrap();
    assert!(max < CALM_BOUND, "the calm user waited {:?} (bound {:?}): {:?}", max, CALM_BOUND, report);
    assert_eq!(report.flood_failed, 0, "{:?}", report);
    assert!(report.flood_refused > 0, "the flood was never refused: {:?}", report);
    // The bucket: its burst plus its rate over the run (and a token of slack)
    let allowed = 5.0 + 20.0 * (report.seconds + 0.5) + 1.0;
    assert!(
        (report.flood_admitted as f64) <= allowed,
        "admitted {} > {}: {:?}",
        report.flood_admitted,
        allowed,
        report
    );

    // Counted per user, and a user sees only its own series
    let metrics = block_on(world.root.metrics()).unwrap();
    let samples = &metrics.family(LIMIT_REFUSALS).unwrap().samples;
    let flood: u64 = samples
        .iter()
        .filter(|s| s.label("user") == Some("flood"))
        .map(|s| match s.value {
            iwdb_query::metrics::Value::Counter(n) => n,
            _ => 0,
        })
        .sum();
    assert_eq!(flood, report.flood_refused, "{:?}", samples);
    assert!(samples.iter().all(|s| s.label("user") == Some("flood")), "only the flood was refused: {:?}", samples);
    let calm = world.client(&world.calm);
    assert!(block_on(calm.metrics()).unwrap().family(LIMIT_REFUSALS).unwrap().samples.is_empty());
}

/// Step 15d's criterion (see the module docs).
#[test]
fn one_client_over_its_limits_cannot_starve_another() {
    let world = World::new(20_000, true);
    let report = stress(&world, CI_SECONDS);
    report.print("limited");
    check(&world, &report);
}

/// The stress test at full length, and a control run without limits, whose
/// calm calls wait behind the flood. Prints both; asserts the limited one.
#[test]
#[ignore = "measurement; run with --release --ignored --nocapture"]
fn stress_measured() {
    let nodes = 100_000;
    let world = World::new(nodes, false);
    let control = stress(&world, 30);
    control.print("without limits");
    drop(world);
    let world = World::new(nodes, true);
    let report = stress(&world, 30);
    report.print("limited");
    check(&world, &report);
}

/// A query budget lowers what a read asks for, as a server cap does; a
/// read that reaches it says that the budget applies.
#[test]
fn budgets_clamp_reads_and_say_so() {
    let world = World::new(200, false);
    let results = ClientLimits { max_results: Some(5), ..ClientLimits::default() };
    block_on(world.root.set_user_limits("calm", results)).unwrap();
    let calm = world.client(&world.calm);
    let all = FindRequest { filter: Expr::Label("P".into()) };
    // Asked for 100 results: 5, and a cursor
    let page = block_on(calm.find(NS, all, QueryOptions::default().with_limits(Some(100), None, None))).unwrap();
    assert_eq!(page.value.len(), 5);
    assert!(page.next.is_some());
    // A pattern over more nodes than the budget: refused, saying why
    let budget = ClientLimits { max_visited: Some(50), ..results };
    block_on(world.root.set_user_limits("calm", budget)).unwrap();
    let pattern = MatchRequest::parse("(x:P)").unwrap();
    let asked = QueryOptions::default().with_limits(None, Some(1_000_000), None);
    let e = block_on(calm.match_pattern(NS, pattern.clone(), asked)).unwrap_err();
    assert_eq!(e.code(), Code::BudgetExceeded);
    assert!(e.message().contains("user 'calm' has a query budget: max_results 5, max_visited 50"), "{}", e);
    // ... or truncated, if asked for a partial answer
    let partial = QueryOptions { partial: true, ..QueryOptions::default() };
    assert!(block_on(calm.match_pattern(NS, pattern.clone(), partial)).unwrap().truncated);
    // Others aren't affected, and the user sees its budget
    assert!(block_on(world.root.match_pattern(NS, pattern, QueryOptions::default())).is_ok());
    let (me, _) = block_on(calm.whoami()).unwrap();
    assert_eq!(me.limits, budget);
}

/// A namespace's concurrency limit counts every user's calls on it; a
/// refused call holds nothing, and cancelling is never limited.
#[test]
fn a_namespace_concurrency_limit_counts_every_user() {
    let world = World::new(10, false);
    block_on(world.root.set_namespace_limits(NS, NamespaceLimits { max_running: Some(1), ..Default::default() }))
        .unwrap();
    // The flood user's long poll holds the namespace's one slot
    let flood = world.client(&world.flood);
    let poll = std::thread::spawn(move || {
        let options = QueryOptions { timeout: Some(Duration::from_secs(30)), ..QueryOptions::default() };
        block_on(flood.changes(NS, ChangesRequest { from_seq: 1 << 40, wait: true }, options))
    });
    let start = Instant::now();
    let id = loop {
        let list = block_on(world.root.active_requests(Some("flood".into()), None)).unwrap();
        if let Some(r) = list.items.iter().find(|r| r.operation == iwdb_query::Operation::Changes) {
            break r.id;
        }
        assert!(start.elapsed() < Duration::from_secs(10), "the long poll never ran");
        std::thread::sleep(Duration::from_millis(5));
    };
    let calm = world.client(&world.calm);
    let e = block_on(calm.get_nodes(NS, vec!["n1".into()], QueryOptions::default())).unwrap_err();
    assert_eq!(e.code(), Code::ResourceExhausted);
    assert!(e.message().contains("namespace 't' has 1 requests running"), "{}", e);
    // Another namespace is free; and the poll's owner may cancel it
    assert!(block_on(calm.namespaces()).is_ok());
    block_on(world.client(&world.flood).cancel_request(id, None)).unwrap();
    assert_eq!(poll.join().unwrap().unwrap_err().code(), Code::Cancelled);
    block_on(calm.get_nodes(NS, vec!["n1".into()], QueryOptions::default())).unwrap();
    let metrics = block_on(world.root.metrics()).unwrap();
    let refusals = &metrics.family(NAMESPACE_LIMIT_REFUSALS).unwrap().samples;
    assert_eq!(refusals.len(), 1, "{:?}", refusals);
    assert_eq!((refusals[0].label("namespace"), refusals[0].label("limit")), (Some(NS), Some("running")));
    let report = block_on(world.root.limits()).unwrap();
    assert_eq!(report.namespaces[0].limits.max_running, Some(1));
}

/// A server admin gets no default limits; a user's 0 lifts a default.
#[test]
fn defaults_apply_to_users_who_are_not_admins() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path(), options()).unwrap();
    let users = store.users().with_params(FAST);
    let mut tokens = Vec::new();
    for (name, admin) in [("root", true), ("ann", false), ("bob", false)] {
        users.create(name, &Secret::new(format!("{}-password", name)), admin).unwrap();
        tokens.push(users.create_token(name, "t", None).unwrap().token);
    }
    users.set_limits("bob", ClientLimits { max_running: Some(0), ..ClientLimits::default() }).unwrap();
    let defaults = ClientLimits { max_running: Some(2), timeout_ms: Some(1000), ..ClientLimits::default() };
    let config = QueryConfig { client_limits: defaults, ..QueryConfig::default() };
    let server = Running::start_auth(Embedded::new(store, config).unwrap().with_auth(auth_settings()));
    let me = |i: usize| block_on(server.client().with_token(tokens[i].clone()).whoami()).unwrap().0.limits;
    assert_eq!(me(0), ClientLimits::default(), "an admin gets only its own");
    assert_eq!(me(1), defaults);
    assert_eq!(me(2), ClientLimits { max_running: Some(0), timeout_ms: Some(1000), ..ClientLimits::default() });
    let report = block_on(server.client().with_token(tokens[1].clone()).limits()).unwrap();
    assert_eq!(report.client_defaults, defaults);
}

/// A watch takes one token for the stream, not one per round.
#[test]
fn a_watch_takes_one_token() {
    let world = World::new(10, false);
    let one = ClientLimits { requests_per_second: Some(1), burst: Some(1), ..ClientLimits::default() };
    block_on(world.root.set_user_limits("calm", one)).unwrap();
    let (token, endpoint) = (world.calm.clone(), world.server.endpoint());
    // Another user commits (flood has no limits in this world)
    let writer = Arc::new(world.client(&world.flood));
    world.server.block_on(async move {
        use iwdb_server::proto as pb;
        let channel = tonic::transport::Endpoint::from_shared(endpoint).unwrap().connect().await.unwrap();
        let mut client = pb::database_service_client::DatabaseServiceClient::new(channel);
        let mut request = tonic::Request::new(pb::WatchRequest { namespace: NS.into(), from_seq: 1, options: None });
        request.metadata_mut().insert("authorization", format!("Bearer {}", token.expose()).parse().unwrap());
        let mut stream = client.watch(request).await.unwrap().into_inner();
        let mut seen = stream.message().await.unwrap().unwrap().next_seq;
        // Five more commits, each a round of the watch: none is refused
        for i in 0..5 {
            let committed = tokio::task::spawn_blocking({
                let writer = writer.clone();
                move || block_on(writer.commit(NS, vec![node(&format!("w{}", i))], Default::default())).unwrap()
            })
            .await
            .unwrap();
            while seen <= committed.seq {
                let batch = stream.message().await.unwrap().expect("the stream goes on");
                seen = batch.next_seq;
            }
        }
    });
    // A call of its own takes the bucket's token, which the stream left
    let calm = world.client(&world.calm);
    std::thread::sleep(Duration::from_millis(1100));
    block_on(calm.namespaces()).unwrap();
    let e = block_on(calm.namespaces()).unwrap_err();
    assert_eq!(e.code(), Code::ResourceExhausted);
    assert!(e.message().contains("rate limit of 1 requests per second"), "{}", e);
}

/// Over REST, a refusal is `resource_exhausted` too (HTTP 503).
#[cfg(feature = "rest")]
#[test]
fn rest_refuses_with_resource_exhausted() {
    let world = World::new(10, true);
    let flood = world.server.rest_client();
    flood.set_token(Some(world.flood.clone()));
    let refused = (0..20).find_map(|_| block_on(flood.namespaces()).err()).expect("refused within its burst");
    assert_eq!(refused.code(), Code::ResourceExhausted);
    assert!(refused.message().contains("user 'flood' is over its rate limit"), "{}", refused);
}
