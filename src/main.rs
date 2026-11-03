fn main() {
    if let Err(error) = slopdex::run() {
        slopdex::report_error(&error);
        std::process::exit(1);
    }
}
