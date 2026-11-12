//! Isolated text extractor, started by whereti with limits (see `worker`).
#![windows_subsystem = "windows"]

fn main() {
    // a panic stops the program at once, except in a guarded parser
    whereti_guard::stop_on_panic();
    std::process::exit(whereti_extract::worker::serve_stdio());
}
