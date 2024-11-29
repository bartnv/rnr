#![allow(dead_code, unused_imports, unused_variables, unused_mut, unreachable_patterns)] // Please be quiet, I'm coding
use std::sync;
use tokio::sync::mpsc;
use crate::{ Config, Job };

pub async fn run(config: sync::Arc<sync::RwLock<Config>>, mut runner: mpsc::Receiver<Box<Job>>) {
    while let Some(update) = runner.recv().await {
        let mut wconfig = config.write().unwrap();
        let job = match wconfig.jobs.iter_mut().find(|j| j.path == update.path) {
            Some(job) => job,
            None => { eprintln!("Job with path {} not found", update.path.display()); continue; }
        };

        if let Some(e) = update.error {
            eprintln!("Job {} permanent error: {}", update.name, e);
            job.error = Some(e);
        }
        if let Some(run) = update.lastrun {
            match run.output.status.success() {
                true => println!("Job {} finished successfully", update.name),
                false => println!("Job {} finished with error code {}", update.name, run.output.status.code().map_or("unknown".to_string(), |c| c.to_string()))
            };
            job.lastrun = Some(run);
        }
    }
}
