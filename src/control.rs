#![allow(dead_code, unused_imports, unused_variables, unused_mut, unreachable_patterns)] // Please be quiet, I'm coding
use std::{path::PathBuf, sync};
use tokio::{fs::remove_dir_all, sync::{ broadcast, mpsc }};
use crate::{ Config, Job, Schedule, duration_from };

pub async fn run(config: sync::Arc<sync::RwLock<Config>>, mut runner: mpsc::Receiver<Box<Job>>, mut spawner: mpsc::Sender<Box<Job>>, mut websockets: broadcast::Sender<Job>) {
    while let Some(update) = runner.recv().await {
        if update.running {
            let _ = websockets.send(*update);
            continue;
        }
        let mut job = None;
        let mut doafter = vec![];
        let mut failed = false;
        {
            let mut wconfig = config.write().unwrap();
            let dir = wconfig.dir.clone();
            for ajob in wconfig.jobs.values_mut() {
                if let Schedule::After(after) = &ajob.schedule && after.contains(&update.path.display().to_string()) {
                    doafter.push(Box::new(ajob.clone_empty()));
                }
                if ajob.path == update.path { job = Some(ajob); }
            };
            let job = match job {
                Some(job) => job,
                None => { eprintln!("Job \"{}\" not found", update.path.display()); continue; }
            };
            job.running = false;
            job.history = update.history;

            if let Some(e) = update.error {
                eprintln!("Job \"{}\" permanent error: {}", update.name, e);
                job.error = Some(e);
                failed = true;
            }
            if let Some(run) = update.lastrun {
                match run.status.unwrap().success() {
                    true => println!("{} [{}] finished {} in {}",
                                chrono::Local::now().format("%Y-%m-%d %H:%M:%S"),
                                update.path.display(),
                                match run.stderr.is_empty() { true => "successfully", false => "with errors" },
                                duration_from(run.duration.unwrap().as_secs())
                            ),
                    false => {
                        failed = true;
                        println!("{} [{}] failed after {} with {}", chrono::Local::now().format("%Y-%m-%d %H:%M:%S"), update.path.display(), duration_from(run.duration.unwrap().as_secs()), run.status.unwrap());
                    }
                };
                job.laststart = update.laststart;
                job.lastrun = Some(run);
            }
            let _ = websockets.send(job.clone());
            if job.history {
                let path = dir.join(&job.path);
                let limit = wconfig.prune;
                tokio::spawn(async move {
                    prune(path, limit).await;
                });
            }
        }
        for mut job in doafter {
            if failed {
                if let Some(job) = config.write().unwrap().jobs.get_mut(&job.path.display().to_string()) {
                    job.skipped += 1;
                    let _ = websockets.send(job.clone());
                }
                eprintln!("Job \"{}\" skipped because job \"{}\" failed", job.path.display(), update.path.display());
                continue;
            }
            job.laststart = Some(chrono::Local::now());
            spawner.send(job).await.unwrap();
        }
    }
}

async fn prune(mut path: PathBuf, limit: usize) {
    path.push("runs");
    if let Ok(mut dir) = tokio::fs::read_dir(&path).await {
        let mut subdirs = vec![];
        while let Ok(Some(entry)) = dir.next_entry().await {
            if let Ok(ftype) = entry.file_type().await && ftype.is_dir() {
                subdirs.push(entry.file_name());
            }
        };
        if subdirs.len() <= limit { return; }
        subdirs.sort_unstable_by(|a, b| b.cmp(a)); // Reverse sort alphabetically
        while subdirs.len() > limit {
            if let Some(dir) = subdirs.pop() && let Err(e) = remove_dir_all(path.join(&dir)).await {
                eprintln!("Failed to remove runs subdir \"{}\": {}", path.join(&dir).display(), e);
            }
        };
    }
}
