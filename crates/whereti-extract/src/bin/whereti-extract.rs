//! Isolated text extractor, started by whereti with limits (see `worker`).
#![windows_subsystem = "windows"]

fn main() {
    std::process::exit(whereti_extract::worker::serve_stdio());
}
