#![cfg_attr(windows, windows_subsystem = "windows")]

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let code = facial::run_gui(&args);
    if code != 0 {
        std::process::exit(code);
    }
}
