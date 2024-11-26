use std::sync;
use crate::{ Config, Schedule };

pub async fn run(config: sync::Arc<sync::RwLock<Config>>) {
    let mut nextrun: Vec<String> = vec![];
    loop {
        for path in &nextrun {
            let path = path.clone();
            tokio::spawn(async move { run_job(path).await });
        }
        tokio::task::yield_now().await;
        nextrun.clear();

        let mut nextloop = chrono::Local::now() + chrono::TimeDelta::new(60, 0).unwrap();
        {
            let rconfig = config.read().unwrap();
            for job in &rconfig.jobs {
                if let Schedule::Schedule(sched) = &job.schedule {
                    let nextjob = sched.upcoming(chrono::Local).next().unwrap();
                    if nextjob > nextloop { continue; }
                    if nextjob < nextloop {
                        nextrun.clear();
                        nextloop = nextjob;
                    }
                    nextrun.push(job.path.display().to_string());
                }
            }
        }
        let wait = nextloop - chrono::Local::now();
        println!("Next loop in {} seconds ({} jobs to run)", wait.num_seconds(), nextrun.len());
        tokio::time::sleep(wait.to_std().unwrap()).await;
    }
}

async fn run_job(path: String) -> u8 {
    println!("{} Running {}", chrono::Local::now(), path);
    return 0;
}
