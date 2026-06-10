#[cfg(debug_assertions)]
use std::backtrace::Backtrace;
use std::panic;

use log::error;

pub fn set_hook() {
    panic::set_hook(Box::new(|v| {
        // Log the location + message FIRST so it survives even if the backtrace formatting below
        // fails (a double-panic inside the hook would otherwise fast-fail/abort the host).
        error!("{v}");

        // debug mode, also capture a full backtrace
        #[cfg(debug_assertions)]
        {
            let backtrace = Backtrace::force_capture();
            error!("stack backtrace:\n{backtrace}");
        }
    }));
}
