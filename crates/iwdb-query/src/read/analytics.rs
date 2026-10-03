//! Analytics jobs on a projection (ADR 0022).

use ironweaver_core::{algo, GraphError, Projection};

use crate::{Job, JobResult};

/// Run `job` on `projection` with the core's algorithms, and rank the
/// result: at most `max_results` rows (scores and counts highest first,
/// groups biggest first; ties by id). Also returns whether rows were cut.
///
/// Runs under the caller's cancel token: the core's algorithms check it.
pub fn run_job(job: &Job, projection: &Projection, max_results: usize) -> Result<(JobResult, bool), GraphError> {
    let p = projection;
    let id = |u: usize| p.id(u as u32).to_owned();
    let scores = |values: Vec<f64>| {
        let mut rows: Vec<(String, f64)> = values.into_iter().enumerate().map(|(u, s)| (id(u), s)).collect();
        rows.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        JobResult::Scores(rows)
    };
    let counts = |values: Vec<u64>| {
        let mut rows: Vec<(String, u64)> = values.into_iter().enumerate().map(|(u, c)| (id(u), c)).collect();
        rows.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        JobResult::Counts(rows)
    };
    let groups = |found: Vec<Vec<u32>>| {
        let mut rows: Vec<Vec<String>> = found
            .into_iter()
            .map(|g| {
                let mut ids: Vec<String> = g.into_iter().map(|u| p.id(u).to_owned()).collect();
                ids.sort_unstable();
                ids
            })
            .collect();
        rows.sort_by(|a, b| b.len().cmp(&a.len()).then_with(|| a.first().cmp(&b.first())));
        JobResult::Groups(rows)
    };
    let result = match job {
        Job::PageRank(options) => {
            if options.personalization.is_some() {
                return Err(GraphError::InvalidArgument(
                    "PageRank's personalization is by dense index and can't be given to a database job".into(),
                ));
            }
            scores(algo::pagerank(p, options)?)
        }
        Job::Degree { incoming } => scores(algo::degree_centrality(p, *incoming)),
        Job::WeaklyConnectedComponents => groups(algo::weakly_connected_components(p)),
        Job::StronglyConnectedComponents => groups(algo::strongly_connected_components(p)),
        Job::Leiden(options) => groups(algo::leiden(p, options)?),
        Job::LabelPropagation { max_iter } => groups(algo::label_propagation(p, *max_iter).0),
        Job::CoreNumber => counts(algo::core_number(p).into_iter().map(u64::from).collect()),
        Job::Triangles => counts(algo::triangles(p)),
    };
    Ok(rank(result, max_results))
}

fn rank(result: JobResult, max: usize) -> (JobResult, bool) {
    match result {
        JobResult::Scores(mut rows) => {
            let cut = rows.len() > max;
            rows.truncate(max);
            (JobResult::Scores(rows), cut)
        }
        JobResult::Counts(mut rows) => {
            let cut = rows.len() > max;
            rows.truncate(max);
            (JobResult::Counts(rows), cut)
        }
        JobResult::Groups(mut rows) => {
            let cut = rows.len() > max;
            rows.truncate(max);
            (JobResult::Groups(rows), cut)
        }
    }
}
