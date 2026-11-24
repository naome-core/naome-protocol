use anyhow::{Result, ensure};
use clap::Parser;
use naome_protocol::{
    cli::{self, ArchiveCommand},
    wire::Format,
};
use std::path::PathBuf;

#[derive(Parser)]
#[command(
    name = "nao",
    bin_name = "nao",
    version,
    about = "Store, exchange and evaluate immutable research evidence"
)]
struct Cli {
    /// Local archive path. Defaults to the absolute NAO_ARCHIVE path, or evidence.sqlite in this directory.
    #[arg(long, global = true)]
    db: Option<PathBuf>,
    #[arg(long, global = true, value_enum, default_value = "a")]
    format: Format,
    #[command(subcommand)]
    command: ArchiveCommand,
}
fn main() -> Result<()> {
    let args = Cli::parse();
    let db = match args.db {
        Some(path) => path,
        None => match std::env::var_os("NAO_ARCHIVE") {
            Some(path) => {
                let path = PathBuf::from(path);
                ensure!(
                    path.is_absolute(),
                    "NAO_ARCHIVE must be an absolute archive path"
                );
                path
            }
            None => PathBuf::from("evidence.sqlite"),
        },
    };
    cli::run(args.command, db, args.format)
}
