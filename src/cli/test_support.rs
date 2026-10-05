use super::args::Cli;
use clap::Parser;

pub(super) fn parse(args: &[&str]) -> Cli {
    let cli = Cli::try_parse_from(std::iter::once("slopdex").chain(args.iter().copied())).unwrap();
    cli.validate().unwrap();
    cli
}
