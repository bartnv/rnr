use std::{ env, error, path, str::FromStr };
use git_version::git_version;
use tokio::{fs, io::AsyncReadExt as _};
use yaml_rust2::YamlLoader;

const VERSION: &str = git_version!();

#[derive(Debug, Default)]
enum Schedule {
    #[default]
    None,
    Continuous,
    Schedule(cron::Schedule)
}

#[derive(Debug, Default)]
struct Job {
    name: String,
    path: path::PathBuf,
    schedule: Schedule,
    command: String
}
impl Job {
    fn from_yaml(path: path::PathBuf, yaml: String) -> Option<Job> {
        let docs = match YamlLoader::load_from_str(&yaml) {
            Ok(config) => config,
            Err(err) => {
                eprintln!("Invalid YAML in jobfile: {}", err);
                return None;
            }
        };
        let config = &docs[0];
        let schedule = match config["schedule"].as_str() {
            Some(sched) => match sched {
                "@continuous" => Schedule::Continuous,
                sched => match cron::Schedule::from_str(sched) {
                    Ok(sched) => Schedule::Schedule(sched),
                    Err(err) => {
                        eprintln!("Failed to parse schedule expression: {}", err);
                        return None;
                    }
                }
            },
            None => Schedule::None
        };
        Some(Job {
            name: config["name"].as_str().unwrap_or(&path.display().to_string()).trim_start_matches("./").to_string(),
            path,
            schedule,
            command: config["command"].as_str().unwrap_or("").to_string(),
            ..Default::default()
        })
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn error::Error>> {
    let cwd = env::current_dir()?;
    println!("Starting rnr {} in {}", VERSION, cwd.display());
    let mut jobs = vec![];
    let mut dir = fs::read_dir(".").await?;
    while let Ok(Some(entry)) = dir.next_entry().await {
        let path = entry.path();
        if !path.is_dir() { continue; }
        let job = match fs::File::open(&path.join("job.yml")).await {
            Ok(mut file) => {
                let mut contents = vec![];
                file.read_to_end(&mut contents).await?;
                Job::from_yaml(path.clone(), String::from_utf8_lossy(&contents).into_owned())
            }
            Err(_) => {
                println!("Directory {} has no job.yml file", path.display());
                continue;
            }
        };
        if let Some(job) = job {
            println!("Found job \"{}\" to run: {}", job.name, job.command);
            if let Schedule::Schedule(ref sched) = job.schedule { println!("Next execution: {}", sched.upcoming(chrono::Local).next().unwrap()); }
            jobs.push(job);
        }
    }
    Ok(())
}
