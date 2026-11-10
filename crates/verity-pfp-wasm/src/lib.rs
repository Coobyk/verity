//! WASM ABI for the BallsMaker renderer — plain extern "C" fns, no
//! wasm-bindgen. JS calls `pfp_render`, reads `pfp_len` bytes from the
//! returned pointer, and the buffer stays valid until the next call.

use std::cell::RefCell;

thread_local! {
    static OUT: RefCell<Vec<u8>> = const { RefCell::new(Vec::new()) };
}

/// Render an avatar. `hue < 0` = the site's white bg; `pupils != 0`
/// draws pupils + glint. Returns a pointer into wasm memory.
#[no_mangle]
pub extern "C" fn pfp_render(seed: u64, size: u32, hue: f64, pupils: u32) -> *mut u8 {
    let png = verity_pfp::render_opts(
        seed,
        size.max(8).min(1024),
        if hue < 0.0 { None } else { Some(hue) },
        pupils != 0,
    )
    .unwrap_or_default();
    OUT.with(|o| {
        let mut o = o.borrow_mut();
        *o = png;
        o.as_mut_ptr()
    })
}

/// Length of the last rendered PNG in bytes.
#[no_mangle]
pub extern "C" fn pfp_len() -> usize {
    OUT.with(|o| o.borrow().len())
}

/// `name_seed` — FNV-1a of the bot name, same face the bot gets.
#[no_mangle]
pub extern "C" fn pfp_name_seed(s: *const u8, n: usize) -> u64 {
    let bytes = unsafe { std::slice::from_raw_parts(s, n) };
    let name = String::from_utf8_lossy(bytes);
    verity_pfp::name_seed(&name)
}
