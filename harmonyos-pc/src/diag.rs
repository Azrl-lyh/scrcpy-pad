#[macro_export]
macro_rules! diag_debug {
    ($target:expr, $($arg:tt)*) => {{
        let _ = format!($($arg)*);
    }};
}

#[macro_export]
macro_rules! diag_info {
    ($target:expr, $($arg:tt)*) => {{
        eprintln!($($arg)*);
    }};
}

#[macro_export]
macro_rules! diag_warn {
    ($target:expr, $($arg:tt)*) => {{
        eprintln!($($arg)*);
    }};
}

#[macro_export]
macro_rules! diag_error {
    ($target:expr, $($arg:tt)*) => {{
        eprintln!($($arg)*);
    }};
}
