//! The compositor's window cap, from the client side.
//!
//! Needs a live compositor, which the shared boot does not have — it is in
//! `RUST_SKIP` and `metal_sim_window_caps` runs it on the metal-sim profile,
//! whose config boots the compositor on the firmware framebuffer.
//!
//! Two refusals, both of which used to be impossible to express:
//!
//! - a size no machine can back. `req.width`/`req.height` are unvalidated u32
//!   from the client and `content_w * content_h * 4` reached
//!   `SharedMemory::allocate`, whose `alloc_shared` asserts on a token the
//!   kernel refused. One message from any client and the compositor was gone.
//! - one window past the memory budget. There was no budget.
//!
//! The count is printed rather than asserted against a constant here: the host
//! side compares it with the number the compositor derived and announced at
//! startup, which is the assertion that ties the derivation to the behaviour.
//! A number hardcoded on both sides would agree with itself forever.

use window::{CreateError, Window};

/// Far beyond any cap this machine could derive, so reaching it means the cap
/// is not there rather than that it is large.
const GIVE_UP_AT: usize = 512;

fn main() {
    // The compositor must refuse this and still be running afterwards.
    match Window::create(u32::MAX, u32::MAX) {
        Err(CreateError::TooLarge) => {}
        Err(e) => panic!("a u32::MAX window was refused, but as {e:?}"),
        Ok(_) => panic!("a u32::MAX window was granted"),
    }

    // Small on purpose: the cap counts windows, not pixels, so this asks the
    // question without moving a gigabyte to do it. Kept rather than dropped —
    // closing it would make the count a race against the compositor noticing.
    let probe = Window::create(64, 64)
        .expect("the compositor did not survive refusing an oversized window");

    let mut held: Vec<Window> = vec![probe];
    let refused_at = loop {
        if held.len() >= GIVE_UP_AT {
            panic!("{GIVE_UP_AT} windows granted and still counting — there is no cap");
        }
        match Window::create(64, 64) {
            Ok(w) => held.push(w),
            Err(CreateError::AtCapacity) => break held.len(),
            Err(e) => panic!("window {} failed with {e:?}, not a capacity refusal", held.len()),
        }
    };

    // Non-vacuity: a compositor that refused everything would also "have a
    // cap", and would pass every assertion above.
    assert!(
        refused_at >= 2,
        "only {refused_at} windows were ever granted; the cap is refusing, not bounding"
    );

    println!("window caps: oversized refused, {refused_at} windows granted then refused");
}
