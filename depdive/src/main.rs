use anyhow::Result;
use depdive::UpdateAnalyzer;
use structopt::StructOpt;

#[derive(Debug, StructOpt)]
#[structopt(about = "Rust dependency analysis")]
struct Args {
    #[structopt(subcommand)]
    cmd: Command,
}

#[derive(Debug, StructOpt)]
enum Command {
    #[structopt(name = "update-review")]
    // Generate update review from two paths
    UpdateReview { old: String, new: String },
}

// Copied from cargo-guppy
fn args() -> impl Iterator<Item = String> {
    let mut args: Vec<String> = ::std::env::args().collect();

    if args.len() >= 2 {
        args.remove(1);
    }

    args.into_iter()
}

fn main() -> Result<()> {
    let args = Args::from_iter(args());

    match args.cmd {
        Command::UpdateReview { old, new } => UpdateAnalyzer::cmd_update_review(&old, &new),
    }
}
