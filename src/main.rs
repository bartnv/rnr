use std::error::Error;
use git_version::git_version;

const VERSION: &str = git_version!(args = ["--always", "--dirty=-modified", "--tags"]);

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    println!("Starting rnr {} in {}", VERSION, std::env::current_dir().unwrap_or_default().display());
    Ok(())
}
