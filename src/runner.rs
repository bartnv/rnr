use std::{ process::Stdio, sync, time };
use tokio::{ io::AsyncWriteExt as _, sync::{ broadcast, mpsc } };

use crate::{ control, web, Config, Job, Run, Schedule };

pub async fn run(config: sync::Arc<sync::RwLock<Config>>, broadcast: broadcast::Sender<Job>) {
    let mut nextjobs: Vec<Box<Job>> = vec![];
    let (ctrltx, ctrlrx) = mpsc::channel(100);
    let (spawntx, mut spawnrx) = mpsc::channel(100);
    let aconfig = config.clone();
    let abroadcast = broadcast.clone();
    let aspawntx = spawntx.clone();
    tokio::spawn(async move {
        control::run(aconfig.clone(), ctrlrx, aspawntx, abroadcast).await;
    });
    if config.read().unwrap().http.is_some() {
        let config = config.clone();
        let broadcast = broadcast.clone();
        let spawntx = spawntx.clone();
        tokio::spawn(async move {
            web::run(config, broadcast, spawntx).await;
        });
    }
    let aconfig = config.clone();
    tokio::spawn(async move {
        while let Some(mut job) = spawnrx.recv().await {
            let ctrltx = ctrltx.clone();
            let config = aconfig.clone();
            tokio::spawn(async move {
                let mut skip = false;
                if let Some(cjob) = config.write().unwrap().jobs.get_mut(&job.path.display().to_string()) {
                    if cjob.running {
                        skip = true;
                        cjob.skipped += 1;
                        job.skipped = cjob.skipped;
                        eprintln!("Job \"{}\" skipped because it is already running", cjob.path.display());
                    }
                    else {
                        cjob.skipped = 0;
                        cjob.running = true;
                        cjob.laststart = job.laststart;
                    }
                }
                if skip {
                    if let Err(e) = ctrltx.send(job).await {
                        eprintln!("Send error: {}", e);
                    }
                    return;
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
                    let nextrun = match sched.upcoming(chrono::Local).next() {
                        Some(n) => n,
                        None => continue
                    };
                    if nextrun > nextloop { continue; }
                    if nextrun < nextloop {
                        nextjobs.clear();
                        nextloop = nextrun;
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
    let executable = match job.command[0].contains("/") {
        true => match tokio::fs::canonicalize(&job.command[0]).await {
            Ok(path) => path,
            Err(e) => {
                job.error = Some(format!("Failed to locate executable {}: {}", &job.command[0], e));
                return job;
            }
        },
        false => std::path::PathBuf::from(&job.command[0])
    };
    println!("{} [{}] starting {}", chrono::Local::now().format("%Y-%m-%d %H:%M:%S"), job.path.display(), executable.display());
    let mut cmd = tokio::process::Command::new(executable);
    cmd.args(&job.command[1..]);
    if let Ok(config) = config.read() {
        cmd.envs(&config.env);
    }
    if let Some(ref dir) = job.workdir {
        cmd.current_dir(dir);
    }
    cmd.stdin(Stdio::piped())
       .stdout(Stdio::piped())
       .stderr(Stdio::piped());
    let start = time::Instant::now();
    let mut proc = match cmd.spawn() {
        Ok(child) => child,
        Err(e) => { job.error = Some(format!("Failed to start: {e}")); return job; }
    };
    if let Some(input) = job.input.take() {
        if let Some(mut stdin) = proc.stdin.take() {
            let path = job.path.display().to_string();
            tokio::spawn(async move {
                if let Err(e) = stdin.write_all(input.as_bytes()).await {
                    eprintln!("Failed to write to stdin for job {path}");
                }
            });
        }
        else { eprintln!("Failed to open stdin for job {}", job.path.display()); }
    }
    let output = match proc.wait_with_output().await {
        Ok(output) => output,
        Err(e) => { job.error = Some(format!("Failed to read job stdout: {e}")); return job; }
    };
    let duration = start.elapsed();
    job.lastrun = Some(Run { start: job.laststart.unwrap(), duration: duration.clone(), output });
    let output = &job.lastrun.as_ref().unwrap().output;
    let mut filename = config.read().unwrap().dir.clone();
    filename.push(job.path.clone());
    filename.push("runs");
    filename.push(job.laststart.unwrap().format("%Y-%m-%d %H:%M").to_string());
    if tokio::fs::create_dir(&filename).await.is_err() { return job; }
    job.history = true;
    filename.push("status");
    match tokio::fs::File::create(&filename).await {
        Ok(mut file) => if let Err(e) = file.write_all(output.status.code().map_or("unknown\n".to_string(), |c| c.to_string() + "\n").as_bytes()).await {
            eprintln!("Job \"{}\" failed to write status file: {}", job.path.display(), e);
            return job;
        },
        Err(e) => {
            eprintln!("Job \"{}\" failed to create status file: {}", job.path.display(), e);
            return job;
        }
    };
    filename.pop();
    filename.push("dur");
    if let Ok(mut file) = tokio::fs::File::create(&filename).await {
        let _ = file.write_all(format!("{}", duration.as_secs()).as_bytes()).await;
    }
    if !output.stdout.is_empty() {
        filename.pop();
        filename.push("out");
        if let Ok(mut file) = tokio::fs::File::create(&filename).await {
            let _ = file.write_all(&output.stdout).await;
        };
    }
    if !output.stderr.is_empty() {
        filename.pop();
        filename.push("err");
        if let Ok(mut file) = tokio::fs::File::create(&filename).await {
            let _ = file.write_all(&output.stderr).await;
        };
    }
    job
}
