// Relay: menu bar app that hands the shared monitor and Logitech keyboard/mouse
// over to another Mac. Without arguments this binary is the resident app; with
// a subcommand it is a short-lived CLI that talks to the resident one over a
// unix socket (spec §7).

mod app;
mod cli;
mod ipc;

fn main() {
    cli::dispatch();
}
