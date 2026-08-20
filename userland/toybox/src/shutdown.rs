//! Power the machine off.
//!
//! **The endowment is the whole of the authority.** `/bin/shutdown` is
//! `/bin/toybox` under another name, so what this holds is what the image's
//! `[programs.toybox]` row declares — a config that does not name `power`
//! there builds an image whose shutdown applet says it cannot and changes
//! nothing else. Nothing here asks for the capability: it is either in the
//! endowment table `/bin/init` filled at spawn or it does not exist for this
//! process.

use toyos::endow::{Endowments, SYSCAP_LABEL};
use toyos::syscap::SysCap;

pub fn main(_args: Vec<String>) {
    let Some(cap) = Endowments::get().take::<SysCap>(SYSCAP_LABEL) else {
        eprintln!("shutdown: this program was endowed no system capability");
        std::process::exit(1);
    };
    // Comes back only refused: on the other path the power is already off.
    let refused = cap.shutdown();
    eprintln!("shutdown: refused ({refused:?}) — this capability carries no POWER");
    std::process::exit(1);
}
