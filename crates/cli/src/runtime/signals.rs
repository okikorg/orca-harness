/// Resolves when the process receives SIGTERM or SIGHUP (terminal window
/// closed). Children now live in their own process groups, so the CLI
/// must exit its run loop cleanly for the Drop-time group kills to fire.
#[cfg(unix)]
pub(crate) async fn shutdown_signal() {
    use tokio::signal::unix::{signal, SignalKind};
    let (Ok(mut term), Ok(mut hup)) = (
        signal(SignalKind::terminate()),
        signal(SignalKind::hangup()),
    ) else {
        return std::future::pending().await;
    };
    tokio::select! {
        _ = term.recv() => {}
        _ = hup.recv() => {}
    }
}

#[cfg(not(unix))]
pub(crate) async fn shutdown_signal() {
    std::future::pending::<()>().await
}
