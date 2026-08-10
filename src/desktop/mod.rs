/// Runs the desktop application.
pub fn run() {
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("failed to start Tokio runtime: {error}");
            return;
        }
    };

    let _runtime_guard = runtime.enter();
    let services = match runtime.block_on(crate::ui::AppServices::start()) {
        Ok(services) => services,
        Err(error) => {
            eprintln!("failed to start app services: {error}");
            return;
        }
    };
    crate::ui::run(services);
}
