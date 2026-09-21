//! Manual check of the native DDC backend.
//!
//! ```text
//! cargo run -p relay-core --example ddc -- list
//! cargo run -p relay-core --example ddc -- get first
//! cargo run -p relay-core --example ddc -- set first 17
//! cargo run -p relay-core --example ddc -- set 05E39027-0000-0000-2F1D-0103803C2278 17
//! ```
//!
//! `list` prints one line per external display (`name  edid_uuid`); `get`
//! asks the display which input source it is showing and prints the code or
//! `unknown`; `set` writes VCP 0x60 and prints whether the display took it.
//! `get` changes nothing, so it is safe to run at any time. DDC needs no TCC
//! permission, so this runs from any shell.

use relay_core::display::ddc::{list_displays, DdcDisplay};
use relay_core::display::DisplayInput;

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let argv: Vec<&str> = args.iter().map(String::as_str).collect();

    match argv.as_slice() {
        ["list"] => list().await,
        ["get", display] => get(display).await,
        ["set", display, code] => match code.parse::<u8>() {
            Ok(code) => set(display, code).await,
            Err(err) => {
                eprintln!("{code:?} is not an input source code: {err}");
                std::process::exit(2);
            }
        },
        _ => {
            eprintln!("usage: ddc list");
            eprintln!("       ddc get <edid_uuid|first>");
            eprintln!("       ddc set <edid_uuid|first> <input code>");
            std::process::exit(2);
        }
    }
}

async fn list() {
    let displays = list_displays().await;
    if displays.is_empty() {
        println!("no external display found");
        return;
    }
    for display in displays {
        println!("{}  {}", display.name, display.edid_uuid);
    }
}

async fn get(display: &str) {
    let started = std::time::Instant::now();
    match open(display).current_input().await {
        // Nothing here is an error: a display that will not answer simply
        // leaves us without a reading.
        Some(code) => println!("{code}  in {:?}", started.elapsed()),
        None => println!("unknown  in {:?}", started.elapsed()),
    }
}

async fn set(display: &str, code: u8) {
    let started = std::time::Instant::now();
    match open(display).set_input(code).await {
        Ok(()) => println!("ok  input {code} in {:?}", started.elapsed()),
        Err(err) => {
            eprintln!("error  {err}");
            std::process::exit(1);
        }
    }
}

/// The display the command line names; `first` is how it spells "no
/// configured EDID UUID".
fn open(display: &str) -> DdcDisplay {
    let edid_uuid = (display != "first").then(|| display.to_string());
    let name = edid_uuid.clone().unwrap_or_else(|| "first".to_string());
    DdcDisplay::new(edid_uuid, name)
}
