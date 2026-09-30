//! `iwdb-crash`: the kill -9 crash harness (see the library docs).
//!
//! ```text
//! iwdb-crash [--policy always|group|off|all] [--seed N] [--seeds K] [--cycles N]
//!            [--acts N] [--max-delay-ms N] [--work DIR] [--keep-work] [--progress N]
//! ```
//!
//! Runs `--cycles` kill/recover cycles for each policy and each of `--seeds`
//! consecutive seeds starting at `--seed` (random if not given; it is
//! printed). Exits with 1 at the first violated guarantee, printing its
//! policy, seed and cycle, and keeps the files. `iwdb-crash child ...` is
//! the child side, run by the parent.

use std::path::PathBuf;
use std::process::ExitCode;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use iwdb_crash::{run, ChildArgs, Config, Policy};

struct Options {
    policies: Vec<Policy>,
    seed: u64,
    seeds: u64,
    cycles: u64,
    acts: usize,
    max_delay: Duration,
    work: Option<PathBuf>,
    keep_work: bool,
    progress: u64,
}

fn parse(args: &[String]) -> Result<Options, String> {
    let random =
        SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_nanos() as u64) ^ u64::from(std::process::id());
    let mut options = Options {
        policies: Policy::ALL.to_vec(),
        seed: random % 1_000_000_000,
        seeds: 1,
        cycles: 100,
        acts: 120,
        max_delay: Duration::from_millis(60),
        work: None,
        keep_work: false,
        progress: 0,
    };
    let mut args = args.iter();
    while let Some(flag) = args.next() {
        if flag == "--keep-work" {
            options.keep_work = true;
            continue;
        }
        let value = args.next().ok_or_else(|| format!("{} needs a value", flag))?;
        let number = || value.parse::<u64>().map_err(|e| format!("{} {}: {}", flag, value, e));
        match flag.as_str() {
            "--policy" if value == "all" => options.policies = Policy::ALL.to_vec(),
            "--policy" => options.policies = vec![value.parse()?],
            "--seed" => options.seed = number()?,
            "--seeds" => options.seeds = number()?,
            "--cycles" => options.cycles = number()?,
            "--acts" => options.acts = number()? as usize,
            "--max-delay-ms" => options.max_delay = Duration::from_millis(number()?),
            "--work" => options.work = Some(value.into()),
            "--progress" => options.progress = number()?,
            other => return Err(format!("unknown option '{}' (see the doc comment in main.rs)", other)),
        }
    }
    Ok(options)
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some("child") {
        match ChildArgs::parse(&args[1..]) {
            Ok(child) => iwdb_crash::child::main(&child),
            Err(e) => {
                eprintln!("iwdb-crash child: {}", e);
                return ExitCode::from(2);
            }
        }
    }
    let options = match parse(&args) {
        Ok(options) => options,
        Err(e) => {
            eprintln!("iwdb-crash: {}", e);
            return ExitCode::from(2);
        }
    };
    let exe = match std::env::current_exe() {
        Ok(exe) => exe,
        Err(e) => {
            eprintln!("iwdb-crash: can't find its own binary: {}", e);
            return ExitCode::from(2);
        }
    };
    let root =
        options.work.clone().unwrap_or_else(|| std::env::temp_dir().join(format!("iwdb-crash-{}", std::process::id())));
    println!(
        "iwdb-crash: seed {} (seeds {}..{}), {} cycles per policy and seed, policies {:?}, work {}",
        options.seed,
        options.seed,
        options.seed + options.seeds,
        options.cycles,
        options.policies.iter().map(ToString::to_string).collect::<Vec<_>>(),
        root.display()
    );
    let mut total = 0;
    for seed in options.seed..options.seed + options.seeds {
        for &policy in &options.policies {
            let work = root.join(format!("{}-{}", policy, seed));
            let mut config = Config::new(exe.clone(), policy, seed, options.cycles, work.clone());
            config.acts = options.acts;
            config.max_delay = options.max_delay;
            config.progress = options.progress;
            match run(&config) {
                Ok(summary) => {
                    total += summary.cycles;
                    println!("{}", summary);
                    if !options.keep_work {
                        let _ = std::fs::remove_dir_all(&work);
                    }
                }
                Err(failure) => {
                    println!("{}", failure);
                    eprintln!("{}", failure);
                    return ExitCode::from(1);
                }
            }
        }
    }
    if !options.keep_work {
        let _ = std::fs::remove_dir(&root);
    }
    println!("iwdb-crash: {} cycles passed", total);
    ExitCode::SUCCESS
}
