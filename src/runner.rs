use std::{ path, process, sync };
use tokio::io::AsyncWriteExt as _;

use crate::{ Config, Job, Schedule };

pub async fn run(config: sync::Arc<sync::RwLock<Config>>) {
    let mut nextjobs: Vec<Job> = vec![];
    loop {
        for job in nextjobs.drain(..) {
            tokio::spawn(async move {
                let name = job.name.clone();
                let job = run_job(job).await;
                match job.lastresult.as_ref().unwrap().status.success() {
                    true => println!("Job {} finished successfully", name),
                    false => println!("Job {} finished with error code {}", name, job.lastresult.unwrap().status.code().map_or("unknown".to_string(), |c| c.to_string()))
                };
            });
        }
        tokio::task::yield_now().await;

        let mut nextloop = chrono::Local::now() + chrono::TimeDelta::new(60, 0).unwrap();
        {
            let rconfig = config.read().unwrap();
            for job in &rconfig.jobs {
                if let Schedule::Schedule(sched) = &job.schedule {
                    let nextrun = sched.upcoming(chrono::Local).next().unwrap();
                    if nextrun > nextloop { continue; }
                    if nextrun < nextloop {
                        nextjobs.clear();
                        nextloop = nextrun.clone();
                    }
                    let mut job = job.clone();
                    job.lastrun = Some(nextrun);
                    nextjobs.push(job);
                }
            }
        }
        let wait = nextloop - chrono::Local::now();
        println!("Next loop in {} seconds ({} jobs to run)", wait.num_seconds(), nextjobs.len());
        tokio::time::sleep(wait.to_std().unwrap()).await;
    }
}

async fn run_job(mut job: Job) -> Job {
    println!("{} Running {}", chrono::Local::now(), job.name);
    let mut cmd = tokio::process::Command::new(&job.command[0]);
    cmd.args(&job.command[1..]);
    job.lastresult = match cmd.output().await {
        Ok(output) => Some(output),
        Err(e) => { job.error = Some(format!("Failed to run job {}: {}", job.name, e)); return job; }
    };
    let result = job.lastresult.as_ref().unwrap();
    job.path.push("runs");
    job.path.push(job.lastrun.unwrap().format("%Y-%m-%d %H:%M").to_string());
    if tokio::fs::create_dir(&job.path).await.is_err() { return job; }
    let mut filename = job.path.clone();
    filename.push("status");
    match tokio::fs::File::create(&filename).await {
        Ok(mut file) => file.write_all(result.status.code().map_or("unknown\n".to_string(), |c| c.to_string() + "\n").as_bytes()).await.unwrap(),
        Err(e) => { job.error = Some(format!("Failed to write status file: {}", e)); return job; }
    };
    if result.stdout.len() > 0 {
        filename.pop();
        filename.push("out");
        if let Ok(mut file) = tokio::fs::File::create(&filename).await {
            file.write_all(&result.stdout).await.unwrap();
        };
    }
    if result.stderr.len() > 0 {
        filename.pop();
        filename.push("err");
        if let Ok(mut file) = tokio::fs::File::create(&filename).await {
            file.write_all(&result.stderr).await.unwrap();
        };
    }
    job
}
