use std::{ path, process, sync, time };
use tokio::{ io::AsyncWriteExt as _, sync::{ broadcast, mpsc } };

use crate::{ control, Config, Job, Run, Schedule, duration_from };

pub async fn run(config: sync::Arc<sync::RwLock<Config>>, broadcast: broadcast::Sender<Job>) {
    let mut nextjobs: Vec<Box<Job>> = vec![];
    let (ctrltx, ctrlrx) = mpsc::channel(100);
    let (spawntx, mut spawnrx) = mpsc::channel(100);
    let aconfig = config.clone();
    let aspawntx = spawntx.clone();
    let ctrl = tokio::spawn(async move {
        control::run(aconfig.clone(), ctrlrx, aspawntx, broadcast).await;
    });
    let aconfig = config.clone();
    let spawn = tokio::spawn(async move {
        while let Some(mut job) = spawnrx.recv().await {
            let ctrltx = ctrltx.clone();
            let config = aconfig.clone();
            tokio::spawn(async move {
                let name = job.name.clone();
                if let Some(cjob) = config.write().unwrap().jobs.get_mut(&job.path.display().to_string()) {
                    if cjob.running {
                        eprintln!("Skipping run of job {} because it is already running", cjob.path.display());
                        return;
                    }
                    cjob.running = true;
                    cjob.laststart = job.laststart.clone();
                }
                job.running = true;
                if let Err(e) = ctrltx.send(job.clone()).await {
                    eprintln!("Send error: {}", e);
                }
                let mut job = run_job(config, job).await;
                job.running = false;
                if let Err(e) = ctrltx.send(job).await {
                    eprintln!("Send error: {}", e);
                }
            });
        }
    });

    loop {
        for job in nextjobs.drain(..) {
            spawntx.send(job).await.unwrap();
        }
        tokio::task::yield_now().await;

        let mut nextloop = chrono::Local::now() + chrono::TimeDelta::new(300, 0).unwrap();
        {
            let rconfig = config.read().unwrap();
            for job in rconfig.jobs.values() {
                if let Schedule::Schedule(sched) = &job.schedule {
                    let nextrun = sched.upcoming(chrono::Local).next().unwrap();
                    if nextrun > nextloop { continue; }
                    if nextrun < nextloop {
                        nextjobs.clear();
                        nextloop = nextrun.clone();
                    }
                    let mut job = Box::new(job.clone_empty());
                    job.laststart = Some(nextrun);
                    nextjobs.push(job);
                }
            }
        }
        let wait = nextloop - chrono::Local::now();
        // println!("Next loop in {} ({} jobs to run)", duration_from(wait.num_seconds().try_into().unwrap()), nextjobs.len());
        tokio::time::sleep(wait.to_std().unwrap()).await;
    }
}

async fn run_job(config: sync::Arc<sync::RwLock<Config>>, mut job: Box<Job>) -> Box<Job> {
    println!("{} Running {}", chrono::Local::now(), job.name);
    let mut cmd = tokio::process::Command::new(&job.command[0]);
    cmd.args(&job.command[1..]);
    if let Ok(config) = config.read() {
        cmd.envs(&config.env);
    }
    let start = time::Instant::now();
    let output = match cmd.output().await {
        Ok(output) => output,
        Err(e) => { job.error = Some(format!("Failed to start: {}", e)); return job; }
    };
    job.lastrun = Some(Run { start: job.laststart.as_ref().unwrap().clone(), duration: start.elapsed(), output });
    let output = &job.lastrun.as_ref().unwrap().output;
    let mut filename = job.path.clone();
    filename.push("runs");
    filename.push(job.laststart.unwrap().format("%Y-%m-%d %H:%M").to_string());
    if tokio::fs::create_dir(&filename).await.is_err() { return job; }
    filename.push("status");
    match tokio::fs::File::create(&filename).await {
        Ok(mut file) => file.write_all(output.status.code().map_or("unknown\n".to_string(), |c| c.to_string() + "\n").as_bytes()).await.unwrap(),
        Err(e) => { job.error = Some(format!("Failed to write status file: {}", e)); return job; }
    };
    if output.stdout.len() > 0 {
        filename.pop();
        filename.push("out");
        if let Ok(mut file) = tokio::fs::File::create(&filename).await {
            file.write_all(&output.stdout).await.unwrap();
        };
    }
    if output.stderr.len() > 0 {
        filename.pop();
        filename.push("err");
        if let Ok(mut file) = tokio::fs::File::create(&filename).await {
            file.write_all(&output.stderr).await.unwrap();
        };
    }
    job
}
