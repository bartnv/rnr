#![allow(dead_code, unused_imports, unused_variables, unused_mut, unreachable_patterns)] // Please be quiet, I'm coding
use std::sync;
use tokio::sync::{ broadcast, mpsc };
use crate::{ Config, Job, Schedule, duration_from };

pub async fn run(config: sync::Arc<sync::RwLock<Config>>, mut runner: mpsc::Receiver<Box<Job>>, mut spawner: mpsc::Sender<Box<Job>>, mut websockets: broadcast::Sender<Job>) {
    while let Some(update) = runner.recv().await {
        if update.running {
            if let Err(e) = websockets.send(*update) {
                eprintln!("Broadcast error: {}", e);
            }
            continue;
        }
        let mut job = None;
        let mut doafter = vec![];
        {
            let mut wconfig = config.write().unwrap();
            for ajob in wconfig.jobs.values_mut() {
                if let Schedule::After(after) = &ajob.schedule {
                    if after.contains(&update.path.display().to_string()) { doafter.push(Box::new(ajob.clone_empty())); }
                }
                if ajob.path == update.path { job = Some(ajob); }
            };
            let job = match job {
                Some(job) => job,
                None => { eprintln!("Job with path {} not found", update.path.display()); continue; }
            };
            job.running = false;

            if let Some(e) = update.error {
                eprintln!("Job \"{}\" permanent error: {}", update.name, e);
                job.error = Some(e);
            }
            if let Some(run) = update.lastrun {
                match run.output.status.success() {
                    true => println!("Job \"{}\" ran successfully in {}", update.name, duration_from(run.duration.as_secs())),
                    false => println!("Job \"{}\" failed after {} with error code {}", update.name, duration_from(run.duration.as_secs()), run.output.status.code().map_or("(unknown)".to_string(), |c| c.to_string()))
                };
                job.laststart = update.laststart.clone();
                job.lastrun = Some(run);
            }
            if let Err(e) = websockets.send(job.clone()) {
                eprintln!("Broadcast error: {}", e);
            }
        }
        for mut job in doafter {
            job.laststart = Some(chrono::Local::now());
            spawner.send(job).await.unwrap();
        }
    }
}
