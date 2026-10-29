use std::process::ExitCode;
use std::sync::Arc;

use git_api::api::router;
use git_api::process::RepositoryService;
use git_api::security::validate_security_configuration;

const TCP_ADDRESS: (&str, u16) = ("0.0.0.0", 8081);
#[cfg(unix)]
const SOCKET_PATH: &str = "/tmp/haskell-ipc.sock";

/// Removes variables that could make Git prompt and disables prompting. Runs
/// before any thread is started.
fn clean_environment() {
    std::env::remove_var("GIT_ASKPASS");
    std::env::remove_var("GCM_INTERACTIVE");
    std::env::set_var("GIT_TERMINAL_PROMPT", "0");
    std::env::set_var("SSH_AUTH_SOCK", r"\.\pipe\ssh-auth-sock");
}

async fn run(service: Arc<RepositoryService>) -> std::io::Result<()> {
    let app = router(service);
    let tcp = tokio::net::TcpListener::bind(TCP_ADDRESS).await?;

    #[cfg(unix)]
    {
        let unix = tokio::net::UnixListener::bind(SOCKET_PATH)?;
        tokio::try_join!(axum::serve(tcp, app.clone()), axum::serve(unix, app))?;
        Ok(())
    }
    #[cfg(not(unix))]
    {
        axum::serve(tcp, app).await
    }
}

fn main() -> ExitCode {
    if let Err(error) = validate_security_configuration() {
        eprintln!("git-api: user error ({error})");
        return ExitCode::FAILURE;
    }

    let service = Arc::new(RepositoryService::new());
    println!(
        "number of cores available for parsingPool: {}",
        service.parsing_workers()
    );
    println!(
        "number of cores available for commandPool: {}",
        service.command_workers()
    );
    clean_environment();

    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("git-api: {error}");
            return ExitCode::FAILURE;
        }
    };
    match runtime.block_on(run(service)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("git-api: {error}");
            ExitCode::FAILURE
        }
    }
}
