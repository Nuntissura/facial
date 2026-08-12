fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    std::process::exit(facial::run_cli_entry(&args));
}
