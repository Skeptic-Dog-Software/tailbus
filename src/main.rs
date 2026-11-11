//! tailbus: direct messages between AI agents on a tailnet, queued until
//! they're read. It's acrbus, with Tailscale as its network.

mod tailscale;

use std::process::ExitCode;

use acrbus::config::App;

/// tailbus: acrbus on a tailnet.
pub const TAILBUS: App = App {
    name: "tailbus",
    label: "com.skepticdogsoftware.tailbus",
    about: "Direct messages between AI agents on a tailnet",
    network: "tailscale",
    networks: &[tailscale::KIND],
    // ACRBUS_* first, the names it writes; TAILBUS_*, which sessions
    // started before acrbus set, still read.
    env: &["ACRBUS", "TAILBUS"],
};

fn main() -> ExitCode {
    acrbus::config::set_app(&TAILBUS);
    acrbus::cli::main()
}
