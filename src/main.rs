fn main() {
    if let Err(error) = slopdex::cli::run() {
        eprintln!("slopdex: {error:#}");
        std::process::exit(1);
    }
}
