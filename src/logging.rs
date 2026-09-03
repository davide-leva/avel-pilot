use std::{
    fmt::Display,
    future::Future,
    sync::atomic::{AtomicBool, Ordering},
    time::{Instant, SystemTime, UNIX_EPOCH},
};

static ENABLED: AtomicBool = AtomicBool::new(true);

pub fn set_enabled(enabled: bool) {
    ENABLED.store(enabled, Ordering::Relaxed);
}

pub fn info(message: impl Display) {
    if !ENABLED.load(Ordering::Relaxed) {
        return;
    }
    println!("{} INFO  {message}", timestamp());
}

pub fn error(message: impl Display) {
    if !ENABLED.load(Ordering::Relaxed) {
        return;
    }
    eprintln!("{} ERROR {message}", timestamp());
}

pub fn warn(message: impl Display) {
    if !ENABLED.load(Ordering::Relaxed) {
        return;
    }
    eprintln!("{} WARN  {message}", timestamp());
}

pub fn timed<T, E>(
    operation: impl Into<String>,
    execute: impl FnOnce() -> Result<T, E>,
) -> Result<T, E>
where
    E: Display,
{
    let operation = operation.into();
    let started = Instant::now();
    info(format_args!("START {operation}"));

    match execute() {
        Ok(value) => {
            info(format_args!(
                "OK    {operation} elapsed_ms={}",
                started.elapsed().as_millis()
            ));
            Ok(value)
        }
        Err(error) => {
            self::error(format_args!(
                "FAIL  {operation} elapsed_ms={} error={error}",
                started.elapsed().as_millis()
            ));
            Err(error)
        }
    }
}

pub async fn timed_async<T, E, F>(operation: impl Into<String>, future: F) -> Result<T, E>
where
    E: Display,
    F: Future<Output = Result<T, E>>,
{
    let operation = operation.into();
    let started = Instant::now();
    info(format_args!("START {operation}"));

    match future.await {
        Ok(value) => {
            info(format_args!(
                "OK    {operation} elapsed_ms={}",
                started.elapsed().as_millis()
            ));
            Ok(value)
        }
        Err(error) => {
            self::error(format_args!(
                "FAIL  {operation} elapsed_ms={} error={error}",
                started.elapsed().as_millis()
            ));
            Err(error)
        }
    }
}

fn timestamp() -> String {
    let elapsed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();

    format!(
        "[{}.{}]",
        elapsed.as_secs(),
        format!("{:03}", elapsed.subsec_millis())
    )
}
