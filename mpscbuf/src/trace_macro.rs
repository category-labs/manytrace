#[cfg(feature = "trace")]
#[macro_export]
macro_rules! mpsc_trace {
    ($($arg:tt)*) => {
        ::tracing::trace!($($arg)*)
    };
}

#[cfg(not(feature = "trace"))]
#[macro_export]
macro_rules! mpsc_trace {
    ($($arg:tt)*) => {};
}
