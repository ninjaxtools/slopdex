fn main() {
    if let Err(error) = slopdex::cli::run() {
        slopdex::cli::report_error(&error);
        std::process::exit(1);
    }
}
