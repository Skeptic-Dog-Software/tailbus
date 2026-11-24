//! tailbus: direct messages between AI agents on a tailnet, queued until
//! they're read, with Tailscale as its network.

mod tailscale;

use std::process::ExitCode;

use tailbus::config::App;

/// tailbus: the bus on a tailnet.
pub const TAILBUS: App = App {
    name: "tailbus",
    label: "com.skepticdogsoftware.tailbus",
    about: "Direct messages between AI agents on a tailnet",
    network: "tailscale",
    networks: &[tailscale::KIND],
    // TAILBUS_* first, the names it writes; ACRBUS_*, which sessions
    // started while tailbus was built on acrbus have, still read.
    env: &["TAILBUS", "ACRBUS"],
};

fn main() -> ExitCode {
    tailbus::config::set_app(&TAILBUS);
    tailbus::cli::main()
}
